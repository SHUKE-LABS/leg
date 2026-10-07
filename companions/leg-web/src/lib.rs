//! Authenticated loopback HTTP host for companion session control.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as RoutePath, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response, Sse, sse::Event};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use fs2::FileExt;
use leg_ui_client::{
    CatalogError, CatalogRunState, CatalogSession, CatalogTurn, SessionCatalog,
    SessionCatalogConfig, SessionInterface, StreamEvent, TurnOutcome, TurnStopHandle,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const CONTROLLER_JS: &str = include_str!("assets/controller.js");
#[cfg(feature = "browser-e2e-themes")]
const THEME_REGISTRY_JS: &str = include_str!("assets/themes/registry-e2e.js");
#[cfg(not(feature = "browser-e2e-themes"))]
const THEME_REGISTRY_JS: &str = include_str!("assets/themes/registry.js");
const DEFAULT_THEME_JS: &str = include_str!("assets/themes/default.js");
const DEFAULT_THEME_HTML: &str = include_str!("assets/themes/default.html");
const DEFAULT_THEME_CSS: &str = include_str!("assets/themes/default.css");
const SHARED_RENDERER_JS: &str = include_str!("assets/themes/shared-renderer.js");
#[cfg(feature = "browser-e2e-themes")]
const FIXTURE_THEME_JS: &str = include_str!("assets/themes/fixture.js");
#[cfg(feature = "browser-e2e-themes")]
const FIXTURE_THEME_HTML: &str = include_str!("assets/themes/fixture.html");
#[cfg(feature = "browser-e2e-themes")]
const FIXTURE_THEME_CSS: &str = include_str!("assets/themes/fixture.css");
/// Fixed default so the launch address stays stable across restarts.
pub const DEFAULT_PORT: u16 = 13579;
const STORE_NAME: &str = "web-host-state.json";
const LOCK_NAME: &str = ".leg-web.lock";
const STORE_VERSION: u32 = 1;
const DEFAULT_EVENT_BUFFER: usize = 256;
const DEFAULT_RECEIPTS: usize = 128;
const MAX_BODY_BYTES: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Only `127.0.0.1` is accepted. Port `0` asks the OS for a free port.
    pub bind_addr: SocketAddr,
    pub catalog: SessionCatalogConfig,
    pub event_buffer: usize,
    pub receipt_limit: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
            catalog: SessionCatalogConfig::default(),
            event_buffer: DEFAULT_EVENT_BUFFER,
            receipt_limit: DEFAULT_RECEIPTS,
        }
    }
}

#[derive(Debug)]
pub enum HostError {
    BindAddress(SocketAddr),
    PortInUse(SocketAddr),
    Io(io::Error),
    Catalog(CatalogError),
    State(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindAddress(addr) => write!(
                f,
                "refusing Web bind address {addr}; leg-web binds only 127.0.0.1"
            ),
            Self::PortInUse(addr) => write!(
                f,
                "{addr} is already in use; pass --bind 127.0.0.1:<port>, or 127.0.0.1:0 for any free port"
            ),
            Self::Io(error) => write!(f, "Web host I/O failed: {error}"),
            Self::Catalog(error) => write!(f, "could not open the shared session catalog: {error}"),
            Self::State(message) => write!(f, "Web host state is invalid: {message}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<io::Error> for HostError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<CatalogError> for HostError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

#[derive(Clone)]
pub struct Host {
    state: HostState,
    bind_addr: SocketAddr,
}

pub struct BoundHost {
    host: Host,
    listener: TcpListener,
    addr: SocketAddr,
}

impl Host {
    pub fn open(config: HostConfig) -> Result<Self, HostError> {
        validate_bind_address(config.bind_addr)?;
        if config.event_buffer == 0 || config.receipt_limit == 0 {
            return Err(HostError::State(
                "event buffer and receipt retention must be positive".into(),
            ));
        }
        let catalog = SessionCatalog::open(config.catalog)?;
        let host_lock = acquire_host_lock(catalog.state_dir())?;
        let token = random_hex(32)?;
        let durable =
            DurableStore::open(catalog.state_dir().join(STORE_NAME), config.receipt_limit)?;
        let runtime = Runtime::default();
        let (shutdown_tx, _) = broadcast::channel(1);
        let state = HostState {
            inner: Arc::new(HostInner {
                catalog,
                token,
                authority: Arc::new(Mutex::new(None)),
                durable,
                runtime: Mutex::new(runtime),
                acceptance_gates: Mutex::new(HashMap::new()),
                #[cfg(test)]
                test_hooks: TestHooks::default(),
                workers: Mutex::new(Vec::new()),
                shutdown_tx,
                shutting_down: AtomicBool::new(false),
                _host_lock: host_lock,
                event_buffer: config.event_buffer,
                receipt_limit: config.receipt_limit,
            }),
        };
        state.recover_after_restart()?;
        Ok(Self {
            state,
            bind_addr: config.bind_addr,
        })
    }

    pub async fn bind(self) -> Result<BoundHost, HostError> {
        let listener =
            TcpListener::bind(self.bind_addr)
                .await
                .map_err(|error| match error.kind() {
                    io::ErrorKind::AddrInUse => HostError::PortInUse(self.bind_addr),
                    _ => HostError::Io(error),
                })?;
        let addr = listener.local_addr()?;
        *lock(&self.state.inner.authority) = Some(authority_for(addr));
        Ok(BoundHost {
            host: self,
            listener,
            addr,
        })
    }
}

impl BoundHost {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Contains the one-time bootstrap token only in the URL fragment.
    pub fn launch_url(&self) -> String {
        format!(
            "http://{}/#{}",
            authority_for(self.addr),
            self.host.state.inner.token
        )
    }

    pub fn router(&self) -> Router {
        build_router(self.host.state.clone())
    }

    pub async fn serve<F>(self, shutdown: F) -> Result<(), HostError>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let router = self.router();
        let shutdown_state = self.host.state.clone();
        let shutdown_state_for_signal = shutdown_state.clone();
        let graceful_shutdown = async move {
            shutdown.await;
            shutdown_state_for_signal.shutdown();
        };
        let result = axum::serve(self.listener, router)
            .with_graceful_shutdown(graceful_shutdown)
            .await
            .map_err(HostError::Io);
        shutdown_state.shutdown();
        result
    }
}

#[derive(Clone)]
struct HostState {
    inner: Arc<HostInner>,
}

struct HostInner {
    catalog: SessionCatalog,
    token: String,
    authority: Arc<Mutex<Option<String>>>,
    durable: DurableStore,
    runtime: Mutex<Runtime>,
    acceptance_gates: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    #[cfg(test)]
    test_hooks: TestHooks,
    workers: Mutex<Vec<JoinHandle<()>>>,
    shutdown_tx: broadcast::Sender<()>,
    shutting_down: AtomicBool,
    // Holding the lock file keeps the single-host claim process-owned.
    _host_lock: File,
    event_buffer: usize,
    receipt_limit: usize,
}

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    pause_after_durable_receipt: Mutex<Option<AcceptancePause>>,
    gate_attempts: Mutex<Option<tokio::sync::mpsc::UnboundedSender<&'static str>>>,
}

#[cfg(test)]
struct AcceptancePause {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[derive(Default)]
struct Runtime {
    sessions: HashMap<String, SessionRuntime>,
    aliases: HashMap<String, String>,
    selected_by_tab: HashMap<String, String>,
}

struct SessionRuntime {
    events: VecDeque<HostEvent>,
    wake: broadcast::Sender<u64>,
    active: Option<ActiveRun>,
    live: Option<LiveSnapshot>,
}

struct ActiveRun {
    turn_id: String,
    stop: PendingStop,
    stop_requested: bool,
}

#[derive(Clone)]
struct PendingStop {
    inner: Arc<PendingStopInner>,
}

struct PendingStopInner {
    requested: AtomicBool,
    handle: Mutex<Option<TurnStopHandle>>,
}

impl PendingStop {
    fn new() -> Self {
        Self {
            inner: Arc::new(PendingStopInner {
                requested: AtomicBool::new(false),
                handle: Mutex::new(None),
            }),
        }
    }

    fn stop(&self) -> Result<(), ()> {
        self.inner.requested.store(true, Ordering::Release);
        if let Some(handle) = lock(&self.inner.handle).as_ref() {
            handle.stop().map_err(|_| ())?;
        }
        Ok(())
    }

    fn attach(&self, handle: TurnStopHandle) {
        *lock(&self.inner.handle) = Some(handle.clone());
        if self.inner.requested.load(Ordering::Acquire) {
            let _ = handle.stop();
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveSnapshot {
    turn_id: String,
    turn_index: Option<u64>,
    prompt: String,
    text: String,
    status: String,
    started_at_ms: u64,
    provider: Option<String>,
    model: Option<String>,
    active_tool: Option<Value>,
    tools: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostEvent {
    session_id: String,
    turn_id: String,
    cursor: u64,
    kind: String,
    data: Value,
}

type EventSubscription = (Vec<HostEvent>, Option<Value>, broadcast::Receiver<u64>, u64);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptStatus {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Stopped,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    request_id: u64,
    prompt_sha256: String,
    turn_id: String,
    status: ReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery_evidence: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionLedger {
    high_water: u64,
    cursor: u64,
    #[serde(default)]
    receipts: VecDeque<Receipt>,
}

#[derive(Debug, PartialEq)]
enum SubmissionDecision {
    New,
    Replay(Receipt),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmissionReject {
    Stale,
    Conflict,
    Unexpected,
    Exhausted,
}

#[derive(Clone)]
struct NewSubmission {
    requested_id: String,
    ledger_id: String,
    bound_session_id: Option<String>,
    request_id: u64,
    turn_id: String,
    prompt: String,
    stop: PendingStop,
}

struct SubmissionAcceptance {
    receipt: SubmissionReceipt,
}

fn submission_decision(
    ledger: Option<&SessionLedger>,
    request_id: u64,
    prompt_hash: &str,
) -> Result<SubmissionDecision, SubmissionReject> {
    let high_water = ledger.map(|ledger| ledger.high_water).unwrap_or(0);
    let Some(next_id) = high_water.checked_add(1) else {
        return Err(SubmissionReject::Exhausted);
    };
    if request_id < next_id {
        let receipt = ledger
            .and_then(|ledger| {
                ledger
                    .receipts
                    .iter()
                    .find(|receipt| receipt.request_id == request_id)
            })
            .ok_or(SubmissionReject::Stale)?;
        if !constant_time_eq(receipt.prompt_sha256.as_bytes(), prompt_hash.as_bytes()) {
            return Err(SubmissionReject::Conflict);
        }
        return Ok(SubmissionDecision::Replay(receipt.clone()));
    }
    if request_id != next_id {
        return Err(SubmissionReject::Unexpected);
    }
    Ok(SubmissionDecision::New)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableData {
    version: u32,
    #[serde(default)]
    sessions: HashMap<String, SessionLedger>,
    /// A new draft becomes a real leg session at the first turn_start event.
    #[serde(default)]
    aliases: HashMap<String, String>,
}

impl Default for DurableData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            sessions: HashMap::new(),
            aliases: HashMap::new(),
        }
    }
}

#[derive(Clone)]
struct DurableStore {
    path: PathBuf,
    data: Arc<Mutex<DurableData>>,
}

impl DurableStore {
    fn open(path: PathBuf, receipt_limit: usize) -> Result<Self, HostError> {
        let data = match fs::read(&path) {
            Ok(bytes) => {
                let data: DurableData = serde_json::from_slice(&bytes)
                    .map_err(|error| HostError::State(error.to_string()))?;
                if data.version != STORE_VERSION {
                    return Err(HostError::State(format!(
                        "unsupported state version {}",
                        data.version
                    )));
                }
                data
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => DurableData::default(),
            Err(error) => return Err(error.into()),
        };
        let store = Self {
            path,
            data: Arc::new(Mutex::new(data)),
        };
        store.transact(|data| {
            for ledger in data.sessions.values_mut() {
                while ledger.receipts.len() > receipt_limit {
                    ledger.receipts.pop_front();
                }
                for receipt in ledger.receipts.iter_mut().filter(|receipt| {
                    matches!(
                        receipt.status,
                        ReceiptStatus::Accepted | ReceiptStatus::Running
                    )
                }) {
                    receipt.status = ReceiptStatus::Incomplete;
                    receipt.recovery_evidence =
                        Some("host_restart_requires_explicit_submission".into());
                }
            }
            Ok(())
        })?;
        Ok(store)
    }

    fn transact<T>(
        &self,
        update: impl FnOnce(&mut DurableData) -> Result<T, HostError>,
    ) -> Result<T, HostError> {
        let mut data = lock(&self.data);
        let previous = data.clone();
        let result = update(&mut data)?;
        if let Err(error) = write_atomic(&self.path, &data) {
            *data = previous;
            return Err(error.into());
        }
        Ok(result)
    }

    fn read<T>(&self, read: impl FnOnce(&DurableData) -> T) -> T {
        read(&lock(&self.data))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameSession {
    name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetWorkspace {
    cwd: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectSession {
    session_id: String,
    tab_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitTurn {
    request_id: u64,
    prompt: String,
}

#[derive(Clone, Debug, Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: u64,
}

#[derive(Clone, Debug, Serialize)]
struct Snapshot {
    session: CatalogSession,
    turn_id: Option<String>,
    cursor: u64,
    next_request_id: Option<u64>,
    high_water: u64,
    last_submission: Option<Receipt>,
    active: Option<LiveSnapshot>,
    recovery_required: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SubmissionReceipt {
    request_id: u64,
    turn_id: String,
    session_id: String,
    status: ReceiptStatus,
    duplicate: bool,
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn api_error(status: StatusCode, message: &'static str) -> ApiError {
    ApiError(status, message)
}

fn build_router(state: HostState) -> Router {
    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/controller.js", get(controller_js))
        .route("/themes/registry.js", get(theme_registry_js))
        .route("/themes/default.js", get(default_theme_js))
        .route("/themes/default.html", get(default_theme_html))
        .route("/themes/default.css", get(default_theme_css))
        .route("/themes/shared-renderer.js", get(shared_renderer_js))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/select", post(select_session))
        .route("/api/sessions/{id}", get(get_session).patch(rename_session))
        .route("/api/sessions/{id}/workspace", put(set_workspace))
        .route("/api/sessions/{id}/submit", post(submit_turn))
        .route("/api/sessions/{id}/stop", post(stop_turn))
        .route("/api/sessions/{id}/snapshot", get(get_snapshot))
        .route("/api/sessions/{id}/events", get(events));
    #[cfg(feature = "browser-e2e-themes")]
    let router = router
        .route("/themes/fixture.js", get(fixture_theme_js))
        .route("/themes/fixture.html", get(fixture_theme_html))
        .route("/themes/fixture.css", get(fixture_theme_css));
    router
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard_request))
        .with_state(state)
}

async fn guard_request(State(state): State<HostState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let api = path.starts_with("/api/");
    let expected_authority = lock(&state.inner.authority).clone();
    let Some(expected_authority) = expected_authority else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "host_not_ready").into_response();
    };
    let host_matches = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(&expected_authority));
    let uri_authority_matches = request
        .uri()
        .authority()
        .is_none_or(|authority| authority.as_str().eq_ignore_ascii_case(&expected_authority));
    if !host_matches || !uri_authority_matches {
        return api_error(StatusCode::BAD_REQUEST, "invalid_host").into_response();
    }

    if api {
        let origin_matches = request
            .headers()
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|origin| origin == format!("http://{expected_authority}"));
        let origin_present = request.headers().contains_key(ORIGIN);
        let unsafe_method = !matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        );
        if (origin_present || unsafe_method) && !origin_matches {
            return api_error(StatusCode::FORBIDDEN, "invalid_origin").into_response();
        }
        if !authorization_matches(request.headers().get(AUTHORIZATION), &state.inner.token) {
            return api_error(StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    } else if !is_embedded_asset(&path) {
        return api_error(StatusCode::NOT_FOUND, "not_found").into_response();
    } else if request.method() != Method::GET {
        return api_error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed").into_response();
    }

    let mut response = next.run(request).await;
    if api && !response.status().is_success() && !response_is_json(&response) {
        let status = response.status();
        response = api_error(status, api_rejection_code(&path, status)).into_response();
    }
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if !api {
        response.headers_mut().insert(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
            ),
        );
    }
    response
}

fn response_is_json(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
}

fn api_rejection_code(path: &str, status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST if path.ends_with("/events") => "invalid_cursor",
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        StatusCode::UNPROCESSABLE_ENTITY => "invalid_request",
        status if status.is_server_error() => "internal_error",
        _ => "invalid_request",
    }
}

async fn index() -> Response {
    static_response("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    static_response("text/javascript; charset=utf-8", APP_JS)
}

async fn controller_js() -> Response {
    static_response("text/javascript; charset=utf-8", CONTROLLER_JS)
}

async fn theme_registry_js() -> Response {
    static_response("text/javascript; charset=utf-8", THEME_REGISTRY_JS)
}

async fn default_theme_js() -> Response {
    static_response("text/javascript; charset=utf-8", DEFAULT_THEME_JS)
}

async fn default_theme_html() -> Response {
    static_response("text/html; charset=utf-8", DEFAULT_THEME_HTML)
}

async fn default_theme_css() -> Response {
    static_response("text/css; charset=utf-8", DEFAULT_THEME_CSS)
}

async fn shared_renderer_js() -> Response {
    static_response("text/javascript; charset=utf-8", SHARED_RENDERER_JS)
}

#[cfg(feature = "browser-e2e-themes")]
async fn fixture_theme_js() -> Response {
    static_response("text/javascript; charset=utf-8", FIXTURE_THEME_JS)
}

#[cfg(feature = "browser-e2e-themes")]
async fn fixture_theme_html() -> Response {
    static_response("text/html; charset=utf-8", FIXTURE_THEME_HTML)
}

#[cfg(feature = "browser-e2e-themes")]
async fn fixture_theme_css() -> Response {
    static_response("text/css; charset=utf-8", FIXTURE_THEME_CSS)
}

fn is_embedded_asset(path: &str) -> bool {
    matches!(
        path,
        "/" | "/app.js"
            | "/controller.js"
            | "/themes/registry.js"
            | "/themes/default.js"
            | "/themes/default.html"
            | "/themes/default.css"
            | "/themes/shared-renderer.js"
    ) || (cfg!(feature = "browser-e2e-themes")
        && matches!(
            path,
            "/themes/fixture.js" | "/themes/fixture.html" | "/themes/fixture.css"
        ))
}

fn static_response(content_type: &'static str, content: &'static str) -> Response {
    let mut response = Response::new(Body::from(content));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

async fn list_sessions(State(state): State<HostState>) -> Result<Json<Value>, ApiError> {
    let sessions = state
        .inner
        .catalog
        .list()
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "catalog_unavailable"))?;
    Ok(Json(json!({"sessions": sessions})))
}

async fn create_session(
    State(state): State<HostState>,
    Json(body): Json<CreateSession>,
) -> Result<(StatusCode, Json<CatalogSession>), ApiError> {
    if let Some(name) = body.name.as_deref() {
        validate_name(name)?;
    }
    if body.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "workspace_must_be_absolute",
        ));
    }
    let session = state
        .inner
        .catalog
        .create_draft(SessionInterface::Web, body.name, body.cwd.as_deref())
        .map_err(map_catalog_error)?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn select_session(
    State(state): State<HostState>,
    Json(body): Json<SelectSession>,
) -> Result<Json<CatalogSession>, ApiError> {
    validate_tab_id(&body.tab_id)?;
    let session = state.catalog_session(&body.session_id)?;
    let mut runtime = lock(&state.inner.runtime);
    if !runtime.selected_by_tab.contains_key(&body.tab_id) && runtime.selected_by_tab.len() >= 256 {
        return Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "tab_limit_reached",
        ));
    }
    runtime.selected_by_tab.insert(body.tab_id, body.session_id);
    Ok(Json(session))
}

async fn get_session(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
) -> Result<Json<CatalogSession>, ApiError> {
    Ok(Json(state.catalog_session(&id)?))
}

async fn rename_session(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
    Json(body): Json<RenameSession>,
) -> Result<StatusCode, ApiError> {
    state.catalog_session(&id)?;
    if let Some(name) = body.name.as_deref() {
        validate_name(name)?;
    }
    state
        .inner
        .catalog
        .rename(&id, body.name)
        .map_err(map_catalog_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_workspace(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
    Json(body): Json<SetWorkspace>,
) -> Result<StatusCode, ApiError> {
    if !valid_session_id(&id) {
        return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
    }
    if !body.cwd.is_absolute() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "workspace_must_be_absolute",
        ));
    }
    let ledger_id = state.ledger_id(&id);
    #[cfg(test)]
    state.note_gate_attempt("workspace");
    let gate = state.acceptance_gate(&ledger_id);
    let _gate = gate.lock().await;
    if state.runtime_is_busy(&ledger_id) {
        return Err(api_error(StatusCode::CONFLICT, "session_busy"));
    }
    let blocking_state = state.clone();
    tokio::task::spawn_blocking(move || {
        blocking_state.catalog_session(&id)?;
        blocking_state
            .inner
            .catalog
            .set_workspace(&id, &body.cwd)
            .map_err(map_catalog_error)
    })
    .await
    .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "workspace_worker_failed"))??;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_snapshot(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
) -> Result<Json<Snapshot>, ApiError> {
    Ok(Json(state.snapshot(&id)?))
}

async fn submit_turn(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
    Json(body): Json<SubmitTurn>,
) -> Result<(StatusCode, Json<SubmissionReceipt>), ApiError> {
    if body.prompt.trim().is_empty() {
        return Err(api_error(StatusCode::BAD_REQUEST, "prompt_is_blank"));
    }
    if !valid_session_id(&id) {
        return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
    }
    if state.inner.shutting_down.load(Ordering::Acquire) {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_shutting_down",
        ));
    }
    let ledger_id = state.ledger_id(&id);
    #[cfg(test)]
    state.note_gate_attempt("submit");
    let gate = state.acceptance_gate(&ledger_id);
    let gate_guard = gate.lock_owned().await;
    if state.inner.shutting_down.load(Ordering::Acquire) {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_shutting_down",
        ));
    }
    let blocking_state = state.clone();
    let acceptance = tokio::task::spawn_blocking(move || {
        // The blocking task can outlive this HTTP future, so it owns the gate.
        let _gate_guard = gate_guard;
        blocking_state.accept_submission(&id, body)
    })
    .await
    .map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "submission_worker_failed",
        )
    })??;
    let response = acceptance.receipt;
    let status = if response.duplicate {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(response)))
}

async fn stop_turn(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
) -> Result<Json<Value>, ApiError> {
    if !valid_session_id(&id) {
        return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
    }
    let ledger_id = state.ledger_id(&id);
    #[cfg(test)]
    state.note_gate_attempt("stop");
    let gate = state.acceptance_gate(&ledger_id);
    let _gate = gate.lock().await;
    let stop = {
        let mut runtime = lock(&state.inner.runtime);
        let runtime_key = runtime_id(&runtime, &ledger_id);
        if let Some(entry) = runtime.sessions.get_mut(&runtime_key)
            && let Some(active) = entry.active.as_mut()
        {
            if active.stop_requested {
                return Ok(Json(json!({
                    "session_id": id,
                    "turn_id": active.turn_id,
                    "status": "stop_requested",
                })));
            }
            active.stop_requested = true;
            let turn_id = active.turn_id.clone();
            let stop = active.stop.clone();
            if let Some(live) = entry.live.as_mut() {
                live.status = "stopping".into();
            }
            Some((turn_id, stop))
        } else {
            None
        }
    };
    if let Some((turn_id, stop)) = stop {
        stop.stop()
            .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "stop_unavailable"))?;
        return Ok(Json(json!({
            "session_id": id,
            "turn_id": turn_id,
            "status": "stop_requested",
        })));
    }

    let session = state.catalog_session_or_bound_alias(&id)?;
    let latest = state.latest_receipt(&ledger_id);
    Ok(Json(json!({
        "session_id": id,
        "status": latest.map(|r| format!("{:?}", r.status).to_ascii_lowercase()).unwrap_or_else(|| "idle".into()),
        "run_state": session.run_state,
    })))
}

async fn events(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError>
{
    state.catalog_session_or_bound_alias(&id)?;
    let ledger_id = state.ledger_id(&id);
    let (initial, reset_snapshot, mut receiver, mut last_cursor) =
        state.subscribe(&id, &ledger_id, query.after)?;
    let mut shutdown_receiver = state.inner.shutdown_tx.subscribe();
    let stream = async_stream::stream! {
        if let Some(snapshot) = reset_snapshot {
            let cursor = snapshot.get("cursor").and_then(Value::as_u64).unwrap_or(last_cursor);
            last_cursor = cursor;
            yield Ok(Event::default().event("reset").id(cursor.to_string()).json_data(snapshot).expect("JSON event"));
        } else {
            for event in initial {
                last_cursor = event.cursor;
                yield Ok(Event::default().event("update").id(event.cursor.to_string()).json_data(event).expect("JSON event"));
            }
        }
        loop {
            tokio::select! {
                _ = shutdown_receiver.recv() => break,
                received = receiver.recv() => match received {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    match state.catch_up(&id, &ledger_id, last_cursor) {
                        Ok((_events, Some(snapshot))) => {
                            let cursor = snapshot.get("cursor").and_then(Value::as_u64).unwrap_or(last_cursor);
                            last_cursor = cursor;
                            yield Ok(Event::default().event("reset").id(cursor.to_string()).json_data(snapshot).expect("JSON event"));
                        }
                        Ok((events, None)) => {
                            for event in events {
                                last_cursor = event.cursor;
                                yield Ok(Event::default().event("update").id(event.cursor.to_string()).json_data(event).expect("JSON event"));
                            }
                        }
                        Err(_) => break,
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

impl HostState {
    #[cfg(test)]
    fn note_gate_attempt(&self, operation: &'static str) {
        if let Some(sender) = lock(&self.inner.test_hooks.gate_attempts).as_ref() {
            let _ = sender.send(operation);
        }
    }

    #[cfg(test)]
    fn pause_after_durable_receipt(&self) {
        if let Some(pause) = lock(&self.inner.test_hooks.pause_after_durable_receipt).take() {
            let _ = pause.entered.send(());
            let _ = lock(&pause.release).recv();
        }
    }

    fn acceptance_gate(&self, ledger_id: &str) -> Arc<AsyncMutex<()>> {
        let mut gates = lock(&self.inner.acceptance_gates);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(ledger_id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(AsyncMutex::new(()));
        gates.insert(ledger_id.to_string(), Arc::downgrade(&gate));
        gate
    }

    fn catalog_session(&self, id: &str) -> Result<CatalogSession, ApiError> {
        if !valid_session_id(id) {
            return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
        }
        self.inner.catalog.get(id).map_err(map_catalog_error)
    }

    fn catalog_session_or_bound_alias(&self, id: &str) -> Result<CatalogSession, ApiError> {
        if !valid_session_id(id) {
            return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
        }
        match self.inner.catalog.get(id) {
            Ok(session) => Ok(session),
            Err(CatalogError::NotFound(_)) => {
                let bound_id = self.inner.durable.read(|data| {
                    data.aliases.iter().find_map(|(actual, ledger_id)| {
                        (ledger_id == id && actual != id).then(|| actual.clone())
                    })
                });
                match bound_id {
                    Some(bound_id) => self.inner.catalog.get(&bound_id).map_err(map_catalog_error),
                    None => Err(api_error(StatusCode::NOT_FOUND, "session_not_found")),
                }
            }
            Err(error) => Err(map_catalog_error(error)),
        }
    }

    fn ledger_id(&self, id: &str) -> String {
        self.inner.durable.read(|data| {
            data.aliases
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_string())
        })
    }

    fn latest_receipt(&self, id: &str) -> Option<Receipt> {
        self.inner.durable.read(|data| {
            data.sessions
                .get(id)
                .and_then(|s| s.receipts.back())
                .cloned()
        })
    }

    fn snapshot(&self, id: &str) -> Result<Snapshot, ApiError> {
        let session = self.catalog_session_or_bound_alias(id)?;
        let ledger_id = self.ledger_id(id);
        let (high_water, cursor, last_submission, runtime_key_hint) =
            self.inner.durable.read(|data| {
                let ledger = data.sessions.get(&ledger_id);
                (
                    ledger.map(|l| l.high_water).unwrap_or(0),
                    ledger.map(|l| l.cursor).unwrap_or(0),
                    ledger.and_then(|l| l.receipts.back()).cloned(),
                    data.aliases
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| id.to_string()),
                )
            });
        let runtime = lock(&self.inner.runtime);
        let runtime_key = runtime_id(&runtime, &runtime_key_hint);
        let active = runtime
            .sessions
            .get(&runtime_key)
            .and_then(|entry| entry.live.clone());
        let latest_event_turn = runtime
            .sessions
            .get(&runtime_key)
            .and_then(|entry| entry.events.back())
            .map(|event| event.turn_id.clone());
        let recovery_required = last_submission.as_ref().is_some_and(|receipt| {
            matches!(receipt.status, ReceiptStatus::Incomplete)
                || (matches!(
                    receipt.status,
                    ReceiptStatus::Accepted | ReceiptStatus::Running
                ) && active.is_none())
        }) || matches!(session.run_state, CatalogRunState::Unknown);
        let turn_id = active
            .as_ref()
            .map(|live| live.turn_id.clone())
            .or_else(|| {
                last_submission
                    .as_ref()
                    .map(|receipt| receipt.turn_id.clone())
            })
            .or(latest_event_turn);
        Ok(Snapshot {
            session,
            turn_id,
            cursor,
            next_request_id: high_water.checked_add(1),
            high_water,
            last_submission,
            active,
            recovery_required,
        })
    }

    fn accept_submission(
        &self,
        id: &str,
        body: SubmitTurn,
    ) -> Result<SubmissionAcceptance, ApiError> {
        if !valid_session_id(id) {
            return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
        }
        let ledger_id = self.ledger_id(id);
        let prompt_hash = hex(&Sha256::digest(body.prompt.as_bytes()));
        let decision = self.inner.durable.read(|data| {
            submission_decision(data.sessions.get(&ledger_id), body.request_id, &prompt_hash)
        });
        match decision {
            Ok(SubmissionDecision::Replay(previous)) => {
                return Ok(SubmissionAcceptance {
                    receipt: SubmissionReceipt {
                        request_id: previous.request_id,
                        turn_id: previous.turn_id,
                        session_id: previous.session_id.unwrap_or_else(|| id.to_string()),
                        status: previous.status,
                        duplicate: true,
                    },
                });
            }
            Ok(SubmissionDecision::New) => {}
            Err(SubmissionReject::Stale) => {
                return Err(api_error(StatusCode::CONFLICT, "submission_id_stale"));
            }
            Err(SubmissionReject::Conflict) => {
                return Err(api_error(StatusCode::CONFLICT, "submission_id_conflict"));
            }
            Err(SubmissionReject::Unexpected) => {
                return Err(api_error(StatusCode::CONFLICT, "submission_id_unexpected"));
            }
            Err(SubmissionReject::Exhausted) => {
                return Err(api_error(StatusCode::CONFLICT, "submission_id_exhausted"));
            }
        }
        if self.runtime_is_busy(&ledger_id) {
            return Err(api_error(StatusCode::CONFLICT, "session_busy"));
        }
        let current = self.catalog_session(id)?;
        if current.run_state != CatalogRunState::Idle {
            return Err(api_error(StatusCode::CONFLICT, "session_busy"));
        }
        let bound_session_id = if id.starts_with("draft-") {
            let mut available = None;
            for _ in 0..128 {
                let candidate = self
                    .inner
                    .catalog
                    .allocate_new_session_id()
                    .map_err(map_catalog_error)?;
                let reserved = self.inner.durable.read(|data| {
                    data.aliases.contains_key(&candidate) || data.sessions.contains_key(&candidate)
                });
                if !reserved {
                    available = Some(candidate);
                    break;
                }
            }
            Some(available.ok_or(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session_id_unavailable",
            ))?)
        } else {
            None
        };
        let turn_id = random_hex(16)
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "random_unavailable"))?;
        let accepted = Receipt {
            request_id: body.request_id,
            prompt_sha256: prompt_hash.clone(),
            turn_id: turn_id.clone(),
            status: ReceiptStatus::Accepted,
            session_id: Some(id.to_string()),
            outcome: None,
            recovery_evidence: None,
        };
        self.inner
            .durable
            .transact(|data| {
                if !matches!(
                    submission_decision(
                        data.sessions.get(&ledger_id),
                        body.request_id,
                        &prompt_hash
                    ),
                    Ok(SubmissionDecision::New)
                ) {
                    return Err(HostError::State(
                        "submission changed before durable acceptance".into(),
                    ));
                }
                if let Some(bound_id) = &bound_session_id {
                    if data.aliases.contains_key(bound_id) || data.sessions.contains_key(bound_id) {
                        return Err(HostError::State(
                            "preallocated session id was already reserved".into(),
                        ));
                    }
                    data.aliases.insert(bound_id.clone(), ledger_id.clone());
                }
                let ledger = data.sessions.entry(ledger_id.clone()).or_default();
                ledger.high_water = body.request_id;
                ledger.receipts.push_back(accepted.clone());
                while ledger.receipts.len() > self.inner.receipt_limit {
                    ledger.receipts.pop_front();
                }
                Ok(())
            })
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))?;
        #[cfg(test)]
        self.pause_after_durable_receipt();
        let pending_stop = PendingStop::new();
        if let Some(bound_id) = &bound_session_id {
            lock(&self.inner.runtime)
                .aliases
                .insert(bound_id.clone(), ledger_id.clone());
        }
        self.ensure_runtime(&ledger_id);
        {
            let mut runtime = lock(&self.inner.runtime);
            let key = runtime_id(&runtime, &ledger_id);
            let entry = runtime
                .sessions
                .entry(key)
                .or_insert_with(SessionRuntime::new);
            entry.active = Some(ActiveRun {
                turn_id: turn_id.clone(),
                stop: pending_stop.clone(),
                stop_requested: false,
            });
            entry.live = Some(LiveSnapshot {
                turn_id: turn_id.clone(),
                turn_index: None,
                prompt: body.prompt.clone(),
                text: String::new(),
                status: "starting".into(),
                started_at_ms: now_millis(),
                provider: None,
                model: None,
                active_tool: None,
                tools: Vec::new(),
            });
        }
        let _ = self.append_event(
            &ledger_id,
            id,
            &turn_id,
            "accepted",
            json!({"request_id": body.request_id}),
        );
        let _ = self.finish_receipt(
            &ledger_id,
            body.request_id,
            ReceiptStatus::Running,
            Some(id.to_string()),
            json!({"status": "running"}),
        );
        let submission = NewSubmission {
            requested_id: id.to_string(),
            ledger_id: ledger_id.clone(),
            bound_session_id,
            request_id: body.request_id,
            turn_id: turn_id.clone(),
            prompt: body.prompt,
            stop: pending_stop,
        };
        let owner_submission = submission.clone();
        let state = self.clone();
        let worker = thread::Builder::new()
            .name("leg-web-turn-owner".into())
            .spawn(move || state.run_submission_owner(owner_submission));
        let worker = match worker {
            Ok(worker) => worker,
            Err(_) => {
                let incomplete = json!({"status": "incomplete"});
                self.release_unbound_session_alias(&submission);
                self.finish_submission(
                    &submission,
                    id,
                    ReceiptStatus::Incomplete,
                    incomplete.clone(),
                );
                self.clear_active_run(&submission);
                let _ = self.append_event(&ledger_id, id, &turn_id, "outcome", incomplete);
                return Ok(SubmissionAcceptance {
                    receipt: SubmissionReceipt {
                        request_id: body.request_id,
                        turn_id,
                        session_id: id.to_string(),
                        status: ReceiptStatus::Incomplete,
                        duplicate: false,
                    },
                });
            }
        };
        self.reap_finished_workers();
        lock(&self.inner.workers).push(worker);
        Ok(SubmissionAcceptance {
            receipt: SubmissionReceipt {
                request_id: body.request_id,
                turn_id,
                session_id: id.to_string(),
                status: ReceiptStatus::Running,
                duplicate: false,
            },
        })
    }

    fn runtime_is_busy(&self, ledger_id: &str) -> bool {
        let runtime = lock(&self.inner.runtime);
        let key = runtime_id(&runtime, ledger_id);
        runtime
            .sessions
            .get(&key)
            .is_some_and(|entry| entry.active.is_some())
    }

    fn run_submission_owner(&self, submission: NewSubmission) {
        let mut bound_id = submission.requested_id.clone();
        let mut session_bound = false;
        let mut turn = match self.start_turn(
            &submission.requested_id,
            &submission.prompt,
            submission.bound_session_id.as_deref(),
        ) {
            Ok(turn) => turn,
            Err(_) => {
                let outcome = json!({"status": "incomplete", "reason": "startup_failed"});
                self.release_unbound_session_alias(&submission);
                self.finish_submission(
                    &submission,
                    &submission.requested_id,
                    ReceiptStatus::Incomplete,
                    outcome.clone(),
                );
                self.clear_active_run(&submission);
                let _ = self.append_event(
                    &submission.ledger_id,
                    &bound_id,
                    &submission.turn_id,
                    "outcome",
                    outcome,
                );
                return;
            }
        };
        submission.stop.attach(turn.stop_handle());
        let mut binding_failed = false;
        while let Ok(Some(event)) = turn.observe() {
            if let StreamEvent::TurnStart {
                session_id: Some(session_id),
                ..
            } = &event
            {
                if submission
                    .bound_session_id
                    .as_deref()
                    .is_some_and(|expected| expected != session_id)
                {
                    binding_failed = true;
                    let _ = submission.stop.stop();
                } else if submission.requested_id.starts_with("draft-") {
                    if self
                        .bind_alias(&submission.requested_id, session_id, submission.request_id)
                        .is_err()
                    {
                        binding_failed = true;
                        let _ = submission.stop.stop();
                    } else {
                        bound_id = session_id.clone();
                        session_bound = true;
                    }
                } else {
                    bound_id = session_id.clone();
                    session_bound = true;
                }
            }
            let _ = self.record_stream_event(
                &submission.ledger_id,
                &bound_id,
                &submission.turn_id,
                event,
            );
        }
        // EOF is not success; wait() supplies the driver's terminal result.
        let (status, outcome) = match turn.wait() {
            Ok(outcome) => outcome_summary(&outcome),
            Err(_) => (ReceiptStatus::Incomplete, json!({"status": "incomplete"})),
        };
        let (status, outcome) = if binding_failed {
            (
                ReceiptStatus::Incomplete,
                json!({"status": "incomplete", "reason": "session_binding_failed"}),
            )
        } else {
            (status, outcome)
        };
        if !session_bound {
            self.release_unbound_session_alias(&submission);
        }
        self.finish_submission(&submission, &bound_id, status, outcome.clone());
        self.clear_active_run(&submission);
        let _ = self.append_event(
            &submission.ledger_id,
            &bound_id,
            &submission.turn_id,
            "outcome",
            outcome,
        );
    }

    fn finish_submission(
        &self,
        submission: &NewSubmission,
        session_id: &str,
        status: ReceiptStatus,
        outcome: Value,
    ) {
        let _ = self.finish_receipt(
            &submission.ledger_id,
            submission.request_id,
            status,
            Some(session_id.to_string()),
            outcome,
        );
    }

    fn release_unbound_session_alias(&self, submission: &NewSubmission) {
        let Some(session_id) = &submission.bound_session_id else {
            return;
        };
        // A failed stream can leave a native trail before it emits turn_start.
        // The catalog may adopt that exact trail during recovery, so keep the
        // submitted draft ID resolvable through its reserved session alias.
        if self.inner.catalog.get(session_id).is_ok() {
            return;
        }
        let _ = self.inner.durable.transact(|data| {
            if data.aliases.get(session_id) == Some(&submission.ledger_id) {
                data.aliases.remove(session_id);
            }
            Ok(())
        });
        let mut runtime = lock(&self.inner.runtime);
        if runtime.aliases.get(session_id) == Some(&submission.ledger_id) {
            runtime.aliases.remove(session_id);
        }
    }

    fn clear_active_run(&self, submission: &NewSubmission) {
        let mut runtime = lock(&self.inner.runtime);
        let key = runtime_id(&runtime, &submission.ledger_id);
        if let Some(entry) = runtime.sessions.get_mut(&key)
            && entry
                .active
                .as_ref()
                .is_some_and(|active| active.turn_id == submission.turn_id)
        {
            entry.active = None;
            entry.live = None;
        }
    }

    fn start_turn(
        &self,
        id: &str,
        prompt: &str,
        bound_session_id: Option<&str>,
    ) -> Result<CatalogTurn, CatalogError> {
        if id.starts_with("draft-") {
            let bound_session_id = bound_session_id.ok_or_else(|| {
                CatalogError::Catalog("draft submission has no reserved native id".into())
            })?;
            return self.inner.catalog.start_new_with_id(
                id,
                bound_session_id.to_string(),
                SessionInterface::Web,
                prompt.to_string(),
            );
        }
        if let Ok(intent) = self.inner.catalog.prepare_retry(id)
            && intent.prompt() == prompt
        {
            return self
                .inner
                .catalog
                .confirm_retry(&intent, SessionInterface::Web);
        }
        self.inner
            .catalog
            .start_existing(id, SessionInterface::Web, prompt.to_string())
    }

    fn reap_finished_workers(&self) {
        let finished = {
            let mut workers = lock(&self.inner.workers);
            let mut finished = Vec::new();
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    finished.push(workers.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        };
        for worker in finished {
            let _ = worker.join();
        }
    }

    fn ensure_runtime(&self, id: &str) {
        let mut runtime = lock(&self.inner.runtime);
        runtime
            .sessions
            .entry(id.to_string())
            .or_insert_with(SessionRuntime::new);
    }

    fn bind_alias(
        &self,
        draft_id: &str,
        session_id: &str,
        request_id: u64,
    ) -> Result<(), ApiError> {
        self.inner
            .durable
            .transact(|data| {
                if data
                    .aliases
                    .get(session_id)
                    .is_some_and(|existing| existing != draft_id)
                {
                    return Err(HostError::State(
                        "native session id was reserved by another ledger".into(),
                    ));
                }
                data.aliases
                    .insert(session_id.to_string(), draft_id.to_string());
                if let Some(receipt) = data.sessions.get_mut(draft_id).and_then(|ledger| {
                    ledger
                        .receipts
                        .iter_mut()
                        .find(|receipt| receipt.request_id == request_id)
                }) {
                    receipt.session_id = Some(session_id.to_string());
                }
                Ok(())
            })
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))?;
        let mut runtime = lock(&self.inner.runtime);
        runtime
            .aliases
            .insert(session_id.to_string(), draft_id.to_string());
        Ok(())
    }

    fn finish_receipt(
        &self,
        ledger_id: &str,
        request_id: u64,
        status: ReceiptStatus,
        session_id: Option<String>,
        outcome: Value,
    ) -> Result<(), ApiError> {
        self.inner
            .durable
            .transact(|data| {
                if let Some(receipt) = data.sessions.get_mut(ledger_id).and_then(|ledger| {
                    ledger
                        .receipts
                        .iter_mut()
                        .find(|r| r.request_id == request_id)
                }) {
                    receipt.status = status;
                    receipt.session_id = session_id;
                    receipt.outcome = Some(outcome);
                    receipt.recovery_evidence = None;
                }
                Ok(())
            })
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))
    }

    fn append_event(
        &self,
        ledger_id: &str,
        session_id: &str,
        turn_id: &str,
        kind: &str,
        data: Value,
    ) -> Result<HostEvent, ApiError> {
        let cursor = self
            .inner
            .durable
            .transact(|data| {
                let ledger = data.sessions.entry(ledger_id.to_string()).or_default();
                ledger.cursor = ledger
                    .cursor
                    .checked_add(1)
                    .ok_or_else(|| HostError::State("event cursor exhausted".into()))?;
                Ok(ledger.cursor)
            })
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))?;
        let event = HostEvent {
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            cursor,
            kind: kind.to_string(),
            data,
        };
        let mut runtime = lock(&self.inner.runtime);
        let key = runtime_id(&runtime, ledger_id);
        let entry = runtime
            .sessions
            .entry(key)
            .or_insert_with(SessionRuntime::new);
        entry.events.push_back(event.clone());
        while entry.events.len() > self.inner.event_buffer {
            entry.events.pop_front();
        }
        let _ = entry.wake.send(cursor);
        Ok(event)
    }

    fn record_stream_event(
        &self,
        ledger_id: &str,
        session_id: &str,
        turn_id: &str,
        event: StreamEvent,
    ) -> Result<(), ApiError> {
        let observed_at_ms = matches!(
            &event,
            StreamEvent::ToolCall { .. } | StreamEvent::ToolResult { .. }
        )
        .then(now_millis);
        let mut value = stream_event_value(&event);
        if let Some(observed_at_ms) = observed_at_ms {
            value["observed_at_ms"] = json!(observed_at_ms);
        }
        let active_turn_index = {
            let mut runtime = lock(&self.inner.runtime);
            let key = runtime_id(&runtime, ledger_id);
            let entry = runtime
                .sessions
                .entry(key)
                .or_insert_with(SessionRuntime::new);
            if let Some(live) = entry.live.as_mut() {
                if live.status != "stopping" {
                    live.status = "running".into();
                }
                match &event {
                    StreamEvent::TurnStart {
                        provider,
                        model,
                        turn_index,
                        ..
                    } => {
                        live.provider = Some(provider.clone());
                        live.model = Some(model.clone());
                        live.turn_index = *turn_index;
                    }
                    StreamEvent::TextDelta { text, .. } => live.text.push_str(text),
                    StreamEvent::ToolCall {
                        tool_use_id,
                        tool_name,
                        input,
                        ..
                    } => {
                        let tool = json!({
                            "tool_use_id": tool_use_id,
                            "tool_name": tool_name,
                            "input": input,
                            "status": "running",
                            "call_observed_at_ms": observed_at_ms,
                        });
                        live.active_tool = Some(tool.clone());
                        live.tools.push(tool);
                    }
                    StreamEvent::ToolResult {
                        tool_use_id,
                        status,
                        output,
                        ..
                    } => {
                        if let Some(tool) = live.tools.iter_mut().rev().find(|item| {
                            item.get("tool_use_id").and_then(Value::as_str) == Some(tool_use_id)
                        }) {
                            tool["status"] = Value::String(status.clone());
                            tool["output"] = output.clone();
                            tool["result_observed_at_ms"] = json!(observed_at_ms);
                        }
                        live.active_tool = None;
                    }
                    _ => {}
                }
            }
            entry.live.as_ref().and_then(|live| live.turn_index)
        };
        if let StreamEvent::TurnStart {
            provider,
            model,
            turn_index: Some(turn_index),
            ..
        } = &event
        {
            let mut metadata = self
                .inner
                .catalog
                .get(session_id)
                .ok()
                .and_then(|session| session.display.get("web.turn_metadata").cloned())
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            metadata[turn_index.to_string()] = json!({
                "provider": provider,
                "model": model,
                "started_at_ms": now_millis(),
            });
            let _ = self.inner.catalog.save_display_metadata(
                session_id,
                "web.turn_metadata".into(),
                metadata,
            );
        }
        match &event {
            StreamEvent::ToolCall { tool_use_id, .. } => {
                if let (Some(turn_index), Some(observed_at_ms)) =
                    (active_turn_index, observed_at_ms)
                {
                    save_tool_observation(
                        &self.inner.catalog,
                        session_id,
                        turn_index,
                        tool_use_id,
                        "call_observed_at_ms",
                        observed_at_ms,
                    );
                }
            }
            StreamEvent::ToolResult { tool_use_id, .. } => {
                if let (Some(turn_index), Some(observed_at_ms)) =
                    (active_turn_index, observed_at_ms)
                {
                    save_tool_observation(
                        &self.inner.catalog,
                        session_id,
                        turn_index,
                        tool_use_id,
                        "result_observed_at_ms",
                        observed_at_ms,
                    );
                }
            }
            StreamEvent::TurnEnd {
                capped,
                turn_index: Some(turn_index),
                ..
            } => {
                let mut metadata = self
                    .inner
                    .catalog
                    .get(session_id)
                    .ok()
                    .and_then(|session| session.display.get("web.turn_metadata").cloned())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                let entry = metadata
                    .as_object_mut()
                    .expect("turn metadata is an object")
                    .entry(turn_index.to_string())
                    .or_insert_with(|| json!({}));
                if !entry.is_object() {
                    *entry = json!({});
                }
                entry["capped"] = json!(capped);
                let _ = self.inner.catalog.save_display_metadata(
                    session_id,
                    "web.turn_metadata".into(),
                    metadata,
                );
            }
            _ => {}
        }
        self.append_event(ledger_id, session_id, turn_id, "stream", value)?;
        Ok(())
    }

    fn subscribe(
        &self,
        id: &str,
        ledger_id: &str,
        after: u64,
    ) -> Result<EventSubscription, ApiError> {
        self.ensure_runtime(ledger_id);
        let runtime_id_value;
        let receiver;
        {
            let runtime = lock(&self.inner.runtime);
            runtime_id_value = runtime_id(&runtime, ledger_id);
            receiver = runtime
                .sessions
                .get(&runtime_id_value)
                .expect("runtime created")
                .wake
                .subscribe();
        }
        let (events, reset) = self.catch_up(id, ledger_id, after)?;
        let cursor = if let Some(snapshot) = &reset {
            snapshot
                .get("cursor")
                .and_then(Value::as_u64)
                .unwrap_or(after)
        } else {
            events.last().map(|e| e.cursor).unwrap_or(after)
        };
        Ok((events, reset, receiver, cursor))
    }

    fn catch_up(
        &self,
        id: &str,
        ledger_id: &str,
        after: u64,
    ) -> Result<(Vec<HostEvent>, Option<Value>), ApiError> {
        let high_water = self
            .inner
            .durable
            .read(|data| data.sessions.get(ledger_id).map(|l| l.cursor).unwrap_or(0));
        if after > high_water {
            return Err(api_error(StatusCode::BAD_REQUEST, "cursor_ahead_of_host"));
        }
        let runtime = lock(&self.inner.runtime);
        let key = runtime_id(&runtime, ledger_id);
        let entry = runtime.sessions.get(&key);
        let events: Vec<HostEvent> = entry
            .map(|entry| {
                entry
                    .events
                    .iter()
                    .filter(|event| event.cursor > after)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let oldest = entry.and_then(|entry| entry.events.front().map(|event| event.cursor));
        let expired = (after < high_water && oldest.is_none())
            || oldest.is_some_and(|oldest| after.saturating_add(1) < oldest);
        drop(runtime);
        if expired {
            let mut snapshot = serde_json::to_value(self.snapshot(id)?).map_err(|_| {
                api_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot_unavailable")
            })?;
            snapshot["cursor"] = json!(high_water);
            Ok((Vec::new(), Some(snapshot)))
        } else {
            Ok((events, None))
        }
    }

    fn recover_after_restart(&self) -> Result<(), HostError> {
        let ids = self
            .inner
            .durable
            .read(|data| data.sessions.keys().cloned().collect::<Vec<_>>());
        for id in ids {
            let active = self.inner.durable.read(|data| {
                data.sessions
                    .get(&id)
                    .and_then(|ledger| ledger.receipts.back())
                    .is_some_and(|receipt| matches!(receipt.status, ReceiptStatus::Incomplete))
            });
            if !active {
                continue;
            }
            let session_id = if id.starts_with("draft-") {
                self.recover_draft_alias(&id)?
            } else {
                id.clone()
            };
            let evidence = self
                .inner
                .catalog
                .get(&session_id)
                .ok()
                .map(|session| match session.run_state {
                    CatalogRunState::Active => "driver_reports_owned_run_active",
                    CatalogRunState::Idle => "driver_reports_idle_after_restart",
                    CatalogRunState::Unknown => "driver_ownership_unknown",
                })
                .unwrap_or("catalog_has_no_bound_session");
            self.inner.durable.transact(|data| {
                if let Some(receipt) = data
                    .sessions
                    .get_mut(&id)
                    .and_then(|ledger| ledger.receipts.back_mut())
                    && matches!(receipt.status, ReceiptStatus::Incomplete)
                {
                    receipt.recovery_evidence = Some(evidence.into());
                }
                Ok(())
            })?;
            self.ensure_runtime(&id);
            let turn_id = self
                .latest_receipt(&id)
                .map(|receipt| receipt.turn_id)
                .unwrap_or_default();
            let _ = self.append_event(
                &id,
                &session_id,
                &turn_id,
                "recovery",
                json!({"status": "incomplete", "evidence": evidence}),
            );
        }
        Ok(())
    }

    fn recover_draft_alias(&self, draft_id: &str) -> Result<String, HostError> {
        self.inner.catalog.recover_pending_new_session(draft_id)?;
        // A reserved alias is confirmed only after the catalog moves the draft record.
        let draft_exists = match self.inner.catalog.get(draft_id) {
            Ok(_) => true,
            Err(CatalogError::NotFound(_)) => false,
            Err(error) => return Err(error.into()),
        };
        let reserved_ids = self.inner.durable.read(|data| {
            data.aliases
                .iter()
                .filter(|&(_, ledger_id)| ledger_id == draft_id)
                .map(|(actual, _)| actual.clone())
                .collect::<Vec<_>>()
        });
        let mut bound_ids = Vec::new();
        for actual in reserved_ids {
            match self.inner.catalog.get(&actual) {
                Ok(_) => bound_ids.push(actual),
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        bound_ids.sort();
        let receipt_session_id = self
            .latest_receipt(draft_id)
            .and_then(|receipt| receipt.session_id);
        let bound_id = if draft_exists {
            None
        } else {
            receipt_session_id
                .filter(|session_id| bound_ids.contains(session_id))
                .or_else(|| bound_ids.first().cloned())
        };
        let session_id = bound_id.clone().unwrap_or_else(|| draft_id.to_string());

        self.inner.durable.transact(|data| {
            data.aliases.retain(|actual, ledger_id| {
                ledger_id != draft_id || bound_id.as_deref() == Some(actual.as_str())
            });
            if let Some(bound_id) = &bound_id {
                data.aliases.insert(bound_id.clone(), draft_id.to_string());
            }
            if let Some(receipt) = data
                .sessions
                .get_mut(draft_id)
                .and_then(|ledger| ledger.receipts.back_mut())
                && matches!(receipt.status, ReceiptStatus::Incomplete)
            {
                receipt.session_id = Some(session_id.clone());
            }
            Ok(())
        })?;
        Ok(session_id)
    }

    fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.inner.shutdown_tx.send(());
        let stops = {
            let runtime = lock(&self.inner.runtime);
            runtime
                .sessions
                .values()
                .filter_map(|entry| entry.active.as_ref().map(|active| active.stop.clone()))
                .collect::<Vec<_>>()
        };
        for stop in stops {
            let _ = stop.stop();
        }
        for worker in std::mem::take(&mut *lock(&self.inner.workers)) {
            let _ = worker.join();
        }
    }
}

impl SessionRuntime {
    fn new() -> Self {
        let (wake, _) = broadcast::channel(128);
        Self {
            events: VecDeque::new(),
            wake,
            active: None,
            live: None,
        }
    }
}

fn runtime_id(runtime: &Runtime, id: &str) -> String {
    runtime
        .aliases
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

fn outcome_summary(outcome: &TurnOutcome) -> (ReceiptStatus, Value) {
    match outcome {
        TurnOutcome::Succeeded { capped, .. } => (
            ReceiptStatus::Succeeded,
            json!({"status": "succeeded", "capped": capped}),
        ),
        TurnOutcome::Failed { .. } => (ReceiptStatus::Failed, json!({"status": "failed"})),
        TurnOutcome::Stopped { .. } => (ReceiptStatus::Stopped, json!({"status": "stopped"})),
        TurnOutcome::Incomplete { forced, .. } => (
            ReceiptStatus::Incomplete,
            json!({"status": "incomplete", "forced": forced}),
        ),
    }
}

fn stream_event_value(event: &StreamEvent) -> Value {
    match event {
        StreamEvent::TurnStart {
            seq,
            request,
            provider,
            model,
            session_id,
            turn_index,
        } => {
            json!({"event":"turn_start","seq":seq,"request":request,"provider":provider,"model":model,"session_id":session_id,"turn_index":turn_index})
        }
        StreamEvent::TextDelta {
            seq,
            round_index,
            block_index,
            text,
        } => {
            json!({"event":"text_delta","seq":seq,"round_index":round_index,"block_index":block_index,"text":text})
        }
        StreamEvent::ToolRound {
            seq,
            round_index,
            content,
        } => json!({"event":"tool_round","seq":seq,"round_index":round_index,"content":content}),
        StreamEvent::ToolCall {
            seq,
            round_index,
            tool_use_id,
            tool_name,
            input,
        } => {
            json!({"event":"tool_call","seq":seq,"round_index":round_index,"tool_use_id":tool_use_id,"tool_name":tool_name,"input":input})
        }
        StreamEvent::ToolResult {
            seq,
            round_index,
            tool_use_id,
            tool_name,
            status,
            output,
        } => {
            json!({"event":"tool_result","seq":seq,"round_index":round_index,"tool_use_id":tool_use_id,"tool_name":tool_name,"status":status,"output":output})
        }
        StreamEvent::TurnEnd {
            seq,
            response,
            capped,
            session_id,
            turn_index,
        } => {
            json!({"event":"turn_end","seq":seq,"response":response,"capped":capped,"session_id":session_id,"turn_index":turn_index})
        }
        StreamEvent::Unknown { seq, event, record } => {
            json!({"event":event,"seq":seq,"record":record})
        }
    }
}

fn save_tool_observation(
    catalog: &SessionCatalog,
    session_id: &str,
    turn_index: u64,
    tool_use_id: &str,
    field: &str,
    observed_at_ms: u64,
) {
    let mut metadata = catalog
        .get(session_id)
        .ok()
        .and_then(|session| session.display.get("web.tool_observations").cloned())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let turns = metadata
        .as_object_mut()
        .expect("tool observation metadata is an object");
    let turn = turns
        .entry(turn_index.to_string())
        .or_insert_with(|| json!({}));
    if !turn.is_object() {
        *turn = json!({});
    }
    let tools = turn
        .as_object_mut()
        .expect("turn observations are an object");
    let tool = tools
        .entry(tool_use_id.to_string())
        .or_insert_with(|| json!({}));
    if !tool.is_object() {
        *tool = json!({});
    }
    tool[field] = json!(observed_at_ms);
    let _ = catalog.save_display_metadata(session_id, "web.tool_observations".into(), metadata);
}

fn validate_bind_address(addr: SocketAddr) -> Result<(), HostError> {
    if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(HostError::BindAddress(addr));
    }
    Ok(())
}

fn authority_for(addr: SocketAddr) -> String {
    format!("127.0.0.1:{}", addr.port())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn authorization_matches(value: Option<&HeaderValue>, token: &str) -> bool {
    let Some(value) = value.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some(candidate) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(candidate.as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn random_hex(bytes: usize) -> Result<String, HostError> {
    let mut value = vec![0u8; bytes];
    getrandom::fill(&mut value).map_err(|error| HostError::State(error.to_string()))?;
    Ok(hex(&value))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_name(name: &str) -> Result<(), ApiError> {
    if name.trim().is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(api_error(StatusCode::BAD_REQUEST, "invalid_session_name"));
    }
    Ok(())
}

fn validate_tab_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(api_error(StatusCode::BAD_REQUEST, "invalid_tab_id"));
    }
    Ok(())
}

fn map_catalog_error(error: CatalogError) -> ApiError {
    match error {
        CatalogError::NotFound(_) | CatalogError::InvalidSessionId => {
            api_error(StatusCode::NOT_FOUND, "session_not_found")
        }
        CatalogError::Busy => api_error(StatusCode::CONFLICT, "session_busy"),
        CatalogError::WorkspaceRequired(_) | CatalogError::WorkspaceMissing(_) => {
            api_error(StatusCode::CONFLICT, "workspace_required")
        }
        CatalogError::ReadOnly(_) => api_error(StatusCode::CONFLICT, "session_read_only"),
        CatalogError::AlreadyExists(_) => api_error(StatusCode::CONFLICT, "session_exists"),
        CatalogError::NoRetry(_) | CatalogError::StaleRetry => {
            api_error(StatusCode::CONFLICT, "retry_unavailable")
        }
        _ => api_error(StatusCode::INTERNAL_SERVER_ERROR, "catalog_unavailable"),
    }
}

fn acquire_host_lock(state_dir: &Path) -> Result<File, HostError> {
    let path = state_dir.join(LOCK_NAME);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let contended_code = fs2::lock_contended_error().raw_os_error();
    file.try_lock_exclusive().map_err(|error| {
        let is_contended = error.kind() == io::ErrorKind::WouldBlock
            || contended_code.is_some_and(|code| error.raw_os_error() == Some(code));
        if is_contended {
            HostError::State(
                "another leg-web host already owns this catalog; use its printed URL or stop it before starting another".into(),
            )
        } else {
            HostError::Io(error)
        }
    })?;
    Ok(file)
}

fn write_atomic(path: &Path, data: &DurableData) -> io::Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".web-host-state-{}-{sequence}.tmp", process::id()));
    let bytes = serde_json::to_vec(data).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn test_host() -> (Host, TempDir) {
        test_host_with_limits(DEFAULT_EVENT_BUFFER, DEFAULT_RECEIPTS)
    }

    fn test_host_with_limits(event_buffer: usize, receipt_limit: usize) -> (Host, TempDir) {
        let temp = TempDir::new().unwrap();
        let host = Host::open(HostConfig {
            catalog: SessionCatalogConfig {
                state_dir: Some(temp.path().to_path_buf()),
                ..SessionCatalogConfig::default()
            },
            event_buffer,
            receipt_limit,
            ..HostConfig::default()
        })
        .unwrap();
        *lock(&host.state.inner.authority) = Some("127.0.0.1:43127".into());
        (host, temp)
    }

    #[cfg(unix)]
    fn test_host_with_driver(temp: &TempDir, leg: &Path, supervisor: &Path) -> Host {
        let host = Host::open(HostConfig {
            catalog: SessionCatalogConfig {
                state_dir: Some(temp.path().to_path_buf()),
                leg_bin: Some(leg.to_path_buf()),
                supervisor_bin: Some(supervisor.to_path_buf()),
            },
            ..HostConfig::default()
        })
        .unwrap();
        *lock(&host.state.inner.authority) = Some("127.0.0.1:43127".into());
        host
    }

    #[test]
    fn restart_recovers_pending_incomplete_draft_receipt() {
        let (host, temp) = test_host();
        let draft = host
            .state
            .inner
            .catalog
            .create_draft(
                SessionInterface::Web,
                Some("pending draft".into()),
                Some(temp.path()),
            )
            .unwrap();
        let bound_draft = host
            .state
            .inner
            .catalog
            .create_draft(
                SessionInterface::Web,
                Some("bound pending draft".into()),
                Some(temp.path()),
            )
            .unwrap();
        let bound_id = "sess-pending-bound";
        let catalog_path = temp.path().join("catalog.json");
        let mut catalog: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
        catalog["sessions"][&draft.id]["pending_new_turn"] = json!(true);
        let sessions = catalog["sessions"].as_object_mut().unwrap();
        let bound_metadata = sessions.remove(&bound_draft.id).unwrap();
        sessions.insert(bound_id.to_string(), bound_metadata);
        fs::write(&catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        host.state
            .inner
            .durable
            .transact(|data| {
                let ledger = data.sessions.entry(draft.id.clone()).or_default();
                ledger.high_water = 1;
                ledger.receipts.push_back(Receipt {
                    request_id: 1,
                    prompt_sha256: "00".into(),
                    turn_id: "turn-pending".into(),
                    status: ReceiptStatus::Running,
                    session_id: Some(draft.id.clone()),
                    outcome: Some(json!({"status": "running"})),
                    recovery_evidence: None,
                });
                let bound_ledger = data.sessions.entry(bound_draft.id.clone()).or_default();
                bound_ledger.high_water = 1;
                bound_ledger.receipts.push_back(Receipt {
                    request_id: 1,
                    prompt_sha256: "11".into(),
                    turn_id: "turn-bound-pending".into(),
                    status: ReceiptStatus::Running,
                    session_id: Some(bound_id.into()),
                    outcome: Some(json!({"status": "running"})),
                    recovery_evidence: None,
                });
                data.aliases.insert(bound_id.into(), bound_draft.id.clone());
                Ok(())
            })
            .unwrap();
        host.state
            .inner
            .durable
            .transact(|data| {
                data.aliases
                    .insert("sess-pending-reserved".into(), draft.id.clone());
                Ok(())
            })
            .unwrap();
        assert!(
            host.state
                .inner
                .catalog
                .get(&draft.id)
                .unwrap()
                .pending_new_turn
        );
        host.state.shutdown();
        drop(host);

        let recovered = Host::open(HostConfig {
            catalog: SessionCatalogConfig {
                state_dir: Some(temp.path().to_path_buf()),
                ..SessionCatalogConfig::default()
            },
            ..HostConfig::default()
        })
        .unwrap();
        let recovered_draft = recovered.state.inner.catalog.get(&draft.id).unwrap();
        assert!(recovered_draft.pending_new_turn);
        assert_eq!(recovered_draft.run_state, CatalogRunState::Unknown);
        assert!(recovered_draft.warnings.iter().any(|warning| {
            warning.contains("legacy pending draft")
                && warning.contains("copy the saved prompt and workspace")
        }));
        assert_eq!(
            recovered.state.snapshot(&draft.id).unwrap().session.id,
            draft.id
        );
        assert_eq!(
            recovered
                .state
                .snapshot(&bound_draft.id)
                .unwrap()
                .session
                .id,
            bound_id
        );
        assert_eq!(
            recovered
                .state
                .inner
                .durable
                .read(|data| data.aliases.clone()),
            HashMap::from([(bound_id.to_string(), bound_draft.id.clone())])
        );
        let receipt = recovered.state.latest_receipt(&draft.id).unwrap();
        assert_eq!(receipt.status, ReceiptStatus::Incomplete);
        assert_eq!(receipt.session_id.as_deref(), Some(draft.id.as_str()));
        assert_eq!(
            receipt.recovery_evidence.as_deref(),
            Some("driver_ownership_unknown")
        );
        let bound_receipt = recovered.state.latest_receipt(&bound_draft.id).unwrap();
        assert_eq!(bound_receipt.status, ReceiptStatus::Incomplete);
        assert_eq!(bound_receipt.session_id.as_deref(), Some(bound_id));
        recovered.state.shutdown();
    }

    async fn round_trip_json(router: &Router, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn api_request(host: &Host, path: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri(path)
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token))
            .body(Body::empty())
            .unwrap()
    }

    fn api_json_request(
        host: &Host,
        method: Method,
        path: &str,
        body: &str,
        content_type: bool,
    ) -> HttpRequest<Body> {
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token));
        if content_type {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        request.body(Body::from(body.to_owned())).unwrap()
    }

    async fn assert_api_error_json(
        response: Response,
        expected_status: StatusCode,
        expected_code: &str,
        forbidden: &str,
    ) {
        assert_eq!(response.status(), expected_status);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains(forbidden),
            "error echoed request input: {text}"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error": expected_code})
        );
    }

    #[test]
    fn refuses_non_loopback_configuration() {
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap()).is_err());
        assert!(validate_bind_address("[::1]:0".parse().unwrap()).is_err());
        assert!(validate_bind_address("127.0.0.1:8080".parse().unwrap()).is_ok());
        assert!(validate_bind_address("127.0.0.1:0".parse().unwrap()).is_ok());
    }

    #[test]
    fn default_bind_is_the_fixed_loopback_port() {
        assert_eq!(
            HostConfig::default().bind_addr,
            SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT))
        );
    }

    #[tokio::test]
    async fn occupied_port_reports_a_bind_hint() {
        let temp = TempDir::new().unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();
        let host = Host::open(HostConfig {
            bind_addr: addr,
            catalog: SessionCatalogConfig {
                state_dir: Some(temp.path().to_path_buf()),
                ..SessionCatalogConfig::default()
            },
            ..HostConfig::default()
        })
        .unwrap();
        let error = host.bind().await.err().unwrap();
        assert!(matches!(error, HostError::PortInUse(reported) if reported == addr));
        assert!(error.to_string().contains("--bind 127.0.0.1:"));
    }

    #[tokio::test]
    async fn binding_uses_loopback_and_the_os_assigned_port() {
        let temp = TempDir::new().unwrap();
        let host = Host::open(HostConfig {
            bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            catalog: SessionCatalogConfig {
                state_dir: Some(temp.path().to_path_buf()),
                ..SessionCatalogConfig::default()
            },
            ..HostConfig::default()
        })
        .unwrap();
        let bound = host.bind().await.unwrap();
        assert_eq!(bound.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(bound.local_addr().port(), 0);
        let url = bound.launch_url();
        assert!(url.starts_with(&format!("http://127.0.0.1:{}/#", bound.local_addr().port())));
        let fragment = url.split_once('#').unwrap().1;
        assert_eq!(fragment.len(), 64);
    }

    #[tokio::test]
    async fn host_lock_is_process_owned_and_prevents_a_second_host() {
        let temp = TempDir::new().unwrap();
        let config = HostConfig {
            catalog: SessionCatalogConfig {
                state_dir: Some(temp.path().to_path_buf()),
                ..SessionCatalogConfig::default()
            },
            ..HostConfig::default()
        };
        let host = Host::open(config.clone()).unwrap();
        let error = Host::open(config).err().unwrap().to_string();
        assert!(error.contains("another leg-web host"));
        drop(host);
    }

    #[tokio::test]
    async fn unauthenticated_or_hostile_requests_cannot_read_or_mutate() {
        let (host, _temp) = test_host();
        let session = host
            .state
            .inner
            .catalog
            .create_draft(SessionInterface::Web, Some("private-session".into()), None)
            .unwrap();
        let submit_path = format!("/api/sessions/{}/submit", session.id);
        let router = build_router(host.state.clone());
        let missing = HttpRequest::builder()
            .uri("/api/sessions")
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .body(Body::empty())
            .unwrap();
        let missing_response = router.clone().oneshot(missing).await.unwrap();
        assert_eq!(missing_response.status(), StatusCode::UNAUTHORIZED);
        let missing_body = missing_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert!(!String::from_utf8_lossy(&missing_body).contains("private-session"));

        let unauthorized_submit = HttpRequest::builder()
            .method(Method::POST)
            .uri(&submit_path)
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"request_id":1,"prompt":"start"}"#))
            .unwrap();
        assert_eq!(
            router
                .clone()
                .oneshot(unauthorized_submit)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let mut bad_origin = api_request(&host, "/api/sessions");
        bad_origin
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://attacker.invalid"));
        assert_eq!(
            router.clone().oneshot(bad_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let hostile_submit = HttpRequest::builder()
            .method(Method::POST)
            .uri(&submit_path)
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "https://attacker.invalid")
            .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"request_id":1,"prompt":"start"}"#))
            .unwrap();
        assert_eq!(
            router
                .clone()
                .oneshot(hostile_submit)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );

        let mut bad_host = api_request(&host, "/api/sessions");
        bad_host
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("attacker.invalid"));
        assert_eq!(
            router.clone().oneshot(bad_host).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let hostile_stop = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("/api/sessions/{}/stop", session.id))
            .header(HOST, "attacker.invalid")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(hostile_stop).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let unchanged = host.state.inner.catalog.get(&session.id).unwrap();
        assert_eq!(unchanged.name.as_deref(), Some("private-session"));
        host.state
            .inner
            .durable
            .read(|data| assert!(data.sessions.is_empty()));
        assert!(lock(&host.state.inner.runtime).sessions.is_empty());
    }

    #[tokio::test]
    async fn static_assets_bootstrap_from_fragment_and_contain_no_token() {
        let (host, _temp) = test_host();
        let persisted = fs::read_to_string(host.inner_state_dir().join(STORE_NAME)).unwrap();
        assert!(!persisted.contains(&host.state.inner.token));
        let router = build_router(host.state.clone());
        let response = router
            .oneshot(
                HttpRequest::builder()
                    .uri("/")
                    .header(HOST, "127.0.0.1:43127")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body).contains(&host.state.inner.token));
        assert!(!APP_JS.contains(&host.state.inner.token));
        assert!(CONTROLLER_JS.contains("location.hash"));
        assert!(CONTROLLER_JS.contains("sessionStorage"));
        assert!(CONTROLLER_JS.contains("replaceState"));
        assert!(!INDEX_HTML.contains("/app.css"));
        assert!(APP_JS.contains("/themes/registry.js"));
        assert!(APP_JS.contains("/controller.js"));
        let controller_response = build_router(host.state.clone())
            .oneshot(
                HttpRequest::builder()
                    .uri("/controller.js")
                    .header(HOST, "127.0.0.1:43127")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(controller_response.status(), StatusCode::OK);
        assert_eq!(
            controller_response.headers().get(CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert!(THEME_REGISTRY_JS.contains("/themes/default.js"));
        assert!(DEFAULT_THEME_CSS.contains(".composer"));
        assert!(SHARED_RENDERER_JS.contains("createSharedRenderer"));
        let css_response = build_router(host.state.clone())
            .oneshot(
                HttpRequest::builder()
                    .uri("/themes/default.css")
                    .header(HOST, "127.0.0.1:43127")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(css_response.status(), StatusCode::OK);
        assert_eq!(
            css_response.headers().get(CONTENT_TYPE).unwrap(),
            "text/css; charset=utf-8"
        );
        assert!(
            css_response
                .headers()
                .get(CONTENT_SECURITY_POLICY)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("style-src 'self'")
        );

        let shared_renderer_response = build_router(host.state.clone())
            .oneshot(
                HttpRequest::builder()
                    .uri("/themes/shared-renderer.js")
                    .header(HOST, "127.0.0.1:43127")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shared_renderer_response.status(), StatusCode::OK);
        assert_eq!(
            shared_renderer_response
                .headers()
                .get(CONTENT_TYPE)
                .unwrap(),
            "text/javascript; charset=utf-8"
        );

        let unknown = build_router(host.state.clone())
            .oneshot(
                HttpRequest::builder()
                    .uri("/themes/not-registered.js")
                    .header(HOST, "127.0.0.1:43127")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        let wrong_method = build_router(host.state.clone())
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/themes/default.html")
                    .header(HOST, "127.0.0.1:43127")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);

        #[cfg(feature = "browser-e2e-themes")]
        {
            assert!(THEME_REGISTRY_JS.contains("id: \"fixture\""));
            let fixture = build_router(host.state.clone())
                .oneshot(
                    HttpRequest::builder()
                        .uri("/themes/fixture.css")
                        .header(HOST, "127.0.0.1:43127")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(fixture.status(), StatusCode::OK);
        }
    }

    #[test]
    fn turn_start_metadata_is_kept_in_the_live_and_session_snapshots() {
        let (host, _temp) = test_host();
        let draft = host
            .state
            .inner
            .catalog
            .create_draft(SessionInterface::Web, Some("metadata-test".into()), None)
            .unwrap();
        host.state.ensure_runtime(&draft.id);
        {
            let mut runtime = lock(&host.state.inner.runtime);
            let entry = runtime.sessions.get_mut(&draft.id).unwrap();
            entry.live = Some(LiveSnapshot {
                turn_id: "turn-metadata".into(),
                turn_index: None,
                prompt: "hello".into(),
                text: String::new(),
                status: "starting".into(),
                started_at_ms: now_millis(),
                provider: None,
                model: None,
                active_tool: None,
                tools: Vec::new(),
            });
        }

        host.state
            .record_stream_event(
                &draft.id,
                &draft.id,
                "turn-metadata",
                StreamEvent::TurnStart {
                    seq: 0,
                    request: json!({"schema": "baton.message/v1"}),
                    provider: "anthropic".into(),
                    model: "fixture-model".into(),
                    session_id: Some(draft.id.clone()),
                    turn_index: Some(0),
                },
            )
            .unwrap();

        host.state
            .record_stream_event(
                &draft.id,
                &draft.id,
                "turn-metadata",
                StreamEvent::ToolCall {
                    seq: 1,
                    round_index: 0,
                    tool_use_id: "reuse-id".into(),
                    tool_name: "bash".into(),
                    input: json!({"command": "pwd"}),
                },
            )
            .unwrap();
        host.state
            .record_stream_event(
                &draft.id,
                &draft.id,
                "turn-metadata",
                StreamEvent::ToolResult {
                    seq: 2,
                    round_index: 0,
                    tool_use_id: "reuse-id".into(),
                    tool_name: "bash".into(),
                    status: "completed".into(),
                    output: json!("done"),
                },
            )
            .unwrap();

        let snapshot = host.state.snapshot(&draft.id).unwrap();
        let live = snapshot.active.unwrap();
        assert_eq!(live.provider.as_deref(), Some("anthropic"));
        assert_eq!(live.model.as_deref(), Some("fixture-model"));
        assert_eq!(live.turn_index, Some(0));
        assert!(live.started_at_ms > 0);
        assert!(live.tools[0]["call_observed_at_ms"].as_u64().is_some());
        assert!(live.tools[0]["result_observed_at_ms"].as_u64().is_some());
        assert_eq!(live.tools[0]["status"], "completed");
        assert_eq!(
            snapshot.session.display["web.turn_metadata"]["0"]["provider"],
            "anthropic"
        );
        assert_eq!(
            snapshot.session.display["web.turn_metadata"]["0"]["model"],
            "fixture-model"
        );
        assert!(snapshot.session.display["web.tool_observations"]["0"]["reuse-id"]["call_observed_at_ms"]
            .as_u64()
            .is_some());
        assert!(snapshot.session.display["web.tool_observations"]["0"]["reuse-id"]["result_observed_at_ms"]
            .as_u64()
            .is_some());
        let first_call_timestamp = snapshot.session.display["web.tool_observations"]["0"]["reuse-id"]["call_observed_at_ms"]
            .as_u64()
            .unwrap();
        save_tool_observation(
            &host.state.inner.catalog,
            &draft.id,
            1,
            "reuse-id",
            "call_observed_at_ms",
            first_call_timestamp + 1,
        );
        let display = host.state.inner.catalog.get(&draft.id).unwrap().display;
        assert_eq!(
            display["web.tool_observations"]["0"]["reuse-id"]["call_observed_at_ms"],
            first_call_timestamp
        );
        assert_eq!(
            display["web.tool_observations"]["1"]["reuse-id"]["call_observed_at_ms"],
            first_call_timestamp + 1
        );
        assert!(
            display["web.tool_observations"]["1"]["reuse-id"]["result_observed_at_ms"].is_null()
        );

        {
            let mut runtime = lock(&host.state.inner.runtime);
            runtime
                .sessions
                .get_mut(&draft.id)
                .unwrap()
                .live
                .as_mut()
                .unwrap()
                .status = "stopping".into();
        }
        host.state
            .record_stream_event(
                &draft.id,
                &draft.id,
                "turn-metadata",
                StreamEvent::TextDelta {
                    seq: 3,
                    round_index: 0,
                    block_index: 0,
                    text: "late text".into(),
                },
            )
            .unwrap();
        host.state
            .record_stream_event(
                &draft.id,
                &draft.id,
                "turn-metadata",
                StreamEvent::TurnEnd {
                    seq: 4,
                    response: json!({}),
                    capped: true,
                    session_id: Some(draft.id.clone()),
                    turn_index: Some(0),
                },
            )
            .unwrap();
        let snapshot = host.state.snapshot(&draft.id).unwrap();
        let live = snapshot.active.unwrap();
        assert_eq!(live.status, "stopping");
        assert_eq!(live.text, "late text");
        assert_eq!(
            snapshot.session.display["web.turn_metadata"]["0"]["capped"],
            true
        );
    }

    #[tokio::test]
    async fn malformed_json_unknown_ids_and_traversal_have_no_effect() {
        let (host, _temp) = test_host();
        let router = build_router(host.state.clone());
        let malformed = router
            .clone()
            .oneshot(api_json_request(
                &host,
                Method::POST,
                "/api/sessions",
                "MALFORMED_SENTINEL {",
                true,
            ))
            .await
            .unwrap();
        assert_api_error_json(
            malformed,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "MALFORMED_SENTINEL",
        )
        .await;

        let unknown_field = router
            .clone()
            .oneshot(api_json_request(
                &host,
                Method::POST,
                "/api/sessions",
                r#"{"name":"safe","UNKNOWN_FIELD_SENTINEL":"private"}"#,
                true,
            ))
            .await
            .unwrap();
        assert_api_error_json(
            unknown_field,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "UNKNOWN_FIELD_SENTINEL",
        )
        .await;

        let bad_query = router
            .clone()
            .oneshot(api_request(
                &host,
                "/api/sessions/not-a-session/events?after=CURSOR_SENTINEL",
            ))
            .await
            .unwrap();
        assert_api_error_json(
            bad_query,
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "CURSOR_SENTINEL",
        )
        .await;

        let missing_content_type = router
            .clone()
            .oneshot(api_json_request(
                &host,
                Method::POST,
                "/api/sessions",
                r#"{"name":"CONTENT_TYPE_SENTINEL"}"#,
                false,
            ))
            .await
            .unwrap();
        assert_api_error_json(
            missing_content_type,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "CONTENT_TYPE_SENTINEL",
        )
        .await;

        let unknown = router
            .clone()
            .oneshot(api_json_request(
                &host,
                Method::POST,
                "/api/sessions/unknown/submit",
                r#"{"request_id":1,"prompt":"start"}"#,
                true,
            ))
            .await
            .unwrap();
        assert_api_error_json(unknown, StatusCode::NOT_FOUND, "session_not_found", "start").await;
        let traversal = router
            .oneshot(api_request(&host, "/api/sessions/%2e%2e%2fetc%2fpasswd"))
            .await
            .unwrap();
        assert_api_error_json(
            traversal,
            StatusCode::NOT_FOUND,
            "session_not_found",
            "/etc/passwd",
        )
        .await;
        assert!(host.inner_sessions().is_empty());
    }

    #[test]
    fn authorization_check_uses_fixed_work_and_token_is_256_bits() {
        let (host, _temp) = test_host();
        assert_eq!(host.state.inner.token.len(), 64);
        assert!(!authorization_matches(
            Some(&HeaderValue::from_static("Bearer short")),
            &host.state.inner.token
        ));
        let valid = HeaderValue::from_str(&format!("Bearer {}", host.state.inner.token)).unwrap();
        assert!(authorization_matches(Some(&valid), &host.state.inner.token));
        assert!(!authorization_matches(
            Some(&HeaderValue::from_static(
                "Bearer 0000000000000000000000000000000000000000000000000000000000000000"
            )),
            &host.state.inner.token
        ));
    }

    #[test]
    fn durable_receipt_summary_does_not_copy_provider_response_content() {
        let (status, receipt) = outcome_summary(&TurnOutcome::Succeeded {
            response: json!({"credential_echo": "provider-secret", "content": "answer"}),
            capped: false,
        });
        assert_eq!(status, ReceiptStatus::Succeeded);
        assert_eq!(receipt, json!({"status": "succeeded", "capped": false}));
        assert!(!receipt.to_string().contains("provider-secret"));
    }

    #[test]
    fn receipt_high_water_survives_bounded_eviction() {
        let receipt = |request_id, prompt_sha256: &str| Receipt {
            request_id,
            prompt_sha256: prompt_sha256.into(),
            turn_id: format!("turn-{request_id}"),
            status: ReceiptStatus::Succeeded,
            session_id: Some("session-a".into()),
            outcome: None,
            recovery_evidence: None,
        };
        let mut ledger = SessionLedger {
            high_water: 4,
            cursor: 0,
            receipts: VecDeque::from([receipt(3, "hash-three"), receipt(4, "hash-four")]),
        };
        assert!(matches!(
            submission_decision(Some(&ledger), 3, "hash-three"),
            Ok(SubmissionDecision::Replay(_))
        ));
        assert_eq!(
            submission_decision(Some(&ledger), 3, "different"),
            Err(SubmissionReject::Conflict)
        );
        assert_eq!(
            submission_decision(Some(&ledger), 1, "hash-one"),
            Err(SubmissionReject::Stale)
        );
        assert_eq!(
            submission_decision(Some(&ledger), 6, "hash-six"),
            Err(SubmissionReject::Unexpected)
        );
        assert_eq!(
            submission_decision(Some(&ledger), 5, "hash-five"),
            Ok(SubmissionDecision::New)
        );
        ledger.high_water = u64::MAX;
        assert_eq!(
            submission_decision(Some(&ledger), 1, "hash-one"),
            Err(SubmissionReject::Exhausted)
        );
    }

    #[test]
    fn failed_start_keeps_the_durable_request_id_spent() {
        let (host, _temp) = test_host();
        let receipt = |request_id, status| Receipt {
            request_id,
            prompt_sha256: format!("hash-{request_id}"),
            turn_id: format!("turn-{request_id}"),
            status,
            session_id: Some("session-a".into()),
            outcome: None,
            recovery_evidence: None,
        };
        host.state
            .inner
            .durable
            .transact(|data| {
                let ledger = data.sessions.entry("session-a".into()).or_default();
                ledger.high_water = 2;
                ledger
                    .receipts
                    .push_back(receipt(1, ReceiptStatus::Succeeded));
                ledger
                    .receipts
                    .push_back(receipt(2, ReceiptStatus::Running));
                Ok(())
            })
            .unwrap();

        host.state
            .finish_receipt(
                "session-a",
                2,
                ReceiptStatus::Incomplete,
                Some("session-a".into()),
                json!({"status": "incomplete"}),
            )
            .unwrap();

        host.state.inner.durable.read(|data| {
            let ledger = data.sessions.get("session-a").unwrap();
            assert_eq!(ledger.high_water, 2);
            assert_eq!(ledger.receipts.len(), 2);
            assert_eq!(ledger.receipts[1].request_id, 2);
            assert_eq!(ledger.receipts[1].status, ReceiptStatus::Incomplete);
        });
    }

    #[test]
    fn expired_cursor_resets_to_snapshot_and_valid_cursor_catches_up_once() {
        let (host, _temp) = test_host_with_limits(2, 4);
        let draft = host
            .state
            .inner
            .catalog
            .create_draft(SessionInterface::Web, Some("test".into()), None)
            .unwrap();
        let id = draft.id;
        host.state.ensure_runtime(&id);
        for n in 1..=3 {
            host.state
                .append_event(&id, &id, "turn-test", "stream", json!({"n": n}))
                .unwrap();
        }
        let (events, reset) = host.state.catch_up(&id, &id, 1).unwrap();
        assert!(reset.is_none());
        assert_eq!(
            events.iter().map(|event| event.cursor).collect::<Vec<_>>(),
            [2, 3]
        );
        let (events, reset) = host.state.catch_up(&id, &id, 3).unwrap();
        assert!(events.is_empty());
        assert!(reset.is_none());
        let (events, reset) = host.state.catch_up(&id, &id, 0).unwrap();
        assert!(events.is_empty());
        let snapshot = reset.unwrap();
        assert_eq!(snapshot["cursor"], 3);
        assert_eq!(snapshot["turn_id"], "turn-test");
        assert_eq!(
            host.state.catch_up(&id, &id, 4).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn restart_marks_every_unfinished_receipt_incomplete_without_replaying() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(STORE_NAME);
        let receipt = |request_id, status| Receipt {
            request_id,
            prompt_sha256: format!("hash-{request_id}"),
            turn_id: format!("turn-{request_id}"),
            status,
            session_id: None,
            outcome: None,
            recovery_evidence: None,
        };
        {
            let store = DurableStore::open(path.clone(), 2).unwrap();
            store
                .transact(|data| {
                    let ledger = data.sessions.entry("draft-example".into()).or_default();
                    ledger.high_water = 2;
                    ledger
                        .receipts
                        .push_back(receipt(1, ReceiptStatus::Accepted));
                    ledger
                        .receipts
                        .push_back(receipt(2, ReceiptStatus::Running));
                    Ok(())
                })
                .unwrap();
        }
        let store = DurableStore::open(path, 2).unwrap();
        store.read(|data| {
            let ledger = data.sessions.get("draft-example").unwrap();
            assert_eq!(ledger.high_water, 2);
            assert!(
                ledger
                    .receipts
                    .iter()
                    .all(|receipt| matches!(receipt.status, ReceiptStatus::Incomplete))
            );
            assert_eq!(ledger.receipts.len(), 2);
        });
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn held_draft_binding_does_not_block_other_sessions_on_one_worker() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let fixture_dir = workspace.join("fixture");
        fs::create_dir_all(&fixture_dir).unwrap();
        let log_path = fixture_dir.join("turns.log");
        let leg = fixture_dir.join("fake-leg");
        let source = fixture_dir.join("fake_leg.rs");
        fs::write(
            &source,
            r#"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

unsafe extern "C" {
    fn signal(signal: i32, handler: extern "C" fn(i32)) -> extern "C" fn(i32);
    fn _exit(status: i32) -> !;
}

extern "C" fn exit_on_interrupt(signal: i32) {
    unsafe { _exit(128 + signal) }
}

extern "C" fn ignore_interrupt(_: i32) {}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args == ["--version"] {
        println!("leg 0.14.0");
        return;
    }
    if args == ["--help"] {
        println!("usage: leg exchange --stream-json --new-session-id <id>");
        return;
    }
    if args.first().map(String::as_str) != Some("exchange") {
        std::process::exit(2);
    }
    let session_id = args
        .windows(2)
        .find(|pair| pair[0] == "--new-session-id" || pair[0] == "--session")
        .map(|pair| pair[1].clone())
        .expect("session argument");
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt).unwrap();
    if prompt == "HOLD-PENDING-STOP" {
        unsafe { signal(2, exit_on_interrupt); }
    } else if prompt == "HOLD-CANCELLED" {
        unsafe { signal(2, ignore_interrupt); }
    }
    let fixture = env::current_dir().unwrap().join("fixture");
    let log = OpenOptions::new().create(true).append(true).open(fixture.join("turns.log")).unwrap();
    writeln!(&log, "{session_id}|{prompt}").unwrap();
    if prompt == "FAIL-BEFORE-TRAIL" {
        std::process::exit(1);
    }
    let session_dir = PathBuf::from(env::var_os("LEG_SESSION_DIR").unwrap());
    fs::create_dir_all(&session_dir).unwrap();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(session_dir.join(format!("{session_id}.jsonl")))
        .unwrap();
    fs::write(fixture.join(format!("{prompt}.id")), &session_id).unwrap();
    if prompt == "FAIL-START" {
        std::process::exit(1);
    }
    if prompt.starts_with("HOLD") {
        let release = fixture.join(format!("{prompt}.release"));
        while !release.exists() {
            thread::sleep(Duration::from_millis(10));
        }
    }
    fs::write(fixture.join(format!("{prompt}.finished")), "finished").unwrap();
    let start = format!(
        "{{\"schema\":\"leg.exchange.stream/v1\",\"event\":\"turn_start\",\"seq\":0,\"provider\":\"fixture\",\"model\":\"fixture\",\"session_id\":\"{session_id}\",\"turn_index\":0,\"request\":{{\"schema\":\"baton.message/v1\",\"message_id\":\"request-{prompt}\",\"conversation_id\":\"conversation-{prompt}\",\"kind\":\"request\",\"body\":\"{prompt}\"}}}}"
    );
    let end = format!(
        "{{\"schema\":\"leg.exchange.stream/v1\",\"event\":\"turn_end\",\"seq\":1,\"capped\":false,\"session_id\":\"{session_id}\",\"turn_index\":0,\"response\":{{\"schema\":\"baton.message/v1\",\"message_id\":\"response-{prompt}\",\"conversation_id\":\"conversation-{prompt}\",\"in_reply_to\":\"request-{prompt}\",\"kind\":\"response\",\"body\":\"ok\"}}}}"
    );
    println!("{start}");
    println!("{end}");
}
"#,
        )
        .unwrap();
        let compiled = Command::new("rustc")
            .args(["--edition=2021"])
            .arg(&source)
            .arg("-o")
            .arg(&leg)
            .output()
            .expect("rustc is available to build the native fixture");
        assert!(
            compiled.status.success(),
            "failed to compile native leg fixture: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let supervisor = std::env::current_exe()
            .unwrap()
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("leg-ui-supervisor");
        assert!(supervisor.is_file(), "missing supervisor at {supervisor:?}");
        let host = test_host_with_driver(&temp, &leg, &supervisor);
        let router = build_router(host.state.clone());

        let release_files = ["HOLD-A", "HOLD-D", "HOLD-CANCELLED"]
            .map(|prompt| fixture_dir.join(format!("{prompt}.release")));
        let (watchdog_tx, watchdog_rx) = std::sync::mpsc::channel();
        let watchdog_releases = release_files.clone();
        let watchdog = thread::spawn(move || {
            if watchdog_rx.recv_timeout(Duration::from_secs(8)).is_err() {
                for release in watchdog_releases {
                    let _ = fs::write(release, "release");
                }
            }
        });

        let create_draft = |name: &str| {
            let body = json!({"name": name, "cwd": workspace.to_string_lossy()}).to_string();
            api_json_request(&host, Method::POST, "/api/sessions", &body, true)
        };
        let (status, base) = round_trip_json(&router, create_draft("base")).await;
        assert_eq!(status, StatusCode::CREATED, "{base}");
        let base_draft = base["id"].as_str().unwrap().to_string();
        let start = Instant::now();
        let (status, receipt) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{base_draft}/submit"),
                r#"{"request_id":1,"prompt":"BASE"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{receipt}");
        assert!(start.elapsed() < Duration::from_secs(1));
        let base_id = wait_for_fixture_id(&fixture_dir, "BASE").await;
        wait_for_receipt(&host, &router, &base_id, 1).await;

        let (status, failed) = round_trip_json(&router, create_draft("failed-start")).await;
        assert_eq!(status, StatusCode::CREATED, "{failed}");
        let failed_draft = failed["id"].as_str().unwrap().to_string();
        let (status, failed_acceptance) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{failed_draft}/submit"),
                r#"{"request_id":1,"prompt":"FAIL-START"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{failed_acceptance}");
        let failed_snapshot = wait_for_receipt(&host, &router, &failed_draft, 1).await;
        assert_eq!(failed_snapshot["last_submission"]["status"], "incomplete");
        assert_eq!(failed_snapshot["high_water"], 1);
        let (status, failed_replay) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{failed_draft}/submit"),
                r#"{"request_id":1,"prompt":"FAIL-START"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{failed_replay}");
        assert_eq!(failed_replay["duplicate"], true);
        assert_eq!(
            fs::read_to_string(&log_path)
                .unwrap()
                .lines()
                .filter(|line| line.ends_with("|FAIL-START"))
                .count(),
            1
        );

        let (status, failed_before_trail) =
            round_trip_json(&router, create_draft("failed-before-trail")).await;
        assert_eq!(status, StatusCode::CREATED, "{failed_before_trail}");
        let failed_draft = failed_before_trail["id"].as_str().unwrap().to_string();
        let (status, accepted_failure) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{failed_draft}/submit"),
                r#"{"request_id":1,"prompt":"FAIL-BEFORE-TRAIL"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted_failure}");
        let failed_snapshot = wait_for_receipt(&host, &router, &failed_draft, 1).await;
        assert_eq!(failed_snapshot["last_submission"]["status"], "incomplete");
        assert_eq!(
            failed_snapshot["last_submission"]["session_id"],
            failed_draft
        );
        let aliases = host.state.inner.durable.read(|data| {
            data.aliases
                .iter()
                .filter(|(_, ledger_id)| *ledger_id == &failed_draft)
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>()
        });
        assert!(
            aliases.is_empty(),
            "failed start left ghost aliases: {aliases:?}"
        );

        let (status, retry_acceptance) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{failed_draft}/submit"),
                r#"{"request_id":2,"prompt":"RETRY-AFTER-FAIL"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{retry_acceptance}");
        let retry_id = wait_for_fixture_id(&fixture_dir, "RETRY-AFTER-FAIL").await;
        let retry_snapshot = wait_for_receipt(&host, &router, &retry_id, 2).await;
        assert_eq!(retry_snapshot["last_submission"]["session_id"], retry_id);
        let aliases = host.state.inner.durable.read(|data| {
            data.aliases
                .iter()
                .filter(|(_, ledger_id)| *ledger_id == &failed_draft)
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(aliases, vec![retry_id.clone()]);
        let (status, draft_retry_snapshot) = round_trip_json(
            &router,
            api_request(&host, &format!("/api/sessions/{failed_draft}/snapshot")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{draft_retry_snapshot}");
        assert_eq!(draft_retry_snapshot["session"]["id"], retry_id);

        let (status, held) = round_trip_json(&router, create_draft("held-A")).await;
        assert_eq!(status, StatusCode::CREATED, "{held}");
        let held_draft = held["id"].as_str().unwrap().to_string();
        let start = Instant::now();
        let (status, accepted_a) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{held_draft}/submit"),
                r#"{"request_id":1,"prompt":"HOLD-A"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted_a}");
        assert_eq!(accepted_a["session_id"], held_draft);
        assert!(start.elapsed() < Duration::from_secs(1));
        let held_id = wait_for_fixture_id(&fixture_dir, "HOLD-A").await;

        let (status, workspace_busy) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::PUT,
                &format!("/api/sessions/{held_draft}/workspace"),
                &json!({"cwd": workspace.to_string_lossy()}).to_string(),
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{workspace_busy}");
        assert_eq!(workspace_busy["error"], "session_busy");

        let start = Instant::now();
        let (status, list) = round_trip_json(&router, api_request(&host, "/api/sessions")).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert!(start.elapsed() < Duration::from_secs(1));
        let start = Instant::now();
        let (status, held_snapshot) = round_trip_json(
            &router,
            api_request(&host, &format!("/api/sessions/{held_draft}/snapshot")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{held_snapshot}");
        assert!(start.elapsed() < Duration::from_secs(1));

        let (status, duplicate) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{held_id}/submit"),
                r#"{"request_id":1,"prompt":"HOLD-A"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{duplicate}");
        assert_eq!(duplicate["duplicate"], true);
        let (status, draft_duplicate) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{held_draft}/submit"),
                r#"{"request_id":1,"prompt":"HOLD-A"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{draft_duplicate}");
        assert_eq!(draft_duplicate["duplicate"], true);
        let (status, conflict) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{held_id}/submit"),
                r#"{"request_id":1,"prompt":"different prompt"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["error"], "submission_id_conflict");

        for alias in [&held_draft, &held_id] {
            for (request_id, prompt, expected_error) in [
                (0, "stale alias request", "submission_id_stale"),
                (2, "busy alias request", "session_busy"),
                (3, "unexpected alias request", "submission_id_unexpected"),
                (1, "conflicting alias request", "submission_id_conflict"),
            ] {
                let body = json!({"request_id": request_id, "prompt": prompt}).to_string();
                let (status, rejected) = round_trip_json(
                    &router,
                    api_json_request(
                        &host,
                        Method::POST,
                        &format!("/api/sessions/{alias}/submit"),
                        &body,
                        true,
                    ),
                )
                .await;
                assert_eq!(status, StatusCode::CONFLICT, "{alias}: {rejected}");
                assert_eq!(rejected["error"], expected_error, "{alias}: {rejected}");
            }
        }

        let (status, draft_c) = round_trip_json(&router, create_draft("independent-C")).await;
        assert_eq!(status, StatusCode::CREATED, "{draft_c}");
        let independent_draft = draft_c["id"].as_str().unwrap().to_string();
        let start = Instant::now();
        let (status, accepted_c) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{independent_draft}/submit"),
                r#"{"request_id":1,"prompt":"QUICK-C"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted_c}");
        assert_eq!(accepted_c["session_id"], independent_draft);
        assert!(start.elapsed() < Duration::from_secs(1));

        let start = Instant::now();
        let (status, accepted_b) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{base_id}/submit"),
                r#"{"request_id":2,"prompt":"B-ONCE"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted_b}");
        assert!(start.elapsed() < Duration::from_secs(1));
        wait_for_receipt(&host, &router, &base_id, 2).await;
        let log = fs::read_to_string(&log_path).unwrap();
        assert_eq!(
            log.lines().filter(|line| line.ends_with("|B-ONCE")).count(),
            1,
            "bound session B must execute one exchange: {log}"
        );

        fs::write(&release_files[0], "release").unwrap();
        wait_for_receipt(&host, &router, &held_id, 1).await;
        let (status, rebound_snapshot) = round_trip_json(
            &router,
            api_request(&host, &format!("/api/sessions/{held_draft}/snapshot")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rebound_snapshot}");
        assert_eq!(rebound_snapshot["session"]["id"], held_id);
        let mut alias_stop = api_request(&host, &format!("/api/sessions/{held_draft}/stop"));
        *alias_stop.method_mut() = Method::POST;
        let (status, stopped_alias) = round_trip_json(&router, alias_stop).await;
        assert_eq!(status, StatusCode::OK, "{stopped_alias}");
        let rebound_events = router
            .clone()
            .oneshot(api_request(
                &host,
                &format!("/api/sessions/{held_draft}/events?after=0"),
            ))
            .await
            .unwrap();
        assert_eq!(rebound_events.status(), StatusCode::OK);
        drop(rebound_events);
        let independent_id = wait_for_fixture_id(&fixture_dir, "QUICK-C").await;
        wait_for_receipt(&host, &router, &independent_id, 1).await;

        let (status, held_d) = round_trip_json(&router, create_draft("held-D-stop")).await;
        assert_eq!(status, StatusCode::CREATED, "{held_d}");
        let stopped_draft = held_d["id"].as_str().unwrap().to_string();
        let (status, accepted_d) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{stopped_draft}/submit"),
                r#"{"request_id":1,"prompt":"HOLD-D"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted_d}");
        let stopped_id = wait_for_fixture_id(&fixture_dir, "HOLD-D").await;
        let mut stop_request = api_request(&host, &format!("/api/sessions/{stopped_id}/stop"));
        *stop_request.method_mut() = Method::POST;
        let (status, stop) = round_trip_json(&router, stop_request).await;
        assert_eq!(status, StatusCode::OK, "{stop}");
        assert_eq!(stop["status"], "stop_requested");
        let stopped_snapshot = wait_for_receipt(&host, &router, &stopped_draft, 1).await;
        assert!(matches!(
            stopped_snapshot["last_submission"]["status"].as_str(),
            Some("stopped" | "incomplete")
        ));
        assert!(stopped_snapshot["active"].is_null());
        assert_eq!(
            stopped_snapshot["last_submission"]["session_id"],
            stopped_draft
        );

        let (status, pending_stop_draft_value) =
            round_trip_json(&router, create_draft("pending-stop")).await;
        assert_eq!(status, StatusCode::CREATED, "{pending_stop_draft_value}");
        let pending_stop_draft = pending_stop_draft_value["id"].as_str().unwrap();
        let pending_stop_session = host.state.inner.catalog.allocate_new_session_id().unwrap();
        let pending_stop = PendingStop::new();
        pending_stop.stop().unwrap();
        let mut pending_turn = host
            .state
            .start_turn(
                pending_stop_draft,
                "HOLD-PENDING-STOP",
                Some(&pending_stop_session),
            )
            .unwrap();
        let _ = wait_for_fixture_id(&fixture_dir, "HOLD-PENDING-STOP").await;
        pending_stop.attach(pending_turn.stop_handle());
        let pending_outcome = pending_turn.wait().unwrap();
        assert!(
            matches!(pending_outcome, TurnOutcome::Stopped { .. }),
            "pending Stop completed with {pending_outcome:?}"
        );
        assert!(
            !fixture_dir.join("HOLD-PENDING-STOP.finished").exists(),
            "a run stopped before handle attachment executed its prompt"
        );

        let (status, cancelled_draft_value) =
            round_trip_json(&router, create_draft("cancelled-acceptance")).await;
        assert_eq!(status, StatusCode::CREATED, "{cancelled_draft_value}");
        let cancelled_draft = cancelled_draft_value["id"].as_str().unwrap().to_string();
        let (pause_entered_tx, pause_entered_rx) = std::sync::mpsc::channel();
        let (pause_release_tx, pause_release_rx) = std::sync::mpsc::channel();
        *lock(&host.state.inner.test_hooks.pause_after_durable_receipt) = Some(AcceptancePause {
            entered: pause_entered_tx,
            release: Mutex::new(pause_release_rx),
        });
        let first_request = api_json_request(
            &host,
            Method::POST,
            &format!("/api/sessions/{cancelled_draft}/submit"),
            r#"{"request_id":1,"prompt":"HOLD-CANCELLED"}"#,
            true,
        );
        let first_router = router.clone();
        let cancelled_submit =
            tokio::spawn(async move { round_trip_json(&first_router, first_request).await });
        tokio::task::spawn_blocking(move || {
            pause_entered_rx
                .recv()
                .expect("acceptance paused after durable receipt creation");
        })
        .await
        .unwrap();
        cancelled_submit.abort();
        assert!(cancelled_submit.await.unwrap_err().is_cancelled());

        let acceptance_gate = host.state.acceptance_gate(&cancelled_draft);
        assert!(
            acceptance_gate.try_lock().is_err(),
            "cancelled HTTP future released the acceptance gate"
        );
        let (status, paused_snapshot) = round_trip_json(
            &router,
            api_request(&host, &format!("/api/sessions/{cancelled_draft}/snapshot")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{paused_snapshot}");
        assert_eq!(paused_snapshot["high_water"], 1);
        assert_eq!(paused_snapshot["last_submission"]["request_id"], 1);
        assert_eq!(paused_snapshot["last_submission"]["status"], "accepted");
        assert!(paused_snapshot["active"].is_null());

        let (gate_attempt_tx, mut gate_attempt_rx) = tokio::sync::mpsc::unbounded_channel();
        *lock(&host.state.inner.test_hooks.gate_attempts) = Some(gate_attempt_tx);
        let second_request = api_json_request(
            &host,
            Method::POST,
            &format!("/api/sessions/{cancelled_draft}/submit"),
            r#"{"request_id":2,"prompt":"SECOND"}"#,
            true,
        );
        let second_router = router.clone();
        let second_submit =
            tokio::spawn(async move { round_trip_json(&second_router, second_request).await });
        assert_eq!(gate_attempt_rx.recv().await, Some("submit"));

        let workspace_body = json!({"cwd": workspace.to_string_lossy()}).to_string();
        let workspace_request = api_json_request(
            &host,
            Method::PUT,
            &format!("/api/sessions/{cancelled_draft}/workspace"),
            &workspace_body,
            true,
        );
        let workspace_router = router.clone();
        let workspace_change =
            tokio::spawn(
                async move { round_trip_json(&workspace_router, workspace_request).await },
            );
        assert_eq!(gate_attempt_rx.recv().await, Some("workspace"));

        let mut stop_request = api_request(&host, &format!("/api/sessions/{cancelled_draft}/stop"));
        *stop_request.method_mut() = Method::POST;
        let stop_router = router.clone();
        let stop = tokio::spawn(async move { round_trip_json(&stop_router, stop_request).await });
        assert_eq!(gate_attempt_rx.recv().await, Some("stop"));

        assert!(!second_submit.is_finished());
        assert!(!workspace_change.is_finished());
        assert!(!stop.is_finished());
        let durable_receipts = host.state.inner.durable.read(|data| {
            let ledger = data.sessions.get(&cancelled_draft).unwrap();
            (ledger.high_water, ledger.receipts.len())
        });
        assert_eq!(durable_receipts, (1, 1));
        let first_turn_id = paused_snapshot["last_submission"]["turn_id"]
            .as_str()
            .unwrap()
            .to_string();

        pause_release_tx.send(()).unwrap();
        let (status, busy) = second_submit.await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{busy}");
        assert_eq!(busy["error"], "session_busy");
        let (status, workspace_busy) = workspace_change.await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{workspace_busy}");
        assert_eq!(workspace_busy["error"], "session_busy");
        let (status, stopped) = stop.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{stopped}");
        assert_eq!(stopped["status"], "stop_requested");
        assert_eq!(stopped["turn_id"], first_turn_id);

        let (status, after_acceptance) = round_trip_json(
            &router,
            api_request(&host, &format!("/api/sessions/{cancelled_draft}/snapshot")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{after_acceptance}");
        assert_eq!(after_acceptance["high_water"], 1);
        assert_eq!(after_acceptance["last_submission"]["request_id"], 1);
        assert_eq!(
            after_acceptance["last_submission"]["turn_id"],
            first_turn_id
        );
        let (status, replay) = round_trip_json(
            &router,
            api_json_request(
                &host,
                Method::POST,
                &format!("/api/sessions/{cancelled_draft}/submit"),
                r#"{"request_id":1,"prompt":"HOLD-CANCELLED"}"#,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["duplicate"], true);
        assert_eq!(replay["turn_id"], first_turn_id);

        fs::write(&release_files[2], "release").unwrap();
        wait_for_receipt(&host, &router, &cancelled_draft, 1).await;
        let log = fs::read_to_string(&log_path).unwrap();
        assert_eq!(
            log.lines()
                .filter(|line| line.ends_with("|HOLD-CANCELLED"))
                .count(),
            1,
            "same-ID replay started another execution: {log}"
        );

        host.state.shutdown();
        let _ = watchdog_tx.send(());
        watchdog.join().unwrap();
    }

    #[cfg(unix)]
    async fn wait_for_fixture_id(fixture_dir: &Path, prompt: &str) -> String {
        let path = fixture_dir.join(format!("{prompt}.id"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(id) = fs::read_to_string(&path) {
                return id;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture did not start {prompt}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_receipt(
        host: &Host,
        router: &Router,
        session_id: &str,
        request_id: u64,
    ) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (status, snapshot) = round_trip_json(
                router,
                api_request(host, &format!("/api/sessions/{session_id}/snapshot")),
            )
            .await;
            if status == StatusCode::OK
                && snapshot["last_submission"]["request_id"].as_u64() == Some(request_id)
                && matches!(
                    snapshot["last_submission"]["status"].as_str(),
                    Some("succeeded" | "failed" | "stopped" | "incomplete")
                )
            {
                return snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "receipt did not finish: status={status} snapshot={snapshot}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    impl Host {
        fn inner_sessions(&self) -> Vec<CatalogSession> {
            self.state.inner.catalog.list().unwrap()
        }

        fn inner_state_dir(&self) -> &Path {
            self.state.inner.catalog.state_dir()
        }
    }
}
