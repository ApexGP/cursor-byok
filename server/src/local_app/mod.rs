//! Exposes the local desktop application integration.
mod account;
mod ca;
pub(crate) mod model_cache;
mod process;
mod proxy;
mod settings;

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    plugin::PluginRegistry,
    store::{Store, TabMode, TabSettings},
    Error, Result,
};

use self::{ca::CaManager, proxy::ProxyRuntime};

pub(crate) fn proxy_host_allowed(host: &str) -> bool {
    proxy::is_cursor_host(host)
}

pub(crate) fn request_uses_local_cursor_token(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(account::is_local_cursor_authorization)
}

#[cfg(test)]
pub(crate) fn local_cursor_authorization() -> String {
    format!("Bearer {}", account::local_token().unwrap())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaState {
    Missing,
    Untrusted,
    Ready,
    Invalid,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Disabled,
    Enabled,
    Degraded,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorHarnessStatus {
    pub platform: &'static str,
    pub ca: CaState,
    pub configured_models: usize,
    pub enabled_models: usize,
    pub integration: IntegrationState,
    pub settings_applied: bool,
    pub proxy_url: Option<String>,
    pub ca_install_command: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SetEnabled {
    pub enabled: bool,
}

#[derive(Clone)]
pub struct CursorHarness {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    ca: CaManager,
    ca_initialization: Mutex<()>,
    backend_addr: RwLock<Option<SocketAddr>>,
    tab_mode: Arc<RwLock<TabMode>>,
    plugins: RwLock<Option<PluginRegistry>>,
    proxy: Mutex<ProxyRuntime>,
    supervisor_started: AtomicBool,
}

impl CursorHarness {
    pub fn new(store: Store) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                ca: CaManager::managed()?,
                ca_initialization: Mutex::new(()),
                backend_addr: RwLock::new(None),
                tab_mode: Arc::new(RwLock::new(TabMode::default())),
                plugins: RwLock::new(None),
                proxy: Mutex::new(ProxyRuntime::default()),
                supervisor_started: AtomicBool::new(false),
            }),
        })
    }

    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.inner.backend_addr.write() = Some(addr);
    }

    /// Lets the harness include plugin-provided models when mirroring the
    /// catalog into Cursor's state. The control service installs the registry
    /// once it is available.
    pub fn set_plugins(&self, plugins: PluginRegistry) {
        *self.inner.plugins.write() = Some(plugins);
    }

    pub async fn proxy_port(&self) -> Option<u16> {
        self.inner.proxy.lock().await.port()
    }

    async fn live_proxy_url(&self) -> Option<String> {
        self.inner.proxy.lock().await.url()
    }

    fn backend_url(&self) -> Option<String> {
        self.inner
            .backend_addr
            .read()
            .map(|address| format!("http://{address}"))
    }

    /// Starts one background loop that revives a dead local proxy and repairs
    /// Cursor's `http.proxy` if it drifts while takeover is enabled. The loop
    /// is a no-op while takeover is disabled.
    fn ensure_supervisor(&self) {
        if self.inner.supervisor_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let harness = self.clone();
        tracing::info!("Cursor proxy supervisor started");
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if let Err(error) = harness.supervise_once().await {
                    tracing::warn!(%error, "failed to supervise Cursor proxy");
                }
            }
        });
    }

    async fn supervise_once(&self) -> Result<()> {
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Ok(());
        }
        if !matches!(self.inner.ca.state()?, CaState::Ready) {
            return Ok(());
        }
        let (running, proxy_url) = {
            let proxy = self.inner.proxy.lock().await;
            (proxy.running(), proxy.url())
        };
        if !running {
            if !self.inner.store.cursor_takeover_enabled().await? {
                return Ok(());
            }
            tracing::warn!("local Cursor proxy stopped unexpectedly; restarting it");
            self.ensure_proxy().await?;
            return Ok(());
        }
        if let Some(proxy_url) = proxy_url {
            if !settings::settings_match(&proxy_url)? {
                // Serialize with `disable()` (which stops and clears settings
                // under the same proxy lock) and re-check the takeover state
                // under the lock before writing.
                let proxy = self.inner.proxy.lock().await;
                if !self.inner.store.cursor_takeover_enabled().await? {
                    return Ok(());
                }
                let Some(live_url) = proxy.url() else {
                    return Ok(());
                };
                if settings::settings_match(&live_url)? {
                    return Ok(());
                }
                tracing::warn!(
                    proxy_url = live_url,
                    "Cursor proxy settings drifted; restoring the local proxy"
                );
                apply_cursor_configuration(&live_url).await?;
            }
        }
        Ok(())
    }

    pub async fn cleanup_stale_settings(&self) -> Result<()> {
        // When takeover is enabled, keep the managed proxy settings: `restore`
        // rewrites them with the live port on every start. Clearing them first
        // leaves Cursor without a proxy if the UI never loads (e.g. a dev-server
        // startup race).
        if self.inner.store.cursor_takeover_enabled().await?
            && matches!(self.inner.ca.state()?, CaState::Ready)
        {
            return Ok(());
        }
        settings::clear_stale_managed_settings()
    }

    /// Restores an already-enabled takeover at process startup without
    /// terminating Cursor. The UI path (`status` -> `enable`) may optionally
    /// terminate Cursor so new settings take effect sooner; startup must not.
    pub async fn restore(&self) -> Result<()> {
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Ok(());
        }
        if !matches!(self.inner.ca.state()?, CaState::Ready) {
            tracing::info!("Cursor takeover is enabled but the local CA is not ready");
            return Ok(());
        }
        let previous_proxy_url = settings::proxy_url().unwrap_or(None);
        self.ensure_proxy().await?;
        self.ensure_supervisor();
        let proxy_url = self.live_proxy_url().await;
        let proxy_changed =
            proxy_url.is_some() && previous_proxy_url.as_deref() != proxy_url.as_deref();
        if let Err(error) = self.sync_model_cache().await {
            tracing::warn!(%error, "failed to sync Cursor model cache at startup");
        }
        let service_url = self.backend_url();
        tracing::info!(
            proxy_port = self.proxy_port().await,
            service_url = service_url.as_deref(),
            proxy_changed,
            "restored Cursor takeover at startup"
        );
        // Let the HTTP server start before relaunching Cursor against the new
        // service port. This also refreshes Cursor's process-level proxy/config
        // after a port change.
        let harness = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(error) = harness.reconcile_cursor_backend(proxy_changed).await {
                tracing::warn!(%error, "failed to reconcile Cursor backend after startup");
            }
        });
        Ok(())
    }

    /// Mirrors configured models into Cursor's persisted catalog. The
    /// agent-host module may overwrite the official catalog directly, so this
    /// is re-applied at startup and after model changes.
    pub async fn sync_model_cache(&self) -> Result<()> {
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Ok(());
        }
        let plugins = self.inner.plugins.read().clone();
        let service_url = self.backend_url();
        model_cache::sync_with(&self.inner.store, plugins.as_ref(), service_url.as_deref()).await?;
        Ok(())
    }

    pub async fn status(&self) -> Result<CursorHarnessStatus> {
        let models = self.inner.store.models().await?;
        let configured_models = models.len();
        let enabled_models = configured_models;
        let ca = self.inner.ca.state()?;
        if self.inner.store.cursor_takeover_enabled().await?
            && matches!(ca, CaState::Ready)
            && self.inner.backend_addr.read().is_some()
        {
            self.enable().await?;
            self.ensure_supervisor();
        }
        let proxy = self.inner.proxy.lock().await;
        let proxy_url = proxy.url();
        let settings_applied = proxy_url
            .as_deref()
            .map(settings::settings_match)
            .transpose()?
            .unwrap_or(false);
        let integration = match (proxy.running(), settings_applied) {
            (false, false) => IntegrationState::Disabled,
            (true, true) => IntegrationState::Enabled,
            _ => IntegrationState::Degraded,
        };
        Ok(CursorHarnessStatus {
            platform: std::env::consts::OS,
            ca,
            configured_models,
            enabled_models,
            integration,
            settings_applied,
            proxy_url,
            ca_install_command: self.inner.ca.install_command(),
        })
    }

    pub async fn initialize_ca(&self) -> Result<CursorHarnessStatus> {
        let _initialization = self.inner.ca_initialization.lock().await;
        let manager = self.inner.ca.clone();
        tokio::task::spawn_blocking(move || manager.initialize_local())
            .await
            .map_err(|error| Error::Store(format!("CA initialization task failed: {error}")))??;
        self.status().await
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<CursorHarnessStatus> {
        if enabled {
            self.inner.store.set_cursor_takeover_enabled(true).await?;
            self.enable().await?;
            self.ensure_supervisor();
            self.sync_model_cache().await?;
            if let Err(error) = self.restart_cursor().await {
                tracing::warn!(%error, "failed to restart Cursor with the local backend");
            }
        } else {
            self.inner.store.set_cursor_takeover_enabled(false).await?;
            self.disable().await?;
        }
        self.status().await
    }

    /// Restarts Cursor with `--test-backend-url` pointing at this service.
    /// Cursor's extension-host transport bypasses `http.proxy`; this is the
    /// supported way to keep Agent Run/Bidi on the local HTTP/1.1 endpoint.
    pub async fn restart_cursor(&self) -> Result<()> {
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Err(Error::Config("Cursor takeover is not enabled".into()));
        }
        let service_url = self
            .backend_url()
            .ok_or_else(|| Error::Config("desktop management server is not ready".into()))?;
        process::restart_cursor_with_backend(&service_url).await
    }

    /// Refreshes Cursor after a BYOK restart when either the proxy port changed
    /// or Cursor still points at a previous local service port. Cursor launched
    /// for an unrelated backend (or without `--test-backend-url`) is left alone.
    async fn reconcile_cursor_backend(&self, proxy_changed: bool) -> Result<()> {
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Ok(());
        }
        let Some(service_url) = self.backend_url() else {
            return Ok(());
        };
        if !process::cursor_running().await? {
            tracing::info!(
                service_url,
                "Cursor is not running; it must be launched with --test-backend-url"
            );
            return Ok(());
        }
        let current = process::cursor_backend_override().await?;
        let molten_ok = if current.as_deref().is_some_and(is_managed_local_backend) {
            process::cursor_has_molten_backend(&service_url).await?
        } else {
            None
        };
        if should_restart_cursor(proxy_changed, current.as_deref(), &service_url, molten_ok) {
            tracing::info!(
                service_url,
                proxy_changed,
                molten_ok,
                "restarting Cursor to apply local backend settings"
            );
            process::restart_cursor_with_backend(&service_url).await?;
            return Ok(());
        }
        match current {
            Some(url) if url == service_url => {
                tracing::info!(service_url, "Cursor already uses the local backend")
            }
            Some(url) => {
                tracing::info!(url, "Cursor uses a non-managed backend URL; not restarting")
            }
            None => tracing::info!(
                "Cursor is running without a detected --test-backend-url; use the restart button"
            ),
        }
        Ok(())
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        let saved = self.inner.store.set_tab_settings(settings).await?;
        *self.inner.tab_mode.write() = saved.mode;
        Ok(saved)
    }

    async fn enable(&self) -> Result<()> {
        self.ensure_proxy().await
    }

    async fn ensure_proxy(&self) -> Result<()> {
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Err(Error::Config("Cursor takeover is not enabled".into()));
        }
        if !matches!(self.inner.ca.state()?, CaState::Ready) {
            return Err(Error::Config(
                "initialize and trust the CA before enabling Cursor".into(),
            ));
        }
        let backend_addr = self
            .inner
            .backend_addr
            .read()
            .ok_or_else(|| Error::Config("desktop management server is not ready".into()))?;
        let mut proxy = self.inner.proxy.lock().await;
        // `disable()` flips the takeover flag before taking this lock. Re-check
        // under the lock so a supervisor tick that raced with disable cannot
        // start a proxy after takeover was turned off.
        if !self.inner.store.cursor_takeover_enabled().await? {
            return Err(Error::Config("Cursor takeover is not enabled".into()));
        }
        if proxy.running() {
            if let Some(url) = proxy.url() {
                apply_cursor_configuration(&url).await?;
            }
            return Ok(());
        }
        let ca = self.inner.ca.load()?;
        let requested_port = self.inner.store.port_settings().await?.proxy_port;
        *self.inner.tab_mode.write() = self.inner.store.tab_settings().await?.mode;
        let (url, actual_port) = proxy
            .start(
                backend_addr,
                ca,
                requested_port,
                self.inner.tab_mode.clone(),
            )
            .await?;
        if let Err(error) = self.inner.store.set_proxy_port(actual_port).await {
            proxy.stop().await;
            return Err(error);
        }
        if let Err(error) = apply_cursor_configuration(&url).await {
            proxy.stop().await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn disable(&self) -> Result<()> {
        // Keep the proxy lock while stopping and clearing settings so a
        // concurrent supervisor tick cannot write the proxy settings after
        // takeover was disabled.
        let mut proxy = self.inner.proxy.lock().await;
        proxy.stop().await;
        settings::clear_proxy_settings()?;
        Ok(())
    }
}

async fn apply_cursor_configuration(proxy_url: &str) -> Result<()> {
    account::inject_if_missing().await?;
    settings::write_proxy_settings(proxy_url)
}

fn is_managed_local_backend(url: &str) -> bool {
    [
        "http://127.0.0.1:",
        "http://localhost:",
        "https://localhost:",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
}

fn should_restart_cursor(
    proxy_changed: bool,
    current: Option<&str>,
    service_url: &str,
    molten_ok: Option<bool>,
) -> bool {
    proxy_changed
        || current.is_some_and(|url| {
            is_managed_local_backend(url) && (url != service_url || molten_ok == Some(false))
        })
}

#[cfg(test)]
mod tests {
    use super::{is_managed_local_backend, should_restart_cursor};

    #[test]
    fn recognizes_only_managed_local_backend_urls() {
        assert!(is_managed_local_backend("http://127.0.0.1:59112"));
        assert!(is_managed_local_backend("http://localhost:59112"));
        assert!(is_managed_local_backend("https://localhost:8000"));
        assert!(!is_managed_local_backend("https://api2.cursor.sh"));
        assert!(!is_managed_local_backend(
            "https://127.0.0.1.example.com:59112"
        ));
    }

    #[test]
    fn restarts_only_for_changed_proxy_or_stale_managed_backend() {
        let service = "http://127.0.0.1:59112";
        assert!(should_restart_cursor(true, None, service, None));
        assert!(should_restart_cursor(
            false,
            Some("http://127.0.0.1:58085"),
            service,
            None
        ));
        assert!(!should_restart_cursor(
            false,
            Some(service),
            service,
            Some(true)
        ));
        assert!(should_restart_cursor(
            false,
            Some(service),
            service,
            Some(false)
        ));
        assert!(!should_restart_cursor(false, None, service, None));
        assert!(!should_restart_cursor(
            false,
            Some("https://api2.cursor.sh"),
            service,
            Some(false)
        ));
    }
}
