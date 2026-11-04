//! Configures the local application proxy.
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex, OnceLock},
};

use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{Request, Uri},
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

use parking_lot::RwLock;

use crate::{
    api::cursor::proxy::UPSTREAM_URL_HEADER, cursor::services::tab::is_tab_path, store::TabMode,
    Error, Result,
};

use super::ca::LoadedCa;

#[derive(Default)]
pub struct ProxyRuntime {
    url: Option<String>,
    port: Option<u16>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ProxyRuntime {
    pub fn running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
    pub fn url(&self) -> Option<String> {
        self.running().then(|| self.url.clone()).flatten()
    }
    pub fn port(&self) -> Option<u16> {
        if self.running() {
            self.port
        } else {
            None
        }
    }

    pub async fn start(
        &mut self,
        backend: SocketAddr,
        ca: LoadedCa,
        requested_port: u16,
        tab_mode: Arc<RwLock<TabMode>>,
    ) -> Result<(String, u16)> {
        if let Some(url) = self.url() {
            return Ok((url, self.port.unwrap_or_default()));
        }
        let listener = bind_proxy_listener(requested_port).await?;
        let address = listener.local_addr()?;
        let (stop, done) = oneshot::channel();
        let authority = RcgenAuthority::new(ca.issuer, 1_000, aws_lc_rs::default_provider());
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(CursorRelay { backend, tab_mode })
            .with_graceful_shutdown(async move {
                let _ = done.await;
            })
            .build()
            .map_err(|error| Error::Store(format!("build Cursor proxy: {error}")))?;
        self.stop = Some(stop);
        self.url = Some(format!("http://{address}"));
        self.port = Some(address.port());
        self.task = Some(tokio::spawn(async move {
            if let Err(error) = proxy.start().await {
                tracing::error!(%error, "Cursor proxy stopped unexpectedly");
            }
        }));
        Ok((self.url.clone().unwrap(), address.port()))
    }

    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        self.url = None;
        self.port = None;
    }
}

async fn bind_proxy_listener(requested_port: u16) -> Result<TcpListener> {
    let requested = SocketAddr::from(([127, 0, 0, 1], requested_port));
    match TcpListener::bind(requested).await {
        Ok(listener) => Ok(listener),
        Err(error) if requested_port != 0 => {
            tracing::warn!(%requested, %error, "configured proxy port unavailable; selecting a random port");
            Ok(TcpListener::bind("127.0.0.1:0").await?)
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone)]
struct CursorRelay {
    backend: SocketAddr,
    tab_mode: Arc<RwLock<TabMode>>,
}

impl HttpHandler for CursorRelay {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut request: Request<Body>,
    ) -> RequestOrResponse {
        let original = request.uri().clone();
        // CONNECT uses authority-form URIs; rewriting them would hide the Cursor
        // host from `should_intercept_connect` and break TLS interception.
        if request.method() == hudsucker::hyper::Method::CONNECT {
            return request.into();
        }
        let locally_routed = should_route_locally(original.path(), *self.tab_mode.read());
        if is_managed_host(original.host().unwrap_or_default()) && locally_routed {
            if !is_local_path(original.path()) {
                log_unhandled_path_once(original.path());
            }
            if let Ok(value) = original.to_string().parse() {
                request.headers_mut().insert(UPSTREAM_URL_HEADER, value);
            }
            let path = original
                .path_and_query()
                .map(|value| value.as_str())
                .unwrap_or("/");
            if let Ok(uri) = format!("http://{}{}", self.backend, path).parse::<Uri>() {
                *request.uri_mut() = uri;
            }
        }
        request.into()
    }

    async fn should_intercept_connect(
        &mut self,
        _ctx: &HttpContext,
        request: &Request<Body>,
    ) -> bool {
        request
            .uri()
            .authority()
            .is_some_and(|authority| is_managed_host(authority.host()))
    }

    async fn should_intercept_tls(
        &mut self,
        _ctx: &HttpContext,
        hello: hudsucker::rustls::server::ClientHello<'_>,
    ) -> bool {
        hello.server_name().is_some_and(is_managed_host)
    }
}

/// Hosts whose traffic must pass through the local backend so it can be
/// routed to Cursor APIs or forwarded through the configured outbound proxy.
/// `marketplace.cursorapi.com` is included because the extension gallery is not
/// a `*.cursor.sh` host; without interception its CONNECT tunnel bypasses the
/// system proxy and extension panels load directly.
pub fn is_managed_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(host.as_str(), "api2.cursor.sh" | "api3.cursor.sh")
        || host.ends_with(".cursor.sh")
        || host.ends_with(".cursorapi.com")
}

/// Logs each unhandled Cursor path once so a Cursor update that adds a new
/// model RPC is visible in normal INFO logs without flooding them.
fn log_unhandled_path_once(path: &str) {
    static SEEN: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| StdMutex::new(HashSet::new()));
    if let Ok(mut seen) = seen.lock() {
        if seen.insert(path.to_string()) {
            tracing::info!(
                path,
                "Cursor path is not locally handled; forwarding through the configured outbound proxy"
            );
        }
    }
}

fn is_local_path(path: &str) -> bool {
    matches!(
        path,
        "/agent.v1.AgentService/RunSSE"
            | "/aiserver.v1.BidiService/BidiAppend"
            | "/aiserver.v1.AiService/AvailableDocs"
            | "/aiserver.v1.DashboardService/GetEffectiveUserPlugins"
            | "/aiserver.v1.DashboardService/GetUserPrivacyMode"
            | "/agent.v1.AgentService/UpdateConversationMetadata"
            | "/aiserver.v1.AiService/GetServerConfig"
            | "/aiserver.v1.ServerConfigService/GetServerConfig"
            | "/aiserver.v1.AiService/AvailableModels"
            | "/agent.v1.AgentService/GetUsableModels"
            | "/aiserver.v1.AiService/GetUsableModels"
            | "/agent.v1.AgentService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModel"
            | "/aiserver.v1.AiService/GetDefaultModelNudgeData"
            | "/aiserver.v1.AiService/GetLastDefaultModelNudge"
            | "/aiserver.v1.AiService/GetModelLabels"
            | "/aiserver.v1.CppService/AvailableModels"
            | "/aiserver.v1.AuthService/GetEmail"
            | "/aiserver.v1.AuthService/GetUserMeta"
            | "/aiserver.v1.DashboardService/GetMe"
            | "/aiserver.v1.DashboardService/GetTeams"
            | "/aiserver.v1.DashboardService/GetUserProfile"
            | "/aiserver.v1.DashboardService/GetCurrentPeriodUsage"
            | "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants"
            | "/aiserver.v1.AiService/KnowledgeBaseAdd"
            | "/aiserver.v1.AiService/KnowledgeBaseList"
            | "/aiserver.v1.AiService/KnowledgeBaseUpdate"
            | "/aiserver.v1.AiService/KnowledgeBaseRemove"
            | "/aiserver.v1.AiService/WriteGitCommitMessage"
            | "/aiserver.v1.NetworkService/IsConnected"
            | "/aiserver.v1.AnalyticsService/BootstrapStatsig"
            | "/auth/full_stripe_profile"
            | "/auth/stripe_profile"
    )
}

fn should_route_locally(path: &str, tab_mode: TabMode) -> bool {
    // Model catalog RPCs always stay local so Cursor can see BYOK models even
    // when Tab is configured for direct mode.
    is_model_catalog_path(path) || !(is_tab_path(path) && tab_mode == TabMode::Direct)
}

fn is_model_catalog_path(path: &str) -> bool {
    matches!(
        path,
        "/aiserver.v1.AiService/AvailableModels"
            | "/aiserver.v1.CppService/AvailableModels"
            | "/agent.v1.AgentService/GetUsableModels"
            | "/aiserver.v1.AiService/GetUsableModels"
            | "/agent.v1.AgentService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModelForCli"
            | "/aiserver.v1.AiService/GetDefaultModel"
            | "/aiserver.v1.AiService/GetDefaultModelNudgeData"
            | "/aiserver.v1.AiService/GetLastDefaultModelNudge"
            | "/aiserver.v1.AiService/GetModelLabels"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_gallery_traffic_is_managed() {
        assert!(is_managed_host("marketplace.cursorapi.com"));
        assert!(is_managed_host("API2.CURSOR.SH."));
        assert!(is_managed_host("api3.cursor.sh"));
        assert!(!is_managed_host("example.com"));
        assert!(!is_managed_host("notcursor.sh.example.com"));
    }

    #[test]
    fn cursor_cli_transport_and_model_metadata_routes_stay_local() {
        for path in [
            "/aiserver.v1.AiService/GetServerConfig",
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            "/agent.v1.AgentService/GetDefaultModelForCli",
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            "/aiserver.v1.AiService/GetDefaultModel",
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            "/aiserver.v1.AiService/GetLastDefaultModelNudge",
            "/aiserver.v1.AiService/GetModelLabels",
            "/aiserver.v1.CppService/AvailableModels",
            "/aiserver.v1.AiService/AvailableDocs",
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            "/aiserver.v1.AuthService/GetUserMeta",
            "/agent.v1.AgentService/UpdateConversationMetadata",
            "/auth/full_stripe_profile",
            "/auth/stripe_profile",
        ] {
            assert!(is_local_path(path), "{path} must not reach Cursor upstream");
        }
    }

    #[test]
    fn model_catalog_paths_ignore_tab_direct_mode() {
        for path in [
            "/aiserver.v1.AiService/AvailableModels",
            "/aiserver.v1.CppService/AvailableModels",
            "/agent.v1.AgentService/GetUsableModels",
            "/aiserver.v1.AiService/GetDefaultModel",
            "/aiserver.v1.AiService/GetModelLabels",
        ] {
            assert!(
                should_route_locally(path, TabMode::Direct),
                "{path} must stay local in Tab Direct mode"
            );
        }
    }

    #[test]
    fn non_tab_paths_use_the_local_backend_in_direct_mode() {
        assert!(should_route_locally(
            "/aiserver.v1.AiService/UnhandledPath",
            TabMode::Direct
        ));
        assert!(!should_route_locally(
            "/aiserver.v1.AiService/StreamCpp",
            TabMode::Direct
        ));
        assert!(should_route_locally(
            "/aiserver.v1.AiService/StreamCpp",
            TabMode::Public
        ));
    }
}
