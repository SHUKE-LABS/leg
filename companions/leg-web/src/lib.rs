//! Authenticated loopback HTTP host for companion session control.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
const STORE_NAME: &str = "web-host-state.json";
const LOCK_NAME: &str = ".leg-web.lock";
const STORE_VERSION: u32 = 1;
const DEFAULT_EVENT_BUFFER: usize = 256;
const DEFAULT_RECEIPTS: usize = 128;
const MAX_BODY_BYTES: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Only `127.0.0.1:0` is accepted. The OS chooses the port.
    pub bind_addr: SocketAddr,
    pub catalog: SessionCatalogConfig,
    pub event_buffer: usize,
    pub receipt_limit: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            catalog: SessionCatalogConfig::default(),
            event_buffer: DEFAULT_EVENT_BUFFER,
            receipt_limit: DEFAULT_RECEIPTS,
        }
    }
}

#[derive(Debug)]
pub enum HostError {
    BindAddress(SocketAddr),
    Io(io::Error),
    Catalog(CatalogError),
    State(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindAddress(addr) => write!(
                f,
                "refusing Web bind address {addr}; leg-web requires 127.0.0.1 and an OS-assigned port"
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
                submit_gate: AsyncMutex::new(()),
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
        let listener = TcpListener::bind(self.bind_addr).await?;
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
    submit_gate: AsyncMutex<()>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    shutdown_tx: broadcast::Sender<()>,
    shutting_down: AtomicBool,
    // Holding the lock file keeps the single-host claim process-owned.
    _host_lock: File,
    event_buffer: usize,
    receipt_limit: usize,
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
    stop: TurnStopHandle,
    stop_requested: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveSnapshot {
    turn_id: String,
    prompt: String,
    text: String,
    status: String,
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
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/select", post(select_session))
        .route("/api/sessions/{id}", get(get_session).patch(rename_session))
        .route("/api/sessions/{id}/workspace", put(set_workspace))
        .route("/api/sessions/{id}/submit", post(submit_turn))
        .route("/api/sessions/{id}/stop", post(stop_turn))
        .route("/api/sessions/{id}/snapshot", get(get_snapshot))
        .route("/api/sessions/{id}/events", get(events))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard_request))
        .with_state(state)
}

async fn guard_request(State(state): State<HostState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
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
    } else if path != "/" && path != "/app.js" {
        return api_error(StatusCode::NOT_FOUND, "not_found").into_response();
    } else if request.method() != Method::GET {
        return api_error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed").into_response();
    }

    let mut response = next.run(request).await;
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
                "default-src 'none'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
            ),
        );
    }
    response
}

async fn index() -> Response {
    static_response("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    static_response("text/javascript; charset=utf-8", APP_JS)
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
    state.catalog_session(&id)?;
    if !body.cwd.is_absolute() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "workspace_must_be_absolute",
        ));
    }
    state
        .inner
        .catalog
        .set_workspace(&id, &body.cwd)
        .map_err(map_catalog_error)?;
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
    let _gate = state.inner.submit_gate.lock().await;
    if state.inner.shutting_down.load(Ordering::Acquire) {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "host_shutting_down",
        ));
    }
    let response = state.accept_submission(&id, body)?;
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
    let session = state.catalog_session(&id)?;
    let ledger_id = state.ledger_id(&id);
    let mut runtime = lock(&state.inner.runtime);
    let runtime_id = runtime_id(&runtime, &ledger_id);
    let Some(active) = runtime
        .sessions
        .get_mut(&runtime_id)
        .and_then(|entry| entry.active.as_mut())
    else {
        let latest = state.latest_receipt(&ledger_id);
        return Ok(Json(json!({
            "session_id": id,
            "status": latest.map(|r| format!("{:?}", r.status).to_ascii_lowercase()).unwrap_or_else(|| "idle".into()),
            "run_state": session.run_state,
        })));
    };
    if active.stop_requested {
        return Ok(Json(json!({
            "session_id": id,
            "turn_id": active.turn_id,
            "status": "stop_requested",
        })));
    }
    active.stop_requested = true;
    active
        .stop
        .stop()
        .map_err(|_| api_error(StatusCode::SERVICE_UNAVAILABLE, "stop_unavailable"))?;
    Ok(Json(json!({
        "session_id": id,
        "turn_id": active.turn_id,
        "status": "stop_requested",
    })))
}

async fn events(
    State(state): State<HostState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError>
{
    state.catalog_session(&id)?;
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
    fn catalog_session(&self, id: &str) -> Result<CatalogSession, ApiError> {
        if !valid_session_id(id) {
            return Err(api_error(StatusCode::NOT_FOUND, "session_not_found"));
        }
        self.inner.catalog.get(id).map_err(map_catalog_error)
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
        let session = self.catalog_session(id)?;
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

    fn accept_submission(&self, id: &str, body: SubmitTurn) -> Result<SubmissionReceipt, ApiError> {
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
                return Ok(SubmissionReceipt {
                    request_id: previous.request_id,
                    turn_id: previous.turn_id,
                    session_id: previous.session_id.unwrap_or_else(|| id.to_string()),
                    status: previous.status,
                    duplicate: true,
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
        let current = self.catalog_session(id)?;
        if current.run_state != CatalogRunState::Idle {
            return Err(api_error(StatusCode::CONFLICT, "session_busy"));
        }
        let turn_id = random_hex(16)
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "random_unavailable"))?;
        let accepted = Receipt {
            request_id: body.request_id,
            prompt_sha256: prompt_hash,
            turn_id: turn_id.clone(),
            status: ReceiptStatus::Accepted,
            session_id: Some(id.to_string()),
            outcome: None,
            recovery_evidence: None,
        };
        self.inner
            .durable
            .transact(|data| {
                let ledger = data.sessions.entry(ledger_id.clone()).or_default();
                ledger.high_water = body.request_id;
                ledger.receipts.push_back(accepted.clone());
                while ledger.receipts.len() > self.inner.receipt_limit {
                    ledger.receipts.pop_front();
                }
                Ok(())
            })
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))?;

        let turn = self.start_turn(id, &body.prompt);
        let mut turn = match turn {
            Ok(turn) => turn,
            Err(error) => {
                if matches!(error, CatalogError::Busy) {
                    self.rollback_accepted_submission(&ledger_id, body.request_id, &turn_id)?;
                    return Err(map_catalog_error(error));
                }
                let error_value = json!({"status": "incomplete"});
                self.finish_receipt(
                    &ledger_id,
                    body.request_id,
                    ReceiptStatus::Incomplete,
                    None,
                    error_value.clone(),
                )?;
                self.append_event(&ledger_id, id, &turn_id, "outcome", error_value)?;
                return Ok(SubmissionReceipt {
                    request_id: body.request_id,
                    turn_id,
                    session_id: id.to_string(),
                    status: ReceiptStatus::Incomplete,
                    duplicate: false,
                });
            }
        };
        let accepted_event = self.append_event(
            &ledger_id,
            id,
            &turn_id,
            "accepted",
            json!({"request_id": body.request_id}),
        );
        let stop = turn.stop_handle();
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
                stop,
                stop_requested: false,
            });
            entry.live = Some(LiveSnapshot {
                turn_id: turn_id.clone(),
                prompt: body.prompt.clone(),
                text: String::new(),
                status: "starting".into(),
                active_tool: None,
                tools: Vec::new(),
            });
        }
        let running_receipt = self.finish_receipt(
            &ledger_id,
            body.request_id,
            ReceiptStatus::Running,
            Some(id.to_string()),
            json!({"status": "running"}),
        );
        let state = self.clone();
        let turn_id_worker = turn_id.clone();
        let requested_id = id.to_string();
        let ledger_id_worker = ledger_id.clone();
        let request_id = body.request_id;
        let (bound_id_tx, bound_id_rx) = if id.starts_with("draft-") {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let worker = thread::Builder::new()
            .name("leg-web-turn-owner".into())
            .spawn(move || {
                let mut bound_id = requested_id.clone();
                let mut bound_id_sent = false;
                loop {
                    match turn.observe() {
                        Ok(Some(event)) => {
                            if let StreamEvent::TurnStart {
                                session_id: Some(session_id),
                                ..
                            } = &event
                            {
                                bound_id = session_id.clone();
                                if requested_id.starts_with("draft-")
                                    && requested_id != session_id.as_str()
                                {
                                    let _ = state.bind_alias(&requested_id, session_id);
                                }
                                if let Some(sender) = bound_id_tx.as_ref() {
                                    let _ = sender.send(bound_id.clone());
                                    bound_id_sent = true;
                                }
                            }
                            let _ = state.record_stream_event(
                                &ledger_id_worker,
                                &bound_id,
                                &turn_id_worker,
                                event,
                            );
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                // Stream EOF is not a successful outcome; wait() supplies the
                // driver's correlated response and process status.
                let (status, outcome) = match turn.wait() {
                    Ok(outcome) => outcome_summary(&outcome),
                    Err(_) => (ReceiptStatus::Incomplete, json!({"status": "incomplete"})),
                };
                if !bound_id_sent && let Some(sender) = bound_id_tx.as_ref() {
                    let _ = sender.send(bound_id.clone());
                }
                let _ = state.finish_receipt(
                    &ledger_id_worker,
                    request_id,
                    status,
                    Some(bound_id.clone()),
                    outcome.clone(),
                );
                let _ = state.append_event(
                    &ledger_id_worker,
                    &bound_id,
                    &turn_id_worker,
                    "outcome",
                    outcome,
                );
                let mut runtime = lock(&state.inner.runtime);
                let key = runtime_id(&runtime, &ledger_id_worker);
                if let Some(entry) = runtime.sessions.get_mut(&key) {
                    entry.active = None;
                    entry.live = None;
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(_) => {
                let incomplete = json!({"status": "incomplete"});
                let receipt_result = self.finish_receipt(
                    &ledger_id,
                    body.request_id,
                    ReceiptStatus::Incomplete,
                    Some(id.to_string()),
                    incomplete.clone(),
                );
                let _ = self.append_event(&ledger_id, id, &turn_id, "outcome", incomplete);
                let mut runtime = lock(&self.inner.runtime);
                let key = runtime_id(&runtime, &ledger_id);
                if let Some(entry) = runtime.sessions.get_mut(&key) {
                    entry.active = None;
                    entry.live = None;
                }
                receipt_result?;
                return Ok(SubmissionReceipt {
                    request_id: body.request_id,
                    turn_id,
                    session_id: id.to_string(),
                    status: ReceiptStatus::Incomplete,
                    duplicate: false,
                });
            }
        };
        self.reap_finished_workers();
        lock(&self.inner.workers).push(worker);
        let receipt_session_id = bound_id_rx
            .and_then(|receiver| receiver.recv_timeout(Duration::from_secs(10)).ok())
            .unwrap_or_else(|| id.to_string());
        running_receipt?;
        accepted_event?;
        Ok(SubmissionReceipt {
            request_id: body.request_id,
            turn_id,
            session_id: receipt_session_id,
            status: ReceiptStatus::Running,
            duplicate: false,
        })
    }

    fn rollback_accepted_submission(
        &self,
        ledger_id: &str,
        request_id: u64,
        turn_id: &str,
    ) -> Result<(), ApiError> {
        self.inner
            .durable
            .transact(|data| {
                let ledger = data
                    .sessions
                    .get_mut(ledger_id)
                    .ok_or_else(|| HostError::State("accepted submission disappeared".into()))?;
                let expected_previous = request_id
                    .checked_sub(1)
                    .ok_or_else(|| HostError::State("invalid accepted request id".into()))?;
                if ledger.high_water != request_id
                    || !ledger.receipts.back().is_some_and(|receipt| {
                        receipt.request_id == request_id
                            && receipt.turn_id == turn_id
                            && receipt.status == ReceiptStatus::Accepted
                    })
                {
                    return Err(HostError::State(
                        "accepted submission changed before rollback".into(),
                    ));
                }
                ledger.receipts.pop_back();
                ledger.high_water = expected_previous;
                Ok(())
            })
            .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))
    }

    fn start_turn(&self, id: &str, prompt: &str) -> Result<CatalogTurn, CatalogError> {
        if id.starts_with("draft-") {
            return self
                .inner
                .catalog
                .start_new(id, SessionInterface::Web, prompt.to_string());
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

    fn bind_alias(&self, draft_id: &str, session_id: &str) -> Result<(), ApiError> {
        self.inner
            .durable
            .transact(|data| {
                data.aliases
                    .insert(session_id.to_string(), draft_id.to_string());
                if let Some(ledger) = data.sessions.get_mut(draft_id)
                    && let Some(receipt) = ledger.receipts.back_mut()
                {
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
        let value = stream_event_value(&event);
        {
            let mut runtime = lock(&self.inner.runtime);
            let key = runtime_id(&runtime, ledger_id);
            let entry = runtime
                .sessions
                .entry(key)
                .or_insert_with(SessionRuntime::new);
            if let Some(live) = entry.live.as_mut() {
                live.status = "running".into();
                match &event {
                    StreamEvent::TextDelta { text, .. } => live.text.push_str(text),
                    StreamEvent::ToolCall {
                        tool_use_id,
                        tool_name,
                        input,
                        ..
                    } => {
                        let tool = json!({"tool_use_id": tool_use_id, "tool_name": tool_name, "input": input, "status": "running"});
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
                        }
                        live.active_tool = None;
                    }
                    _ => {}
                }
            }
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
            let session_id = self.inner.durable.read(|data| {
                data.aliases
                    .iter()
                    .find_map(|(actual, draft)| (draft == &id).then(|| actual.clone()))
                    .unwrap_or_else(|| id.clone())
            });
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

fn validate_bind_address(addr: SocketAddr) -> Result<(), HostError> {
    if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || addr.port() != 0 {
        return Err(HostError::BindAddress(addr));
    }
    Ok(())
}

fn authority_for(addr: SocketAddr) -> String {
    format!("127.0.0.1:{}", addr.port())
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
    file.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
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

    fn api_request(host: &Host, path: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri(path)
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token))
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn refuses_non_loopback_and_fixed_port_configuration() {
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap()).is_err());
        assert!(validate_bind_address("127.0.0.1:8080".parse().unwrap()).is_err());
        assert!(validate_bind_address("127.0.0.1:0".parse().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn binding_uses_loopback_and_the_os_assigned_port() {
        let temp = TempDir::new().unwrap();
        let host = Host::open(HostConfig {
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
        assert!(APP_JS.contains("location.hash"));
        assert!(APP_JS.contains("sessionStorage"));
        assert!(APP_JS.contains("replaceState"));
    }

    #[tokio::test]
    async fn malformed_json_unknown_ids_and_traversal_have_no_effect() {
        let (host, _temp) = test_host();
        let router = build_router(host.state.clone());
        let malformed = HttpRequest::builder()
            .method(Method::POST)
            .uri("/api/sessions")
            .header(HOST, "127.0.0.1:43127")
            .header(ORIGIN, "http://127.0.0.1:43127")
            .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from("{bad json"))
            .unwrap();
        assert_eq!(
            router.clone().oneshot(malformed).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let unknown = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/api/sessions/unknown/submit")
                    .header(HOST, "127.0.0.1:43127")
                    .header(ORIGIN, "http://127.0.0.1:43127")
                    .header(AUTHORIZATION, format!("Bearer {}", host.state.inner.token))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"request_id":1,"prompt":"start"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let traversal = router
            .oneshot(api_request(&host, "/api/sessions/%2e%2e%2fetc%2fpasswd"))
            .await
            .unwrap();
        assert_eq!(traversal.status(), StatusCode::NOT_FOUND);
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
    fn busy_start_rollback_restores_the_unspent_request_id() {
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
                    .push_back(receipt(2, ReceiptStatus::Accepted));
                Ok(())
            })
            .unwrap();

        host.state
            .rollback_accepted_submission("session-a", 2, "turn-2")
            .unwrap();

        host.state.inner.durable.read(|data| {
            let ledger = data.sessions.get("session-a").unwrap();
            assert_eq!(ledger.high_water, 1);
            assert_eq!(ledger.receipts.len(), 1);
            assert_eq!(ledger.receipts[0].request_id, 1);
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

    impl Host {
        fn inner_sessions(&self) -> Vec<CatalogSession> {
            self.state.inner.catalog.list().unwrap()
        }

        fn inner_state_dir(&self) -> &Path {
            self.state.inner.catalog.state_dir()
        }
    }
}
