//! Captures panics into crash files and recovers them on the next start.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    diagnostics::{
        record, DiagnosticCode, DiagnosticContext, DiagnosticEvent, DiagnosticsHub, Redactor,
    },
    Result,
};

/// Installs a panic hook that preserves the previous hook.
pub fn install_panic_hook(data_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown".into());
        let event = build_panic_event(&message, &location);
        // The global writer may not get a chance to drain before the process
        // aborts, so persist a synchronous crash file as the durable record.
        if let Err(error) = write_crash_file(&data_dir, &event) {
            tracing::warn!(%error, "failed to write diagnostic crash file");
        }
        record(event);
        previous(info);
    }));
}

/// Reads crash files written by the panic hook and enqueues them.
pub fn recover_crash_files(hub: &DiagnosticsHub, data_dir: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };

    let mut recovered = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "failed to read diagnostics crash directory entry");
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("crash-") || !name.ends_with(".json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<DiagnosticEvent>(&text).ok())
        {
            Some(event) => {
                hub.record(event);
                recovered += 1;
            }
            None => {
                tracing::warn!(path = %path.display(), "ignoring malformed diagnostic crash file")
            }
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(%error, path = %path.display(), "failed to remove diagnostic crash file");
        }
    }
    Ok(recovered)
}

pub(crate) fn build_panic_event(message: &str, location: &str) -> DiagnosticEvent {
    let redactor = Redactor::new();
    let message = redactor.redact(message);
    let location = redactor.redact(location);
    let summary: String = format!("Panic at {location}: {message}")
        .chars()
        .take(200)
        .collect();
    let context = DiagnosticContext::new("app", "panic");
    DiagnosticEvent::new(DiagnosticCode::AppPanic, summary)
        .with_error("panic", message)
        .with_context(context)
}

fn write_crash_file(data_dir: &Path, event: &DiagnosticEvent) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = data_dir.join(format!("crash-{stamp}.json"));
    std::fs::write(&path, serde_json::to_vec(event)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diagnostics::DiagnosticsConfig, store::Store};
    use std::time::Duration;

    #[test]
    fn panic_event_redacts_payload_and_location() {
        let event = build_panic_event(
            "panic with token=SECRET-SENTINEL",
            "/Users/alice/project/main.rs:10:2",
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("SECRET-SENTINEL"));
        assert!(!json.contains("alice"));
        assert_eq!(event.code.as_str(), "CB-APP-002");
    }

    #[tokio::test]
    async fn crash_files_are_recovered_and_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _db) = test_store(&temp).await;
        let config = test_config(&temp);
        let hub = DiagnosticsHub::new(config, store).await.unwrap();
        let event = build_panic_event("boom", "main.rs:1:1");
        let path = temp.path().join("diagnostics").join("crash-1.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&event).unwrap()).unwrap();

        let count = recover_crash_files(&hub, path.parent().unwrap()).unwrap();
        assert_eq!(count, 1);
        assert!(!path.exists());
        assert!(hub
            .reports()
            .await
            .iter()
            .any(|report| report.code == "CB-APP-002"));
    }

    #[tokio::test]
    async fn malformed_crash_files_are_ignored_without_panicking() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _db) = test_store(&temp).await;
        let hub = DiagnosticsHub::new(test_config(&temp), store)
            .await
            .unwrap();
        let dir = temp.path().join("diagnostics");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crash-bad.json");
        std::fs::write(&path, "{not-json").unwrap();

        assert_eq!(recover_crash_files(&hub, &dir).unwrap(), 0);
        assert!(!path.exists());
    }

    async fn test_store(temp: &tempfile::TempDir) -> (Store, PathBuf) {
        let path = temp.path().join("test.db");
        let store = Store::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        (store, path)
    }

    fn test_config(temp: &tempfile::TempDir) -> DiagnosticsConfig {
        DiagnosticsConfig {
            enabled: true,
            upload_url: None,
            github_repository: "ApexGP/cursor-byok".into(),
            retention_days: 7,
            max_events: 200,
            log_window: Duration::from_secs(120),
            data_dir: temp.path().join("diagnostics"),
            app_version: "test".into(),
        }
    }
}
