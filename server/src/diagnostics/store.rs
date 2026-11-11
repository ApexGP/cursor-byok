//! Persists, aggregates, and dismisses diagnostic reports.
//!
//! Nothing in this module may block the caller on disk I/O: [`DiagnosticsHub::record`]
//! only updates the in-memory aggregate and enqueues a snapshot for the background
//! writer task. Writer failures are logged and never recurse into diagnostics.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, error::TrySendError};
#[cfg(test)]
use tokio::sync::oneshot;

use crate::{
    diagnostics::{
        code::Severity,
        event::{DiagnosticContext, DiagnosticEvent},
        redact::Redactor,
    },
    store::Store,
    Error, Result,
};

/// A fingerprint seen again inside this window aggregates into the same report.
const AGGREGATION_WINDOW_MS: i64 = 10 * 60 * 1000;
/// How long a dismissed report stays hidden unless its severity escalates.
const DISMISS_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const EVENTS_FILE: &str = "events.jsonl";
const DISMISSED_FILE: &str = "dismissed.json";
const EVENTS_FILE_LIMIT_BYTES: usize = 5 * 1024 * 1024;
const WRITER_QUEUE: usize = 256;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Configuration for the diagnostics subsystem.
#[derive(Debug, Clone)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    pub upload_url: Option<String>,
    pub github_repository: String,
    pub retention_days: u32,
    pub max_events: usize,
    pub log_window: Duration,
    pub data_dir: PathBuf,
}

/// A deduplicated report exposed to the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportSummary {
    pub report_id: String,
    pub code: String,
    pub severity: Severity,
    pub summary: String,
    pub occurrences: u32,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub dismissed: bool,
}

struct ReportState {
    event: DiagnosticEvent,
    fingerprint: String,
    occurrences: u32,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
}

struct State {
    reports: BTreeMap<String, ReportState>,
    latest_by_fingerprint: HashMap<String, String>,
    dismissed: HashMap<String, DismissedEntry>,
}

impl State {
    fn from_loaded(
        persisted: &BTreeMap<String, PersistedEvent>,
        dismissed: HashMap<String, DismissedEntry>,
        now: DateTime<Utc>,
        config: &DiagnosticsConfig,
    ) -> Self {
        let reports: BTreeMap<String, ReportState> = persisted
            .iter()
            .map(|(report_id, record)| {
                (
                    report_id.clone(),
                    ReportState {
                        event: record.event.clone(),
                        fingerprint: record.event.fingerprint.clone(),
                        occurrences: record.occurrences,
                        first_seen: record.first_seen,
                        last_seen: record.last_seen,
                    },
                )
            })
            .collect();

        let mut latest_by_fingerprint = HashMap::new();
        for (report_id, report) in &reports {
            let replace = match latest_by_fingerprint
                .get(&report.fingerprint)
                .and_then(|existing: &String| reports.get(existing))
            {
                Some(existing) => report.last_seen > existing.last_seen,
                None => true,
            };
            if replace {
                latest_by_fingerprint.insert(report.fingerprint.clone(), report_id.clone());
            }
        }

        let mut state = Self {
            reports,
            latest_by_fingerprint,
            dismissed,
        };
        prune_state(&mut state, now, config);
        state
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedEvent {
    event: DiagnosticEvent,
    occurrences: u32,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissedEntry {
    dismissed_at: DateTime<Utc>,
    severity: Severity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissedFile {
    #[serde(default = "dismissed_schema_version")]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, DismissedEntry>,
}

impl Default for DismissedFile {
    fn default() -> Self {
        Self {
            version: dismissed_schema_version(),
            entries: HashMap::new(),
        }
    }
}

fn dismissed_schema_version() -> u32 {
    1
}

enum Command {
    Write(Box<PersistedEvent>),
    Dismiss {
        fingerprint: String,
        at: DateTime<Utc>,
        severity: Severity,
    },
    Compact,
    #[cfg(test)]
    Flush(oneshot::Sender<()>),
}

struct Inner {
    config: DiagnosticsConfig,
    database_store: Store,
    state: Mutex<State>,
    writer: mpsc::Sender<Command>,
}

/// Owns the in-memory report index and the background persistence writer.
pub struct DiagnosticsHub {
    inner: Arc<Inner>,
}

impl DiagnosticsHub {
    /// Loads existing persisted reports, compacts them, and starts the writer.
    pub async fn new(config: DiagnosticsConfig, store: Store) -> Result<Arc<Self>> {
        ensure_data_dir(&config.data_dir)?;

        let now = Utc::now();
        let mut persisted = load_events(&config.data_dir).await;
        prune_persisted(&mut persisted, now, &config);
        let redactor = Redactor::new();
        for record in persisted.values_mut() {
            record.event = redact_event(&redactor, record.event.clone());
        }
        let mut dismissed = load_dismissed(&config.data_dir).await;
        prune_dismissed(&mut dismissed, now);

        // Compaction failures are non-fatal: the hub can still serve reports from
        // memory and the writer retries on later updates.
        if let Err(error) = write_events_file(&config.data_dir, &persisted).await {
            tracing::warn!(%error, "failed to compact diagnostics events at startup");
        }
        if let Err(error) = write_dismissed_file(&config.data_dir, &dismissed).await {
            tracing::warn!(%error, "failed to compact diagnostic dismissals at startup");
        }

        let state = State::from_loaded(&persisted, dismissed.clone(), now, &config);
        let (writer, receiver) = mpsc::channel(WRITER_QUEUE);
        let inner = Arc::new(Inner {
            config: config.clone(),
            database_store: store,
            state: Mutex::new(state),
            writer,
        });

        let writer_dir = config.data_dir.clone();
        let writer_config = config.clone();
        tokio::spawn(async move {
            writer_loop(receiver, writer_dir, writer_config, persisted, dismissed).await;
        });

        let cleanup = Arc::downgrade(&inner);
        tokio::spawn(async move { cleanup_loop(cleanup).await });

        Ok(Arc::new(Self { inner }))
    }

    /// Records an event without performing any disk I/O.
    pub fn record(&self, event: DiagnosticEvent) {
        if !self.inner.config.enabled {
            return;
        }
        self.record_at(event, Utc::now());
    }

    fn record_at(&self, mut event: DiagnosticEvent, now: DateTime<Utc>) {
        event.captured_at = now;
        let event = redact_event(&Redactor::new(), event);
        let snapshot = {
            let mut state = self.inner.state.lock();
            apply_event(&mut state, &self.inner.config, event, now)
        };
        self.enqueue(Command::Write(Box::new(snapshot)));
    }

    /// Returns visible reports, newest first. Dismissed reports stay hidden for
    /// 24 hours unless their severity has escalated.
    pub async fn reports(&self) -> Vec<ReportSummary> {
        self.reports_at(Utc::now()).await
    }

    async fn reports_at(&self, now: DateTime<Utc>) -> Vec<ReportSummary> {
        let mut state = self.inner.state.lock();
        prune_state(&mut state, now, &self.inner.config);

        let mut reports = Vec::with_capacity(state.reports.len());
        for report in state.reports.values() {
            let hidden = state
                .dismissed
                .get(&report.fingerprint)
                .map(|entry| severity_rank(report.event.severity) <= severity_rank(entry.severity))
                .unwrap_or(false);
            if hidden {
                continue;
            }
            reports.push(ReportSummary {
                report_id: report.event.report_id.clone(),
                code: report.event.code.as_str().to_string(),
                severity: report.event.severity,
                summary: report.event.summary.clone(),
                occurrences: report.occurrences,
                first_seen: report.first_seen,
                last_seen: report.last_seen,
                dismissed: false,
            });
        }
        drop(state);

        reports.sort_by(|left, right| {
            right
                .last_seen
                .cmp(&left.last_seen)
                .then_with(|| left.report_id.cmp(&right.report_id))
        });
        reports
    }

    /// Returns the representative event for a report.
    pub async fn event(&self, report_id: &str) -> Option<DiagnosticEvent> {
        self.inner
            .state
            .lock()
            .reports
            .get(report_id)
            .map(|report| report.event.clone())
    }

    /// Hides a report for 24 hours (or until its severity escalates).
    pub async fn dismiss(&self, report_id: &str) -> Result<()> {
        self.dismiss_at(report_id, Utc::now()).await
    }

    async fn dismiss_at(&self, report_id: &str, now: DateTime<Utc>) -> Result<()> {
        let (fingerprint, severity) = {
            let mut state = self.inner.state.lock();
            let report = state
                .reports
                .get(report_id)
                .ok_or_else(|| Error::Store(format!("diagnostic report not found: {report_id}")))?;
            let fingerprint = report.fingerprint.clone();
            let severity = report.event.severity;
            state.dismissed.insert(
                fingerprint.clone(),
                DismissedEntry {
                    dismissed_at: now,
                    severity,
                },
            );
            prune_dismissed(&mut state.dismissed, now);
            (fingerprint, severity)
        };

        if let Err(error) = self
            .inner
            .writer
            .send(Command::Dismiss {
                fingerprint,
                at: now,
                severity,
            })
            .await
        {
            tracing::warn!(%error, "diagnostics writer stopped; dismissal not persisted");
        }
        Ok(())
    }

    /// Installs this hub as the process-wide recording target.
    pub fn install_global(hub: Arc<Self>) {
        if GLOBAL.set(hub).is_err() {
            tracing::debug!("diagnostics hub is already installed");
        }
    }

    /// Used by report assembly to collect database evidence.
    #[allow(dead_code)]
    pub(crate) fn database_store(&self) -> &Store {
        &self.inner.database_store
    }

    fn enqueue(&self, command: Command) {
        match self.inner.writer.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("diagnostics writer queue is full; dropping update");
            }
            Err(TrySendError::Closed(_)) => {}
        }
    }

    #[cfg(test)]
    async fn flush(&self) {
        let (ack, receiver) = oneshot::channel();
        if self.inner.writer.send(Command::Flush(ack)).await.is_ok() {
            let _ = receiver.await;
        }
    }
}

static GLOBAL: OnceLock<Arc<DiagnosticsHub>> = OnceLock::new();

/// Returns the globally installed hub, if any.
pub fn global() -> Option<&'static Arc<DiagnosticsHub>> {
    GLOBAL.get()
}

/// Records an event through the global hub; a no-op when diagnostics is off.
pub fn record(event: DiagnosticEvent) {
    if let Some(hub) = global() {
        hub.record(event);
    }
}

fn apply_event(
    state: &mut State,
    config: &DiagnosticsConfig,
    event: DiagnosticEvent,
    now: DateTime<Utc>,
) -> PersistedEvent {
    let fingerprint = event.fingerprint.clone();
    let aggregate_id = state
        .latest_by_fingerprint
        .get(&fingerprint)
        .and_then(|report_id| state.reports.get(report_id))
        .filter(|report| within_window(now, report.last_seen, AGGREGATION_WINDOW_MS))
        .map(|report| report.event.report_id.clone());

    let report_id = match aggregate_id {
        Some(report_id) => {
            let report = state
                .reports
                .get_mut(&report_id)
                .expect("aggregate id points at an existing report");
            report.occurrences = report.occurrences.saturating_add(1);
            report.last_seen = later(report.last_seen, now);
            if severity_rank(event.severity) > severity_rank(report.event.severity) {
                report.event.severity = event.severity;
            }
            report_id
        }
        None => {
            let report_id = event.report_id.clone();
            state.reports.insert(
                report_id.clone(),
                ReportState {
                    event,
                    fingerprint: fingerprint.clone(),
                    occurrences: 1,
                    first_seen: now,
                    last_seen: now,
                },
            );
            report_id
        }
    };
    state
        .latest_by_fingerprint
        .insert(fingerprint, report_id.clone());

    let snapshot = state
        .reports
        .get(&report_id)
        .map(|report| PersistedEvent {
            event: report.event.clone(),
            occurrences: report.occurrences,
            first_seen: report.first_seen,
            last_seen: report.last_seen,
        })
        .expect("report exists before pruning");

    prune_state(state, now, config);
    snapshot
}

fn prune_state(state: &mut State, now: DateTime<Utc>, config: &DiagnosticsConfig) {
    let retention_ms = i64::from(config.retention_days) * 24 * 60 * 60 * 1000;
    let cutoff = now.timestamp_millis() - retention_ms;
    state
        .reports
        .retain(|_, report| report.last_seen.timestamp_millis() >= cutoff);

    if state.reports.len() > config.max_events {
        let excess = state.reports.len() - config.max_events;
        let mut oldest: Vec<(String, i64)> = state
            .reports
            .iter()
            .map(|(report_id, report)| (report_id.clone(), report.last_seen.timestamp_millis()))
            .collect();
        oldest.sort_by_key(|(_, last_seen)| *last_seen);
        for (report_id, _) in oldest.into_iter().take(excess) {
            state.reports.remove(&report_id);
        }
    }

    state
        .latest_by_fingerprint
        .retain(|_, report_id| state.reports.contains_key(report_id));
    prune_dismissed(&mut state.dismissed, now);
}

fn prune_persisted(
    records: &mut BTreeMap<String, PersistedEvent>,
    now: DateTime<Utc>,
    config: &DiagnosticsConfig,
) {
    let retention_ms = i64::from(config.retention_days) * 24 * 60 * 60 * 1000;
    let cutoff = now.timestamp_millis() - retention_ms;
    records.retain(|_, record| record.last_seen.timestamp_millis() >= cutoff);

    while records.len() > config.max_events {
        remove_oldest(records);
    }
    while !records.is_empty() && serialized_events_len(records) > EVENTS_FILE_LIMIT_BYTES {
        remove_oldest(records);
    }
}

fn remove_oldest(records: &mut BTreeMap<String, PersistedEvent>) {
    if let Some(report_id) = records
        .iter()
        .min_by_key(|(_, record)| record.last_seen.timestamp_millis())
        .map(|(report_id, _)| report_id.clone())
    {
        records.remove(&report_id);
    }
}

fn serialized_events_len(records: &BTreeMap<String, PersistedEvent>) -> usize {
    records
        .values()
        .map(|record| serde_json::to_vec(record).map_or(0, |line| line.len() + 1))
        .sum()
}

fn prune_dismissed(dismissed: &mut HashMap<String, DismissedEntry>, now: DateTime<Utc>) {
    dismissed.retain(|_, entry| within_window(now, entry.dismissed_at, DISMISS_WINDOW_MS));
}

fn within_window(now: DateTime<Utc>, then: DateTime<Utc>, window_ms: i64) -> bool {
    now.timestamp_millis()
        .saturating_sub(then.timestamp_millis())
        <= window_ms
}

fn later(left: DateTime<Utc>, right: DateTime<Utc>) -> DateTime<Utc> {
    if left > right {
        left
    } else {
        right
    }
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Warning => 0,
        Severity::Error => 1,
        Severity::Fatal => 2,
    }
}

fn redact_event(redactor: &Redactor, mut event: DiagnosticEvent) -> DiagnosticEvent {
    event.summary = redactor.redact(&event.summary);
    if let Some(value) = event.error_kind.take() {
        event.error_kind = Some(redactor.redact(&value));
    }
    if let Some(value) = event.error_message.take() {
        event.error_message = Some(redactor.redact(&value));
    }
    event.error_chain = event
        .error_chain
        .iter()
        .map(|line| redactor.redact(line))
        .collect();
    event.context = redact_context(redactor, event.context);
    event
}

fn redact_context(redactor: &Redactor, mut context: DiagnosticContext) -> DiagnosticContext {
    context.area = redactor.redact(&context.area);
    context.operation = redactor.redact(&context.operation);
    context.run_id = context.run_id.map(|value| redactor.redact(&value));
    context.request_id = context.request_id.map(|value| redactor.redact(&value));
    context.conversation_id = context.conversation_id.map(|value| redactor.redact(&value));
    context.model_hash = context.model_hash.map(|value| redactor.redact(&value));
    context.provider_type = context.provider_type.map(|value| redactor.redact(&value));
    context
}

fn ensure_data_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

async fn load_events(dir: &Path) -> BTreeMap<String, PersistedEvent> {
    let path = dir.join(EVENTS_FILE);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            tracing::warn!(%error, "failed to read diagnostics events");
            return BTreeMap::new();
        }
    };

    let mut records = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PersistedEvent>(line) {
            Ok(record) if !record.event.report_id.is_empty() => {
                records.insert(record.event.report_id.clone(), record);
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "skipping malformed diagnostics event"),
        }
    }
    records
}

async fn load_dismissed(dir: &Path) -> HashMap<String, DismissedEntry> {
    let path = dir.join(DISMISSED_FILE);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            tracing::warn!(%error, "failed to read diagnostic dismissals");
            return HashMap::new();
        }
    };
    match serde_json::from_str::<DismissedFile>(&text) {
        Ok(file) => file.entries,
        Err(error) => {
            tracing::warn!(%error, "ignoring malformed diagnostic dismissals");
            HashMap::new()
        }
    }
}

async fn write_events_file(dir: &Path, records: &BTreeMap<String, PersistedEvent>) -> Result<()> {
    let mut bytes = Vec::new();
    for record in records.values() {
        match serde_json::to_vec(record) {
            Ok(line) => {
                bytes.extend_from_slice(&line);
                bytes.push(b'\n');
            }
            Err(error) => tracing::warn!(%error, "failed to serialize diagnostic event"),
        }
    }
    write_atomic(&dir.join(EVENTS_FILE), &bytes).await?;
    Ok(())
}

async fn write_dismissed_file(
    dir: &Path,
    dismissed: &HashMap<String, DismissedEntry>,
) -> Result<()> {
    let file = DismissedFile {
        version: dismissed_schema_version(),
        entries: dismissed.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file)?;
    write_atomic(&dir.join(DISMISSED_FILE), &bytes).await?;
    Ok(())
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    tokio::fs::write(&temporary, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    }
    #[cfg(windows)]
    {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    tokio::fs::rename(&temporary, path).await
}

async fn writer_loop(
    mut receiver: mpsc::Receiver<Command>,
    dir: PathBuf,
    config: DiagnosticsConfig,
    mut records: BTreeMap<String, PersistedEvent>,
    mut dismissed: HashMap<String, DismissedEntry>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            Command::Write(record) => {
                let record = *record;
                records.insert(record.event.report_id.clone(), record);
                prune_persisted(&mut records, Utc::now(), &config);
                if let Err(error) = write_events_file(&dir, &records).await {
                    tracing::warn!(%error, "failed to persist diagnostic event");
                }
            }
            Command::Dismiss {
                fingerprint,
                at,
                severity,
            } => {
                dismissed.insert(
                    fingerprint,
                    DismissedEntry {
                        dismissed_at: at,
                        severity,
                    },
                );
                prune_dismissed(&mut dismissed, Utc::now());
                if let Err(error) = write_dismissed_file(&dir, &dismissed).await {
                    tracing::warn!(%error, "failed to persist diagnostic dismissal");
                }
            }
            Command::Compact => {
                let now = Utc::now();
                prune_persisted(&mut records, now, &config);
                prune_dismissed(&mut dismissed, now);
                if let Err(error) = write_events_file(&dir, &records).await {
                    tracing::warn!(%error, "failed to compact diagnostic events");
                }
                if let Err(error) = write_dismissed_file(&dir, &dismissed).await {
                    tracing::warn!(%error, "failed to compact diagnostic dismissals");
                }
            }
            #[cfg(test)]
            Command::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

async fn cleanup_loop(weak: Weak<Inner>) {
    let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        interval.tick().await;
        let Some(inner) = weak.upgrade() else {
            break;
        };
        {
            let mut state = inner.state.lock();
            prune_state(&mut state, Utc::now(), &inner.config);
        }
        let _ = inner.writer.try_send(Command::Compact);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::code::DiagnosticCode;
    use tempfile::TempDir;

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn test_config(temp: &TempDir) -> DiagnosticsConfig {
        DiagnosticsConfig {
            enabled: true,
            upload_url: None,
            github_repository: "ApexGP/cursor-byok".to_string(),
            retention_days: 7,
            max_events: 200,
            log_window: Duration::from_secs(20),
            data_dir: temp.path().join("diagnostics"),
        }
    }

    async fn test_store() -> Store {
        Store::connect("sqlite::memory:").await.unwrap()
    }

    fn sync_event(summary: &str) -> DiagnosticEvent {
        DiagnosticEvent::new(DiagnosticCode::Sync, summary)
    }

    #[tokio::test]
    async fn same_fingerprint_is_aggregated() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        hub.record(sync_event("database is locked"));
        hub.record(sync_event("database is locked"));
        let reports = hub.reports().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].occurrences, 2);
        assert_eq!(reports[0].code, "CB-SYNC-001");
    }

    #[tokio::test]
    async fn dismissed_report_is_hidden_until_retention_window() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        hub.record(DiagnosticEvent::new(
            DiagnosticCode::UiBoundary,
            "render failed",
        ));
        let id = hub.reports().await[0].report_id.clone();
        hub.dismiss(&id).await.unwrap();
        assert!(hub.reports().await.is_empty());
    }

    #[tokio::test]
    async fn dismissed_report_surfaces_again_after_24_hours() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        let start = at("2026-11-11T12:00:00Z");
        hub.record_at(sync_event("database is locked"), start);
        let id = hub.reports_at(start).await[0].report_id.clone();
        hub.dismiss_at(&id, start).await.unwrap();

        assert!(hub
            .reports_at(start + chrono::Duration::hours(23))
            .await
            .is_empty());
        assert_eq!(
            hub.reports_at(start + chrono::Duration::hours(25))
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn severity_escalation_resurfaces_a_dismissed_report() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        let start = at("2026-11-11T12:00:00Z");
        hub.record_at(
            DiagnosticEvent::new(DiagnosticCode::CursorState, "state locked"),
            start,
        );
        let id = hub.reports_at(start).await[0].report_id.clone();
        hub.dismiss_at(&id, start).await.unwrap();
        assert!(hub.reports_at(start).await.is_empty());

        let mut escalated = DiagnosticEvent::new(DiagnosticCode::CursorState, "state locked");
        escalated.severity = Severity::Error;
        hub.record_at(escalated, start + chrono::Duration::seconds(30));
        let reports = hub.reports_at(start + chrono::Duration::seconds(30)).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].severity, Severity::Error);
        assert_eq!(reports[0].occurrences, 2);
    }

    #[tokio::test]
    async fn different_fingerprints_stay_separate() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        hub.record(sync_event("database is locked"));
        hub.record(DiagnosticEvent::new(
            DiagnosticCode::Run,
            "database is locked",
        ));
        assert_eq!(hub.reports().await.len(), 2);
    }

    #[tokio::test]
    async fn aggregation_window_creates_a_new_report_after_ten_minutes() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        let start = at("2026-11-11T12:00:00Z");
        hub.record_at(sync_event("database is locked"), start);
        hub.record_at(
            sync_event("database is locked"),
            start + chrono::Duration::minutes(11),
        );
        assert_eq!(
            hub.reports_at(start + chrono::Duration::minutes(11))
                .await
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn redacts_before_storing_and_writing() {
        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        let hub = DiagnosticsHub::new(config.clone(), test_store().await)
            .await
            .unwrap();
        let mut event = DiagnosticEvent::new(DiagnosticCode::Sync, "sync failed api_key=SENTINEL");
        event.error_message = Some("Authorization: Bearer TOKEN-SENTINEL".to_string());
        event.error_chain = vec!["/Users/alice/secret".to_string()];
        hub.record(event);
        hub.flush().await;

        let reports = hub.reports().await;
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].summary.contains("SENTINEL"));
        let stored_event = hub.event(&reports[0].report_id).await.unwrap();
        assert!(!stored_event
            .error_message
            .unwrap_or_default()
            .contains("SENTINEL"));
        assert!(!stored_event.error_chain.join(" ").contains("alice"));

        let raw = std::fs::read_to_string(config.data_dir.join("events.jsonl")).unwrap();
        for secret in ["SENTINEL", "TOKEN-SENTINEL", "alice"] {
            assert!(!raw.contains(secret), "leaked {secret}: {raw}");
        }
    }

    #[tokio::test]
    async fn reports_survive_a_hub_restart() {
        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        let store = test_store().await;
        let hub = DiagnosticsHub::new(config.clone(), store.clone())
            .await
            .unwrap();
        hub.record(sync_event("database is locked"));
        hub.record(sync_event("database is locked"));
        hub.flush().await;
        drop(hub);

        let hub = DiagnosticsHub::new(config, store).await.unwrap();
        let reports = hub.reports().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].occurrences, 2);
    }

    #[tokio::test]
    async fn startup_drops_expired_events_and_compacts_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let expired = DiagnosticEvent::new(DiagnosticCode::Sync, "expired");
        let fresh = DiagnosticEvent::new(DiagnosticCode::Sync, "fresh");
        let lines = [
            serde_json::json!({
                "event": expired,
                "occurrences": 1,
                "firstSeen": "2020-01-01T00:00:00Z",
                "lastSeen": "2020-01-01T00:00:00Z",
            })
            .to_string(),
            serde_json::json!({
                "event": fresh,
                "occurrences": 1,
                "firstSeen": Utc::now(),
                "lastSeen": Utc::now(),
            })
            .to_string(),
        ];
        std::fs::write(
            config.data_dir.join("events.jsonl"),
            format!("{}\n{}\n", lines[0], lines[1]),
        )
        .unwrap();

        let hub = DiagnosticsHub::new(config.clone(), test_store().await)
            .await
            .unwrap();
        let reports = hub.reports().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].summary, "fresh");
        let raw = std::fs::read_to_string(config.data_dir.join("events.jsonl")).unwrap();
        assert!(!raw.contains("expired"));
    }

    #[tokio::test]
    async fn max_events_is_a_hard_cap() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = test_config(&temp);
        config.max_events = 2;
        let hub = DiagnosticsHub::new(config, test_store().await)
            .await
            .unwrap();
        let start = at("2026-11-11T12:00:00Z");
        for (index, summary) in ["first", "second", "third"].into_iter().enumerate() {
            hub.record_at(
                sync_event(summary),
                start + chrono::Duration::seconds(index as i64),
            );
        }
        let reports = hub.reports_at(start + chrono::Duration::seconds(3)).await;
        assert_eq!(reports.len(), 2);
        assert!(!reports.iter().any(|report| report.summary == "first"));
    }

    #[tokio::test]
    async fn dismissing_an_unknown_report_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        assert!(hub.dismiss("r-missing").await.is_err());
    }

    #[tokio::test]
    async fn global_hub_receives_events() {
        let temp = tempfile::tempdir().unwrap();
        let hub = DiagnosticsHub::new(test_config(&temp), test_store().await)
            .await
            .unwrap();
        DiagnosticsHub::install_global(hub.clone());
        assert!(global().is_some());
        record(sync_event("database is locked"));
        assert_eq!(hub.reports().await.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        let hub = DiagnosticsHub::new(config.clone(), test_store().await)
            .await
            .unwrap();
        hub.record(sync_event("database is locked"));
        hub.flush().await;

        let dir_mode = std::fs::metadata(&config.data_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        for file in ["events.jsonl", "dismissed.json"] {
            let mode = std::fs::metadata(config.data_dir.join(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "unexpected mode for {file}");
        }
    }
}
