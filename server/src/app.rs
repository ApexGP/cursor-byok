//! Assembles server dependencies and starts the application services.
use std::{future::IntoFuture, net::SocketAddr, sync::Arc, time::Duration};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{
    api,
    config::{Config, ConsoleSource},
    control,
    cursor::{
        prompting::{PromptAssets, PromptCompiler},
        transport::TransportRegistry,
    },
    diagnostics::{DiagnosticsConfig, DiagnosticsHub},
    local_app::CursorHarness,
    provider::ProviderRouter,
    search::WebCache,
    store::Store,
    Result,
};

pub struct App {
    config: Config,
    router: axum::Router,
    registry: TransportRegistry,
    harness: CursorHarness,
    store: Store,
}

impl App {
    pub async fn new(mut config: Config) -> Result<Self> {
        let store = Store::connect(&config.database_url).await?;
        let diagnostics_config = DiagnosticsConfig {
            data_dir: crate::config::managed_data_dir()?.join("diagnostics"),
            app_version: config.app_version.clone(),
            upload_url: normalize_upload_url(
                std::env::var("CURSOR_DIAGNOSTICS_UPLOAD_URL")
                    .ok()
                    .or_else(|| option_env!("CURSOR_DIAGNOSTICS_UPLOAD_URL").map(str::to_owned)),
            ),
            github_repository: std::env::var("CURSOR_DIAGNOSTICS_GITHUB_REPOSITORY")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "ApexGP/cursor-byok".to_string()),
            enabled: true,
            retention_days: 7,
            max_events: 200,
            log_window: Duration::from_secs(120),
        };
        let diagnostics_data_dir = diagnostics_config.data_dir.clone();
        let diagnostics = init_diagnostics(store.clone(), diagnostics_config).await;
        crate::diagnostics::install_panic_hook(diagnostics_data_dir.clone());
        if let Some(hub) = &diagnostics {
            if let Err(error) = crate::diagnostics::recover_crash_files(hub, &diagnostics_data_dir)
            {
                tracing::warn!(%error, "failed to recover diagnostic crash files");
            }
        }
        if config.use_persisted_ports {
            config
                .listen_addr
                .set_port(store.port_settings().await?.service_port);
        }
        let assets = PromptAssets::embedded()?;
        let compiler = PromptCompiler::new(assets);
        let clients = crate::network::NetworkClients::new(store.clone());
        let provider = std::sync::Arc::new(ProviderRouter::new(
            store.clone(),
            clients.clone(),
            config.provider_request_timeout,
            config.provider_stream_idle_timeout,
        ));
        let registry = TransportRegistry::with_web_cache_and_local_rules(
            store.clone(),
            provider.clone(),
            compiler,
            WebCache::managed()?,
            crate::config::managed_data_dir()?.join("rules"),
        );
        let control = control::ControlService::new(store.clone(), provider, clients.clone())?;
        if let Some(hub) = diagnostics {
            DiagnosticsHub::install_global(hub.clone());
            control.set_diagnostics(hub);
        }
        // Desktop startup syncs the catalog from `CursorHarness::restore()`
        // after the listener is bound, so it always uses the actual service
        // port. Skipping the pre-bind sync avoids writing a stale URL twice.
        let harness = control.cursor_harness().clone();
        let mut router = api::router(registry.clone(), clients)?;
        router = match &config.console {
            Some(ConsoleSource::Directory(directory)) => {
                router.merge(control::web_router(control.clone(), directory))
            }
            Some(ConsoleSource::Proxy(target)) => {
                router.merge(control::proxy_web_router(control.clone(), target.clone()))
            }
            None => router.merge(control::api_router(control.clone())),
        };
        Ok(Self {
            router,
            registry,
            harness,
            store,
            config,
        })
    }

    pub fn merge_router(mut self, router: axum::Router) -> Self {
        self.router = self.router.merge(router);
        self
    }

    pub async fn bind(&self) -> Result<TcpListener> {
        let requested = self.config.listen_addr;
        let listener = bind_service_listener(requested, self.config.use_persisted_ports).await?;
        if self.config.use_persisted_ports {
            self.store
                .set_service_port(listener.local_addr()?.port())
                .await?;
        }
        Ok(listener)
    }

    pub fn harness(&self) -> CursorHarness {
        self.harness.clone()
    }

    pub fn store(&self) -> Store {
        self.store.clone()
    }

    pub async fn serve(self) -> Result<()> {
        let listener = self.bind().await?;
        let shutdown = CancellationToken::new();
        let signal_shutdown = shutdown.clone();
        let running = self.serve_on(listener, shutdown);
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => result,
            () = shutdown_signal() => {
                tracing::info!("shutdown signal received; cancelling active runs");
                signal_shutdown.cancel();
                running.await
            }
        }
    }

    pub async fn serve_on(self, listener: TcpListener, shutdown: CancellationToken) -> Result<()> {
        let address = listener.local_addr()?;
        self.registry.web_cache().set_service_addr(address);
        self.harness.set_backend_addr(address);
        tracing::info!(%address, "cursor server listening");
        let registry = self.registry;
        let harness = self.harness;
        let graceful = shutdown.clone();
        let server = axum::serve(listener, self.router)
            .with_graceful_shutdown(async move {
                graceful.cancelled().await;
            })
            .into_future();
        tokio::pin!(server);

        tokio::select! {
            result = &mut server => {
                if let Err(error) = harness.disable().await {
                    tracing::warn!(%error, "failed to disable Cursor harness after server stop");
                }
                result?
            },
            () = shutdown.cancelled() => {
                if let Err(error) = harness.disable().await {
                    tracing::warn!(%error, "failed to disable Cursor harness during shutdown");
                }
                registry.shutdown().await;
                match tokio::time::timeout(Duration::from_secs(10), &mut server).await {
                    Ok(result) => result?,
                    Err(_) => tracing::warn!("graceful shutdown timed out; forcing server close"),
                }
            }
        }
        Ok(())
    }
}

/// Diagnostics is optional: storage failures must never prevent the main
/// service from starting.
async fn init_diagnostics(store: Store, config: DiagnosticsConfig) -> Option<Arc<DiagnosticsHub>> {
    match DiagnosticsHub::new(config, store).await {
        Ok(hub) => Some(hub),
        Err(error) => {
            tracing::warn!(%error, "failed to initialize diagnostics; continuing without it");
            None
        }
    }
}

/// Normalizes a diagnostics upload URL: blank values disable uploads.
fn normalize_upload_url(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn bind_service_listener(
    requested: SocketAddr,
    allow_random_fallback: bool,
) -> Result<TcpListener> {
    match TcpListener::bind(requested).await {
        Ok(listener) => Ok(listener),
        Err(error) if allow_random_fallback && requested.port() != 0 => {
            tracing::warn!(%requested, %error, "configured service port unavailable; selecting a random port");
            Ok(TcpListener::bind(SocketAddr::new(requested.ip(), 0)).await?)
        }
        Err(error) => Err(error.into()),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

#[cfg(test)]
mod tests {
    use super::{init_diagnostics, normalize_upload_url};
    use crate::{diagnostics::DiagnosticsConfig, store::Store};
    use std::time::Duration;

    fn test_diagnostics_config(data_dir: std::path::PathBuf) -> DiagnosticsConfig {
        DiagnosticsConfig {
            enabled: true,
            upload_url: None,
            github_repository: "ApexGP/cursor-byok".to_string(),
            retention_days: 7,
            max_events: 200,
            log_window: Duration::from_secs(120),
            data_dir,
            app_version: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn diagnostics_init_failure_is_non_fatal() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("diagnostics");
        // An existing file makes `create_dir_all` fail, exercising the
        // best-effort path without touching permissions.
        std::fs::write(&data_dir, b"not a directory").unwrap();
        let store = Store::connect("sqlite::memory:").await.unwrap();

        let hub = init_diagnostics(store, test_diagnostics_config(data_dir)).await;

        assert!(hub.is_none());
    }

    #[test]
    fn blank_upload_url_is_disabled() {
        assert_eq!(normalize_upload_url(None), None);
        assert_eq!(normalize_upload_url(Some(String::new())), None);
        assert_eq!(normalize_upload_url(Some("   \n\t".to_string())), None);
    }

    #[test]
    fn upload_url_is_trimmed() {
        assert_eq!(
            normalize_upload_url(Some(
                "  https://example.com/diagnostics/upload \n".to_string()
            )),
            Some("https://example.com/diagnostics/upload".to_string())
        );
    }
}
