//! Shared local session catalog for companion interfaces.
//!
//! Catalog records contain UI metadata only. The leg-owned JSONL trail under
//! the catalog's sessions directory remains the source of provider history.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::client::{
    Client, ClientConfig, ClientError, LegSession, StartError, TurnHandle, TurnOutcome,
    TurnRequest, TurnStopHandle, new_native_session_id,
};
use crate::process_owner::{OwnerState, ProcessOwner};
use crate::protocol::StreamEvent;
use crate::supervisor;

const INDEX_NAME: &str = "catalog.json";
const LOCK_NAME: &str = ".catalog.lock";
const INDEX_VERSION: u32 = 1;
const EXCHANGE_SCHEMA: &str = "baton.exchange/v1";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default)]
pub struct SessionCatalogConfig {
    /// Override the companion state root. LEG_UI_STATE_DIR is used when absent.
    pub state_dir: Option<PathBuf>,
    /// Native leg executable or recognized npm launcher.
    pub leg_bin: Option<PathBuf>,
    /// Companion supervisor executable.
    pub supervisor_bin: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SessionInterface {
    Tui,
    Web,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRunState {
    Idle,
    Active,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrailOutcome {
    Succeeded,
    Failed,
    Interrupted,
    Incomplete,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TrailToolResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TrailTool {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TrailToolResult>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub struct TrailTurn {
    pub turn_index: u64,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    #[serde(skip)]
    retry_prompt: Option<String>,
    pub outcome: TrailOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    #[serde(default)]
    pub tool_rounds: Vec<Value>,
    #[serde(default)]
    pub tools: Vec<TrailTool>,
}

impl fmt::Debug for TrailTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrailTurn")
            .field("turn_index", &self.turn_index)
            .field("prompt", &self.prompt)
            .field("outcome", &self.outcome)
            .field("reply", &self.reply)
            .field("failure_kind", &self.failure_kind)
            .field("failure_message", &self.failure_message)
            .field("tool_rounds", &self.tool_rounds)
            .field("tools", &self.tools)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogSession {
    pub id: String,
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub drafts: BTreeMap<SessionInterface, String>,
    pub display: BTreeMap<String, Value>,
    pub turns: Vec<TrailTurn>,
    pub warnings: Vec<String>,
    pub recovered: bool,
    pub read_only: bool,
    pub run_state: CatalogRunState,
    pub pending_new_turn: bool,
    pub ended: bool,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("session catalog I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("session catalog JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session id; use only ASCII letters, digits, '-' and '_'")]
    InvalidSessionId,
    #[error("session {0:?} is not in the managed catalog or sessions directory")]
    NotFound(String),
    #[error("session {0:?} already exists in the catalog")]
    AlreadyExists(String),
    #[error("session {0:?} has no selected workspace")]
    WorkspaceRequired(String),
    #[error("workspace {0:?} is missing or is no longer the recorded directory")]
    WorkspaceMissing(PathBuf),
    #[error("session is busy: another companion turn owns it")]
    Busy,
    #[error("could not verify companion process ownership: {0}")]
    Driver(String),
    #[error("session {0:?} has an unreadable trail and is read-only")]
    ReadOnly(String),
    #[error("session {0:?} has no failed or incomplete turn to retry")]
    NoRetry(String),
    #[error("retry confirmation is stale; inspect the latest turn again")]
    StaleRetry,
    #[error("session catalog could not be updated: {0}")]
    Catalog(String),
    #[error(transparent)]
    Start(#[from] StartError),
}

#[derive(Debug, Error)]
pub enum CatalogTurnError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("session catalog could not be updated: {0}")]
    Catalog(String),
}

#[derive(Clone)]
pub struct RetryIntent {
    session_id: String,
    turn_index: u64,
    prompt: String,
}

impl RetryIntent {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn warning(&self) -> &'static str {
        "Retry sends this prompt again and may repeat tool side effects."
    }
}

#[derive(Clone)]
pub struct SessionCatalog {
    inner: Arc<CatalogInner>,
}

struct CatalogInner {
    state_dir: PathBuf,
    sessions_dir: PathBuf,
    leg_bin: Option<PathBuf>,
    supervisor_bin: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct CatalogIndex {
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, SessionMetadata>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SessionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<PathBuf>,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    updated_at_ms: u64,
    #[serde(default)]
    drafts: BTreeMap<SessionInterface, String>,
    #[serde(default)]
    display: BTreeMap<String, Value>,
    #[serde(default)]
    pending_new_turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_attempt: Option<PendingAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bound_attempt_token: Option<String>,
    #[serde(default)]
    recovered: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingAttempt {
    #[serde(default)]
    token: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    controller: Option<ProcessOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supervisor: Option<ProcessOwner>,
    #[serde(default)]
    candidate_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_session_id: Option<String>,
    #[serde(default)]
    controller_released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery_warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecoveryDecision {
    Active,
    Unknown,
    Clear,
    Bind(String),
}

impl SessionMetadata {
    fn new(name: Option<String>, cwd: Option<PathBuf>) -> Self {
        let now = now_ms();
        Self {
            name,
            cwd,
            created_at_ms: now,
            updated_at_ms: now,
            ..Self::default()
        }
    }
}

fn merge_pending_session_metadata(
    mut session: SessionMetadata,
    draft: SessionMetadata,
    attempt_token: &str,
) -> SessionMetadata {
    // Keep edits made on the recovered candidate; fill only metadata it lacks from the draft.
    if session.name.is_none() {
        session.name = draft.name;
    }
    if session.cwd.is_none() {
        session.cwd = draft.cwd;
    }
    if session.created_at_ms == 0
        || (draft.created_at_ms != 0 && draft.created_at_ms < session.created_at_ms)
    {
        session.created_at_ms = draft.created_at_ms;
    }
    for (interface, text) in draft.drafts {
        session.drafts.entry(interface).or_insert(text);
    }
    for (key, value) in draft.display {
        session.display.entry(key).or_insert(value);
    }
    session.pending_new_turn = false;
    session.pending_attempt = None;
    session.bound_attempt_token = Some(attempt_token.to_string());
    session.recovered = false;
    session.updated_at_ms = now_ms();
    session
}

struct TrailSnapshot {
    turns: Vec<TrailTurn>,
    warnings: Vec<String>,
    read_only: bool,
    ended: bool,
}

impl SessionCatalog {
    pub fn open(config: SessionCatalogConfig) -> Result<Self, CatalogError> {
        let state_dir = resolve_state_dir(config.state_dir.as_deref())?;
        fs::create_dir_all(&state_dir)?;
        set_private_directory(&state_dir)?;
        let state_dir = fs::canonicalize(state_dir)?;

        let sessions_dir = state_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;
        set_private_directory(&sessions_dir)?;
        let sessions_dir = fs::canonicalize(sessions_dir)?;

        let catalog = Self {
            inner: Arc::new(CatalogInner {
                state_dir,
                sessions_dir,
                leg_bin: config.leg_bin,
                supervisor_bin: config.supervisor_bin,
            }),
        };
        {
            let _guard = catalog.lock_index()?;
            if !catalog.index_path().exists() {
                catalog.write_index_unlocked(&CatalogIndex {
                    version: INDEX_VERSION,
                    sessions: BTreeMap::new(),
                })?;
            } else {
                let _ = catalog.read_index_unlocked()?;
            }
        }
        catalog.reconcile_pending_attempts(None)?;
        Ok(catalog)
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.inner.sessions_dir
    }

    /// Creates an unsubmitted UI record. The workspace may remain unset until
    /// a person explicitly chooses an existing directory.
    pub fn create_draft(
        &self,
        interface: SessionInterface,
        name: Option<String>,
        cwd: Option<&Path>,
    ) -> Result<CatalogSession, CatalogError> {
        let cwd = cwd.map(canonical_workspace).transpose()?;
        let id = new_draft_id();
        let index = self.update_index(|index| {
            let mut record = SessionMetadata::new(name, cwd);
            record.drafts.insert(interface, String::new());
            index.sessions.insert(id.clone(), record);
            Ok(())
        })?;
        self.session_from_metadata(&id, index.sessions.get(&id).expect("draft inserted"), false)
    }

    pub fn rename(&self, id: &str, name: Option<String>) -> Result<(), CatalogError> {
        self.mutate_metadata(id, |record| {
            record.name = name;
            Ok(())
        })
    }

    pub fn save_draft(
        &self,
        id: &str,
        interface: SessionInterface,
        draft: String,
    ) -> Result<(), CatalogError> {
        self.mutate_metadata(id, |record| {
            record.drafts.insert(interface, draft);
            Ok(())
        })
    }

    pub fn save_display_metadata(
        &self,
        id: &str,
        key: String,
        value: Value,
    ) -> Result<(), CatalogError> {
        if key.trim().is_empty() {
            return Err(CatalogError::Catalog(
                "display metadata key is blank".into(),
            ));
        }
        self.mutate_metadata(id, |record| {
            record.display.insert(key, value);
            Ok(())
        })
    }

    /// Changes a recorded workspace only while the companion driver confirms
    /// that no owned leg/tool process holds the session lock.
    pub fn set_workspace(&self, id: &str, cwd: &Path) -> Result<(), CatalogError> {
        validate_session_id(id)?;
        let cwd = canonical_workspace(cwd)?;
        if is_draft_id(id) {
            self.reconcile_pending_attempts(Some(id))?;
        }
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if !index.sessions.contains_key(id) {
            if !self.managed_trail_exists(id) {
                return Err(CatalogError::NotFound(id.into()));
            }
            let mut record = SessionMetadata::new(None, None);
            record.recovered = true;
            index.sessions.insert(id.to_string(), record);
        }
        let record = index.sessions.get_mut(id).expect("record inserted above");
        if is_draft_id(id) {
            if record.pending_new_turn {
                return Err(CatalogError::Busy);
            }
        } else {
            let session_guard = supervisor::try_session_lock(&self.inner.sessions_dir, id)
                .map_err(CatalogError::Driver)?
                .ok_or(CatalogError::Busy)?;
            record.cwd = Some(cwd);
            record.recovered = false;
            record.updated_at_ms = now_ms();
            self.write_index_unlocked(&index)?;
            drop(session_guard);
            return Ok(());
        }
        record.cwd = Some(cwd);
        record.recovered = false;
        record.updated_at_ms = now_ms();
        self.write_index_unlocked(&index)
    }

    pub fn get(&self, id: &str) -> Result<CatalogSession, CatalogError> {
        validate_session_id(id)?;
        self.reconcile_pending_attempts(Some(id))?;
        let index = self.read_index()?;
        if let Some(record) = index.sessions.get(id) {
            return self.session_from_metadata(id, record, record.recovered);
        }
        if self.managed_trail_exists(id) {
            return self.session_from_metadata(id, &SessionMetadata::default(), true);
        }
        Err(CatalogError::NotFound(id.into()))
    }

    pub fn list(&self) -> Result<Vec<CatalogSession>, CatalogError> {
        self.reconcile_pending_attempts(None)?;
        let index = self.read_index()?;
        let mut entries = Vec::new();
        for (id, record) in &index.sessions {
            entries.push(self.session_from_metadata(id, record, record.recovered)?);
        }
        for id in self.trail_ids()? {
            if !index.sessions.contains_key(&id) {
                entries.push(self.session_from_metadata(&id, &SessionMetadata::default(), true)?);
            }
        }
        entries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(entries)
    }

    /// Starts a new leg session from a saved draft. Catalog-managed sessions
    /// always pass this catalog's sessions directory as LEG_SESSION_DIR,
    /// regardless of an inherited LEG_SESSION_DIR value.
    pub fn start_new(
        &self,
        draft_id: &str,
        _interface: SessionInterface,
        prompt: impl Into<String>,
    ) -> Result<CatalogTurn, CatalogError> {
        let session_id = self.allocate_new_session_id()?;
        self.start_new_with_id(draft_id, session_id, _interface, prompt)
    }

    /// Mints a native-format ID that is not already present in this catalog.
    /// The supervisor reserves the per-session lock before the ID is published.
    pub fn allocate_new_session_id(&self) -> Result<String, CatalogError> {
        let _guard = self.lock_index()?;
        let index = self.read_index_unlocked()?;
        for _ in 0..128 {
            let id = new_native_session_id();
            if !index.sessions.contains_key(&id) && !self.trail_path(&id).exists() {
                return Ok(id);
            }
        }
        Err(CatalogError::Catalog(
            "could not allocate a unique native session id".into(),
        ))
    }

    /// Starts a draft under an ID reserved by the acceptance ledger.
    pub fn start_new_with_id(
        &self,
        draft_id: &str,
        session_id: String,
        _interface: SessionInterface,
        prompt: impl Into<String>,
    ) -> Result<CatalogTurn, CatalogError> {
        validate_session_id(draft_id)?;
        if !is_draft_id(draft_id) {
            return Err(CatalogError::NotFound(draft_id.into()));
        }
        validate_session_id(&session_id)?;
        if is_draft_id(&session_id) || !session_id.starts_with("sess-") {
            return Err(CatalogError::InvalidSessionId);
        }
        self.reconcile_pending_attempts(Some(draft_id))?;
        let prompt = prompt.into();
        let controller = ProcessOwner::current().map_err(CatalogError::Driver)?;
        let token = new_attempt_token(&controller);
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if index.sessions.contains_key(&session_id) || self.trail_path(&session_id).exists() {
            return Err(CatalogError::AlreadyExists(session_id));
        }
        let record = index
            .sessions
            .get_mut(draft_id)
            .ok_or_else(|| CatalogError::NotFound(draft_id.into()))?;
        if record.pending_new_turn {
            return Err(CatalogError::Busy);
        }
        let cwd = self.validated_cwd(draft_id, record)?;
        record.pending_new_turn = true;
        record.pending_attempt = Some(PendingAttempt {
            token: token.clone(),
            revision: 0,
            controller: Some(controller),
            supervisor: None,
            candidate_session_id: session_id.clone(),
            native_session_id: None,
            controller_released: false,
            recovery_warning: None,
        });
        record.updated_at_ms = now_ms();
        self.write_index_unlocked(&index)?;
        drop(_guard);

        let client = self.client();
        let request = TurnRequest::new(prompt, cwd, LegSession::NewWithId(session_id));
        match client.start_catalog_new(request, draft_id, &token) {
            Ok(turn) => Ok(CatalogTurn {
                turn,
                catalog: self.clone(),
                draft_id: Some(draft_id.to_string()),
                attempt_token: Some(token),
                session_id: None,
                pending_event: VecDeque::new(),
                finished: None,
            }),
            Err(error) => {
                self.release_controller_attempt(draft_id, &token)?;
                Err(map_start_error(error))
            }
        }
    }

    pub fn start_existing(
        &self,
        id: &str,
        _interface: SessionInterface,
        prompt: impl Into<String>,
    ) -> Result<CatalogTurn, CatalogError> {
        validate_session_id(id)?;
        if is_draft_id(id) {
            return Err(CatalogError::NotFound(id.into()));
        }
        let prompt = prompt.into();
        let _guard = self.lock_index()?;
        let index = self.read_index_unlocked()?;
        let record = index
            .sessions
            .get(id)
            .ok_or_else(|| CatalogError::NotFound(id.into()))?;
        let session = self.session_from_metadata(id, record, record.recovered)?;
        if session.read_only {
            return Err(CatalogError::ReadOnly(id.into()));
        }
        ensure_idle(&session)?;
        let cwd = self.validated_cwd(id, record)?;
        drop(_guard);
        let client = self.client();
        let turn = client
            .start(TurnRequest::new(
                prompt,
                cwd,
                LegSession::Existing(id.to_string()),
            ))
            .map_err(map_start_error)?;
        Ok(CatalogTurn {
            turn,
            catalog: self.clone(),
            draft_id: None,
            attempt_token: None,
            session_id: Some(id.to_string()),
            pending_event: VecDeque::new(),
            finished: None,
        })
    }

    /// Prepares an explicit retry but does not start a turn. The caller must
    /// show warning() and call confirm_retry only after user confirmation.
    pub fn prepare_retry(&self, id: &str) -> Result<RetryIntent, CatalogError> {
        let session = self.get(id)?;
        if session.read_only {
            return Err(CatalogError::ReadOnly(id.into()));
        }
        ensure_idle(&session)?;
        let turn = session
            .turns
            .last()
            .filter(|turn| {
                matches!(
                    turn.outcome,
                    TrailOutcome::Failed | TrailOutcome::Interrupted | TrailOutcome::Incomplete
                )
            })
            .ok_or_else(|| CatalogError::NoRetry(id.into()))?;
        Ok(RetryIntent {
            session_id: id.to_string(),
            turn_index: turn.turn_index,
            prompt: turn
                .retry_prompt
                .as_deref()
                .unwrap_or(&turn.prompt)
                .to_string(),
        })
    }

    pub fn confirm_retry(
        &self,
        intent: &RetryIntent,
        interface: SessionInterface,
    ) -> Result<CatalogTurn, CatalogError> {
        let current = self.prepare_retry(&intent.session_id)?;
        if current.turn_index != intent.turn_index || current.prompt != intent.prompt {
            return Err(CatalogError::StaleRetry);
        }
        self.start_existing(&intent.session_id, interface, intent.prompt.clone())
    }

    /// Exports only the parsed leg trail. Catalog metadata and process
    /// environment values are not included.
    pub fn export_transcript(&self, id: &str) -> Result<String, CatalogError> {
        let session = self.get(id)?;
        #[derive(Serialize)]
        struct Transcript<'a> {
            schema: &'static str,
            session_id: &'a str,
            turns: &'a [TrailTurn],
            warnings: &'a [String],
            ended: bool,
        }
        serde_json::to_string_pretty(&Transcript {
            schema: "leg-ui.transcript/v1",
            session_id: &session.id,
            turns: &session.turns,
            warnings: &session.warnings,
            ended: session.ended,
        })
        .map_err(CatalogError::Json)
    }

    fn client(&self) -> Client {
        Client::new(ClientConfig {
            leg_bin: self.inner.leg_bin.clone(),
            supervisor_bin: self.inner.supervisor_bin.clone(),
            session_store_dir: Some(self.inner.sessions_dir.clone()),
        })
    }

    fn session_from_metadata(
        &self,
        id: &str,
        record: &SessionMetadata,
        recovered: bool,
    ) -> Result<CatalogSession, CatalogError> {
        let mut warnings = Vec::new();
        let (turns, mut read_only, ended) = if is_draft_id(id) {
            (Vec::new(), false, false)
        } else if !self.managed_trail_exists(id) {
            warnings.push(
                "session trail is missing or outside the managed store; this entry is read-only"
                    .into(),
            );
            (Vec::new(), true, false)
        } else {
            let path = self.trail_path(id);
            match read_trail(&path, id) {
                Ok(snapshot) => {
                    warnings.extend(snapshot.warnings);
                    (snapshot.turns, snapshot.read_only, snapshot.ended)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    warnings.push("session trail is missing; this entry is read-only".into());
                    (Vec::new(), true, false)
                }
                Err(error) => {
                    warnings.push(format!("could not read session trail: {error}"));
                    (Vec::new(), true, false)
                }
            }
        };
        let mut recovered = recovered;
        if recovered && record.cwd.is_none() {
            warnings.push("recovered session needs an explicitly selected workspace".into());
            read_only = true;
        } else if recovered {
            recovered = false;
        }
        if !is_draft_id(id) {
            match record.cwd.as_ref() {
                None => warnings.push("session needs an explicitly selected workspace".into()),
                Some(cwd) if canonical_workspace(cwd).is_err() => {
                    warnings.push(format!(
                        "recorded workspace {} is missing; choose an existing replacement",
                        cwd.display()
                    ));
                }
                Some(_) => {}
            }
        }

        let run_state = if is_draft_id(id) {
            let (state, pending_warnings) = self.pending_attempt_state(id, record);
            warnings.extend(pending_warnings);
            state
        } else {
            match supervisor::session_lock_is_held(&self.inner.sessions_dir, id) {
                Ok(true) => CatalogRunState::Active,
                Ok(false) => CatalogRunState::Idle,
                Err(error) => {
                    warnings.push(error);
                    read_only = true;
                    CatalogRunState::Unknown
                }
            }
        };
        if run_state == CatalogRunState::Unknown {
            read_only = true;
        }

        Ok(CatalogSession {
            id: id.to_string(),
            name: record.name.clone(),
            cwd: record.cwd.clone(),
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            drafts: record.drafts.clone(),
            display: record.display.clone(),
            turns,
            warnings,
            recovered,
            read_only,
            run_state,
            pending_new_turn: record.pending_new_turn,
            ended,
        })
    }

    fn pending_attempt_state(
        &self,
        draft_id: &str,
        record: &SessionMetadata,
    ) -> (CatalogRunState, Vec<String>) {
        if !record.pending_new_turn {
            return (CatalogRunState::Idle, Vec::new());
        }
        let Some(attempt) = record.pending_attempt.as_ref() else {
            return (
                CatalogRunState::Unknown,
                vec![format!(
                    "legacy pending draft {draft_id} has no verified attempt record; after confirming its original companion and supervisor have stopped, create a new conversation and copy the saved prompt and workspace. The draft and any orphan trail are preserved"
                )],
            );
        };

        let mut warnings = attempt.recovery_warning.iter().cloned().collect::<Vec<_>>();
        let mut active = false;
        let mut unknown = attempt.recovery_warning.is_some();
        if attempt.token.is_empty()
            || attempt.controller.is_none()
            || !safe_session_id(&attempt.candidate_session_id)
            || !attempt.candidate_session_id.starts_with("sess-")
        {
            warnings.push(
                "pending attempt is missing valid ownership or session handoff evidence; its reservation is preserved".into(),
            );
            return (CatalogRunState::Unknown, warnings);
        }
        if !attempt.controller_released {
            if let Some(controller) = attempt.controller.as_ref() {
                record_owner_state(
                    "controller",
                    controller,
                    &mut active,
                    &mut unknown,
                    &mut warnings,
                );
            } else {
                unknown = true;
                warnings.push(
                    "pending attempt has no recorded controller identity; its reservation is preserved".into(),
                );
            }
        }
        if let Some(supervisor_owner) = attempt.supervisor.as_ref() {
            record_owner_state(
                "supervisor",
                supervisor_owner,
                &mut active,
                &mut unknown,
                &mut warnings,
            );
        }
        match supervisor::session_lock_is_held(
            &self.inner.sessions_dir,
            &attempt.candidate_session_id,
        ) {
            Ok(true) => active = true,
            Ok(false) => {}
            Err(error) => {
                unknown = true;
                warnings.push(format!(
                    "could not verify the pending session lock for {}: {error}",
                    attempt.candidate_session_id
                ));
            }
        }
        if active {
            return (CatalogRunState::Active, warnings);
        }
        if unknown {
            warnings.push(
                "pending creation remains reserved until process and session ownership can be verified".into(),
            );
            return (CatalogRunState::Unknown, warnings);
        }
        if let Some(session_id) = attempt.native_session_id.as_deref() {
            if session_id != attempt.candidate_session_id {
                warnings.push(format!(
                    "pending attempt {} recorded a session id that does not match its reservation; it was not adopted",
                    attempt.token
                ));
            } else if !self.managed_trail_exists(session_id) {
                warnings.push(format!(
                    "supervisor recorded session {session_id}, but its managed trail is missing; the pending draft is preserved and no prompt was replayed"
                ));
            } else {
                warnings.push(
                    "pending session ownership changed during inspection; refresh to finish recovery".into(),
                );
            }
        } else if self.managed_trail_exists(&attempt.candidate_session_id) {
            warnings.push(format!(
                "trail {} exists without a token-matched supervisor handoff; it was not adopted and the prompt was not replayed",
                attempt.candidate_session_id
            ));
        } else {
            warnings.push(
                "pending creation changed during inspection; refresh to finish recovery".into(),
            );
        }
        (CatalogRunState::Unknown, warnings)
    }

    fn reconcile_pending_attempts(&self, only_id: Option<&str>) -> Result<(), CatalogError> {
        let pending = self
            .read_index()?
            .sessions
            .into_iter()
            .filter(|(id, record)| {
                record.pending_new_turn
                    && only_id.is_none_or(|only_id| only_id == id)
                    && record.pending_attempt.is_some()
            })
            .filter_map(|(id, record)| record.pending_attempt.map(|attempt| (id, attempt)))
            .collect::<Vec<_>>();

        for (draft_id, attempt) in pending {
            let decision = self.recovery_decision(&attempt);
            let result = match decision {
                RecoveryDecision::Active | RecoveryDecision::Unknown => Ok(()),
                RecoveryDecision::Clear => self.clear_pending_attempt_if_current(
                    &draft_id,
                    &attempt.token,
                    attempt.revision,
                ),
                RecoveryDecision::Bind(session_id) => self.bind_session_id(
                    &draft_id,
                    &session_id,
                    &attempt.token,
                    Some(attempt.revision),
                ),
            };
            if let Err(error) = result {
                self.record_recovery_failure(&draft_id, &attempt, &error.to_string())?;
            }
        }
        Ok(())
    }

    fn record_recovery_failure(
        &self,
        draft_id: &str,
        expected: &PendingAttempt,
        error: &str,
    ) -> Result<(), CatalogError> {
        let warning = format!(
            "could not safely recover draft {draft_id} into session {}: {error}. The saved draft and workspace are preserved; start a new conversation and copy them there. No prompt was replayed",
            expected.candidate_session_id
        );
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        let Some(record) = index.sessions.get_mut(draft_id) else {
            return Ok(());
        };
        let Some(attempt) = record.pending_attempt.as_mut().filter(|attempt| {
            record.pending_new_turn
                && attempt.token == expected.token
                && attempt.revision == expected.revision
        }) else {
            return Ok(());
        };
        if attempt.recovery_warning.as_deref() != Some(warning.as_str()) {
            attempt.recovery_warning = Some(warning);
            attempt.revision = attempt.revision.saturating_add(1);
            record.updated_at_ms = now_ms();
            self.write_index_unlocked(&index)?;
        }
        Ok(())
    }

    fn recovery_decision(&self, attempt: &PendingAttempt) -> RecoveryDecision {
        let mut active = false;
        let mut unknown = false;
        if attempt.token.is_empty()
            || attempt.controller.is_none()
            || !safe_session_id(&attempt.candidate_session_id)
            || !attempt.candidate_session_id.starts_with("sess-")
        {
            return RecoveryDecision::Unknown;
        }
        if !attempt.controller_released {
            match attempt.controller.as_ref() {
                Some(controller) => match controller.inspect() {
                    OwnerState::Alive => active = true,
                    OwnerState::Dead | OwnerState::Reused => {}
                    OwnerState::Unknown(_) => unknown = true,
                },
                None => unknown = true,
            }
        }
        if let Some(supervisor_owner) = attempt.supervisor.as_ref() {
            match supervisor_owner.inspect() {
                OwnerState::Alive => active = true,
                OwnerState::Dead | OwnerState::Reused => {}
                OwnerState::Unknown(_) => unknown = true,
            }
        }
        match supervisor::session_lock_is_held(
            &self.inner.sessions_dir,
            &attempt.candidate_session_id,
        ) {
            Ok(true) => active = true,
            Ok(false) => {}
            Err(_) => unknown = true,
        }
        if active {
            return RecoveryDecision::Active;
        }
        if unknown {
            return RecoveryDecision::Unknown;
        }
        if let Some(session_id) = attempt.native_session_id.as_deref() {
            if session_id == attempt.candidate_session_id && self.managed_trail_exists(session_id) {
                RecoveryDecision::Bind(session_id.to_string())
            } else {
                RecoveryDecision::Unknown
            }
        } else if self.managed_trail_exists(&attempt.candidate_session_id) {
            RecoveryDecision::Unknown
        } else {
            RecoveryDecision::Clear
        }
    }

    fn validated_cwd(&self, id: &str, record: &SessionMetadata) -> Result<PathBuf, CatalogError> {
        let cwd = record
            .cwd
            .as_ref()
            .ok_or_else(|| CatalogError::WorkspaceRequired(id.into()))?;
        let canonical =
            fs::canonicalize(cwd).map_err(|_| CatalogError::WorkspaceMissing(cwd.clone()))?;
        if !canonical.is_dir() || canonical != *cwd {
            return Err(CatalogError::WorkspaceMissing(cwd.clone()));
        }
        Ok(canonical)
    }

    fn trail_path(&self, id: &str) -> PathBuf {
        self.inner.sessions_dir.join(format!("{id}.jsonl"))
    }

    fn managed_trail_exists(&self, id: &str) -> bool {
        let path = self.trail_path(id);
        let Ok(file_type) = fs::symlink_metadata(&path).map(|metadata| metadata.file_type()) else {
            return false;
        };
        if !file_type.is_file() {
            return false;
        }
        fs::canonicalize(path)
            .ok()
            .and_then(|canonical| canonical.parent().map(Path::to_path_buf))
            .as_deref()
            == Some(self.inner.sessions_dir.as_path())
    }

    fn trail_ids(&self) -> Result<Vec<String>, CatalogError> {
        let mut ids = Vec::new();
        for item in fs::read_dir(&self.inner.sessions_dir)? {
            let item = item?;
            if !item.file_type()?.is_file() {
                continue;
            }
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !valid_leg_session_id(id) {
                continue;
            }
            let path = item.path();
            if fs::canonicalize(&path)
                .ok()
                .and_then(|canonical| canonical.parent().map(Path::to_path_buf))
                .as_deref()
                != Some(self.inner.sessions_dir.as_path())
            {
                continue;
            }
            ids.push(id.to_string());
        }
        ids.sort();
        Ok(ids)
    }

    fn index_path(&self) -> PathBuf {
        self.inner.state_dir.join(INDEX_NAME)
    }

    fn lock_index(&self) -> Result<File, CatalogError> {
        let file = private_open_rw_create(&self.inner.state_dir.join(LOCK_NAME))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn from_sessions_dir(sessions_dir: &Path) -> Result<Self, CatalogError> {
        let sessions_dir = fs::canonicalize(sessions_dir)?;
        let state_dir = sessions_dir
            .parent()
            .ok_or_else(|| CatalogError::Catalog("session store has no state directory".into()))?
            .to_path_buf();
        Ok(Self {
            inner: Arc::new(CatalogInner {
                state_dir,
                sessions_dir,
                leg_bin: None,
                supervisor_bin: None,
            }),
        })
    }

    fn read_index(&self) -> Result<CatalogIndex, CatalogError> {
        let _guard = self.lock_index()?;
        self.read_index_unlocked()
    }

    fn mutate_metadata<F>(&self, id: &str, mutate: F) -> Result<(), CatalogError>
    where
        F: FnOnce(&mut SessionMetadata) -> Result<(), CatalogError>,
    {
        validate_session_id(id)?;
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if !index.sessions.contains_key(id) {
            if self.managed_trail_exists(id) {
                let mut record = SessionMetadata::new(None, None);
                record.recovered = true;
                index.sessions.insert(id.to_string(), record);
            } else {
                return Err(CatalogError::NotFound(id.into()));
            }
        }
        let record = index.sessions.get_mut(id).expect("record inserted above");
        mutate(record)?;
        record.updated_at_ms = now_ms();
        self.write_index_unlocked(&index)
    }

    fn update_index<F>(&self, mutate: F) -> Result<CatalogIndex, CatalogError>
    where
        F: FnOnce(&mut CatalogIndex) -> Result<(), CatalogError>,
    {
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        mutate(&mut index)?;
        self.write_index_unlocked(&index)?;
        Ok(index)
    }

    fn read_index_unlocked(&self) -> Result<CatalogIndex, CatalogError> {
        let bytes = match fs::read(self.index_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(CatalogIndex {
                    version: INDEX_VERSION,
                    sessions: BTreeMap::new(),
                });
            }
            Err(error) => return Err(CatalogError::Io(error)),
        };
        let index: CatalogIndex = serde_json::from_slice(&bytes)?;
        if index.version != INDEX_VERSION {
            return Err(CatalogError::Catalog(format!(
                "unsupported catalog version {}",
                index.version
            )));
        }
        for id in index.sessions.keys() {
            validate_session_id(id)?;
        }
        Ok(index)
    }

    fn write_index_unlocked(&self, index: &CatalogIndex) -> Result<(), CatalogError> {
        let path = self.index_path();
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self.inner.state_dir.join(format!(
            ".{INDEX_NAME}.tmp-{}-{sequence}",
            std::process::id()
        ));
        let bytes = serde_json::to_vec_pretty(index)?;
        let mut file = private_create_new(&temporary)?;
        let result = file
            .write_all(&bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                drop(file);
                fs::rename(&temporary, &path)
            });
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(CatalogError::Io(error));
        }
        sync_directory(&self.inner.state_dir);
        Ok(())
    }

    fn bind_session_id(
        &self,
        draft_id: &str,
        session_id: &str,
        attempt_token: &str,
        expected_revision: Option<u64>,
    ) -> Result<(), CatalogError> {
        validate_session_id(session_id)?;
        if is_draft_id(session_id) {
            return Err(CatalogError::InvalidSessionId);
        }
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if let Some(existing) = index.sessions.get(session_id) {
            if existing.bound_attempt_token.as_deref() == Some(attempt_token) {
                return Ok(());
            }
            if existing.bound_attempt_token.is_some()
                || existing.pending_new_turn
                || existing.pending_attempt.is_some()
            {
                return Err(CatalogError::AlreadyExists(session_id.into()));
            }
        }
        let current_attempt = index
            .sessions
            .get(draft_id)
            .and_then(|record| record.pending_attempt.as_ref())
            .ok_or_else(|| CatalogError::NotFound(draft_id.into()))?;
        if current_attempt.token != attempt_token
            || current_attempt.native_session_id.as_deref() != Some(session_id)
            || expected_revision.is_some_and(|revision| current_attempt.revision != revision)
        {
            return Err(CatalogError::Catalog(
                "pending session attempt changed before binding".into(),
            ));
        }
        let draft_record = index
            .sessions
            .remove(draft_id)
            .ok_or_else(|| CatalogError::NotFound(draft_id.into()))?;
        let mut record = match index.sessions.remove(session_id) {
            Some(existing) => merge_pending_session_metadata(existing, draft_record, attempt_token),
            None => draft_record,
        };
        record.pending_new_turn = false;
        record.pending_attempt = None;
        record.bound_attempt_token = Some(attempt_token.to_string());
        record.recovered = false;
        record.updated_at_ms = now_ms();
        index.sessions.insert(session_id.to_string(), record);
        self.write_index_unlocked(&index)
    }

    fn clear_pending_attempt_if_current(
        &self,
        draft_id: &str,
        attempt_token: &str,
        revision: u64,
    ) -> Result<(), CatalogError> {
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if let Some(record) = index.sessions.get_mut(draft_id) {
            let matches = record.pending_new_turn
                && record.pending_attempt.as_ref().is_some_and(|attempt| {
                    attempt.token == attempt_token && attempt.revision == revision
                });
            if matches {
                record.pending_new_turn = false;
                record.pending_attempt = None;
                record.updated_at_ms = now_ms();
                self.write_index_unlocked(&index)?;
            }
        }
        Ok(())
    }

    fn release_controller_attempt(
        &self,
        draft_id: &str,
        attempt_token: &str,
    ) -> Result<(), CatalogError> {
        {
            let _guard = self.lock_index()?;
            let mut index = self.read_index_unlocked()?;
            if let Some(record) = index.sessions.get_mut(draft_id)
                && let Some(attempt) = record.pending_attempt.as_mut()
                && record.pending_new_turn
                && attempt.token == attempt_token
                && !attempt.controller_released
            {
                attempt.controller_released = true;
                attempt.revision = attempt.revision.saturating_add(1);
                attempt.recovery_warning = None;
                record.updated_at_ms = now_ms();
                self.write_index_unlocked(&index)?;
            }
        }
        self.reconcile_pending_attempts(Some(draft_id))
    }

    fn record_supervisor_owner(
        &self,
        draft_id: &str,
        attempt_token: &str,
        candidate_session_id: &str,
        owner: ProcessOwner,
    ) -> Result<(), CatalogError> {
        self.mutate_pending_attempt(draft_id, attempt_token, |attempt| {
            if attempt.candidate_session_id != candidate_session_id {
                return Err(CatalogError::Catalog(
                    "supervisor session reservation does not match the pending attempt".into(),
                ));
            }
            if attempt.controller_released {
                return Err(CatalogError::Catalog(
                    "controller already released this pending attempt".into(),
                ));
            }
            match attempt.supervisor.as_ref() {
                Some(existing) if existing == &owner => return Ok(false),
                Some(_) => {
                    return Err(CatalogError::Catalog(
                        "pending attempt already has a different supervisor owner".into(),
                    ));
                }
                None => {}
            }
            attempt.supervisor = Some(owner);
            Ok(true)
        })
    }

    fn record_native_session_handoff(
        &self,
        draft_id: &str,
        attempt_token: &str,
        candidate_session_id: &str,
        session_id: &str,
        supervisor_owner: &ProcessOwner,
    ) -> Result<(), CatalogError> {
        self.mutate_pending_attempt(draft_id, attempt_token, |attempt| {
            if attempt.candidate_session_id != candidate_session_id
                || candidate_session_id != session_id
                || attempt.supervisor.as_ref() != Some(supervisor_owner)
            {
                return Err(CatalogError::Catalog(
                    "native session handoff does not match the verified pending attempt".into(),
                ));
            }
            match attempt.native_session_id.as_deref() {
                Some(existing) if existing == session_id => return Ok(false),
                Some(_) => {
                    return Err(CatalogError::Catalog(
                        "pending attempt already names a different native session".into(),
                    ));
                }
                None => {}
            }
            attempt.native_session_id = Some(session_id.to_string());
            Ok(true)
        })
    }

    fn mutate_pending_attempt<F>(
        &self,
        draft_id: &str,
        attempt_token: &str,
        mutate: F,
    ) -> Result<(), CatalogError>
    where
        F: FnOnce(&mut PendingAttempt) -> Result<bool, CatalogError>,
    {
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        let record = index
            .sessions
            .get_mut(draft_id)
            .ok_or_else(|| CatalogError::NotFound(draft_id.into()))?;
        let attempt = record
            .pending_attempt
            .as_mut()
            .filter(|attempt| record.pending_new_turn && attempt.token == attempt_token)
            .ok_or_else(|| {
                CatalogError::Catalog("pending session attempt is no longer current".into())
            })?;
        if mutate(attempt)? {
            attempt.revision = attempt.revision.saturating_add(1);
            attempt.recovery_warning = None;
            record.updated_at_ms = now_ms();
            self.write_index_unlocked(&index)?;
        }
        Ok(())
    }

    /// Reconciles the catalog attempt associated with an unfinished durable
    /// receipt. Live or ambiguous ownership remains reserved.
    pub fn recover_pending_new_session(&self, draft_id: &str) -> Result<(), CatalogError> {
        if !is_draft_id(draft_id) {
            return Err(CatalogError::InvalidSessionId);
        }
        self.reconcile_pending_attempts(Some(draft_id))
    }
}

pub(crate) fn publish_supervisor_owner(
    sessions_dir: &Path,
    draft_id: &str,
    attempt_token: &str,
    candidate_session_id: &str,
    owner: ProcessOwner,
) -> Result<(), String> {
    SessionCatalog::from_sessions_dir(sessions_dir)
        .and_then(|catalog| {
            catalog.record_supervisor_owner(draft_id, attempt_token, candidate_session_id, owner)
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn publish_native_session_handoff(
    sessions_dir: &Path,
    draft_id: &str,
    attempt_token: &str,
    candidate_session_id: &str,
    session_id: &str,
    supervisor_owner: &ProcessOwner,
) -> Result<(), String> {
    pause_before_handoff_for_tests()?;
    SessionCatalog::from_sessions_dir(sessions_dir)
        .and_then(|catalog| {
            if !catalog.managed_trail_exists(session_id) {
                return Err(CatalogError::Catalog(format!(
                    "managed trail for session {session_id} is not present yet"
                )));
            }
            catalog.record_native_session_handoff(
                draft_id,
                attempt_token,
                candidate_session_id,
                session_id,
                supervisor_owner,
            )
        })
        .map_err(|error| error.to_string())
}

#[cfg(debug_assertions)]
fn pause_before_handoff_for_tests() -> Result<(), String> {
    let (Some(ready), Some(release)) = (
        std::env::var_os("LEG_UI_TEST_HANDOFF_PAUSE_READY"),
        std::env::var_os("LEG_UI_TEST_HANDOFF_PAUSE_RELEASE"),
    ) else {
        return Ok(());
    };
    let ready = PathBuf::from(ready);
    let release = PathBuf::from(release);
    fs::write(ready, "ready").map_err(|error| error.to_string())?;
    while !release.exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn pause_before_handoff_for_tests() -> Result<(), String> {
    Ok(())
}

fn record_owner_state(
    label: &str,
    owner: &ProcessOwner,
    active: &mut bool,
    unknown: &mut bool,
    warnings: &mut Vec<String>,
) {
    match owner.inspect() {
        OwnerState::Alive => *active = true,
        OwnerState::Dead | OwnerState::Reused => {}
        OwnerState::Unknown(error) => {
            *unknown = true;
            warnings.push(format!(
                "could not verify the recorded {label} process {} birth identity: {error}",
                owner.pid
            ));
        }
    }
}

fn new_attempt_token(owner: &ProcessOwner) -> String {
    let sequence = ATTEMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "attempt-{}-{}-{started}-{sequence}",
        owner.pid,
        owner.birth_token.replace(':', "-")
    )
}

pub struct CatalogTurn {
    turn: TurnHandle,
    catalog: SessionCatalog,
    draft_id: Option<String>,
    attempt_token: Option<String>,
    session_id: Option<String>,
    pending_event: VecDeque<StreamEvent>,
    finished: Option<TurnOutcome>,
}

impl CatalogTurn {
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn observe(&mut self) -> Result<Option<StreamEvent>, CatalogTurnError> {
        if let Some(event) = self.pending_event.pop_front() {
            return Ok(Some(event));
        }
        if self.finished.is_some() {
            return Ok(None);
        }
        let Some(event) = self.turn.observe()? else {
            return Ok(None);
        };
        if let (
            Some(draft_id),
            StreamEvent::TurnStart {
                session_id: Some(session_id),
                ..
            },
        ) = (&self.draft_id, &event)
        {
            let attempt_token = self
                .attempt_token
                .as_deref()
                .expect("new catalog turns carry an attempt token");
            match self
                .catalog
                .bind_session_id(draft_id, session_id, attempt_token, None)
            {
                Ok(()) => {
                    self.session_id = Some(session_id.clone());
                    self.draft_id = None;
                }
                Err(error) => {
                    self.pending_event.push_back(event);
                    return Err(CatalogTurnError::Catalog(error.to_string()));
                }
            }
        }
        Ok(Some(event))
    }

    pub fn stop(&mut self) -> Result<(), ClientError> {
        self.turn.stop()
    }

    /// Returns a cloneable Stop control for an event loop that reads the turn
    /// from a separate thread.
    pub fn stop_handle(&self) -> TurnStopHandle {
        self.turn.stop_handle()
    }

    pub fn wait(&mut self) -> Result<TurnOutcome, CatalogTurnError> {
        if let Some(outcome) = &self.finished {
            return Ok(outcome.clone());
        }
        while self.observe()?.is_some() {}
        let outcome = self.turn.wait()?;
        self.finished = Some(outcome.clone());
        if let (Some(draft_id), Some(attempt_token)) =
            (self.draft_id.as_deref(), self.attempt_token.as_deref())
        {
            self.catalog
                .release_controller_attempt(draft_id, attempt_token)
                .map_err(|error| CatalogTurnError::Catalog(error.to_string()))?;
        }
        Ok(outcome)
    }
}

impl Drop for CatalogTurn {
    fn drop(&mut self) {
        if let (Some(draft_id), Some(attempt_token)) =
            (self.draft_id.as_deref(), self.attempt_token.as_deref())
        {
            let _ = self
                .catalog
                .release_controller_attempt(draft_id, attempt_token);
        }
    }
}

fn map_start_error(error: StartError) -> CatalogError {
    match error {
        StartError::Busy => CatalogError::Busy,
        other => CatalogError::Start(other),
    }
}

fn ensure_idle(session: &CatalogSession) -> Result<(), CatalogError> {
    match session.run_state {
        CatalogRunState::Idle => Ok(()),
        CatalogRunState::Active => Err(CatalogError::Busy),
        CatalogRunState::Unknown => Err(CatalogError::Driver(
            "the session ownership lock could not be verified".into(),
        )),
    }
}

fn resolve_state_dir(override_path: Option<&Path>) -> Result<PathBuf, CatalogError> {
    if let Some(path) = override_path {
        if path.as_os_str().is_empty() {
            return Err(CatalogError::Catalog(
                "state directory override is blank".into(),
            ));
        }
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("LEG_UI_STATE_DIR") {
        if path.is_empty() {
            return Err(CatalogError::Catalog(
                "LEG_UI_STATE_DIR must not be blank".into(),
            ));
        }
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|path| !path.is_empty()))
        .ok_or_else(|| CatalogError::Catalog("could not determine home directory".into()))?;
    let home = PathBuf::from(home);
    #[cfg(target_os = "macos")]
    {
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("leg-ui"))
    }
    #[cfg(target_os = "windows")]
    {
        let base = env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));
        Ok(base.join("leg-ui"))
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
            Ok(PathBuf::from(path).join("leg-ui"))
        } else {
            Ok(home.join(".local").join("state").join("leg-ui"))
        }
    }
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, CatalogError> {
    let canonical =
        fs::canonicalize(path).map_err(|_| CatalogError::WorkspaceMissing(path.to_path_buf()))?;
    if !canonical.is_dir() {
        return Err(CatalogError::WorkspaceMissing(path.to_path_buf()));
    }
    Ok(canonical)
}

fn validate_session_id(id: &str) -> Result<(), CatalogError> {
    if safe_session_id(id) {
        Ok(())
    } else {
        Err(CatalogError::InvalidSessionId)
    }
}

fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn valid_leg_session_id(id: &str) -> bool {
    safe_session_id(id) && !is_draft_id(id)
}

fn is_draft_id(id: &str) -> bool {
    id.starts_with("draft-")
}

fn new_draft_id() -> String {
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("draft-{}-{}-{sequence}", std::process::id(), now_ms())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_trail(path: &Path, expected_id: &str) -> io::Result<TrailSnapshot> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut turns = Vec::<TrailTurn>::new();
    let mut positions = HashMap::<u64, usize>::new();
    let mut warnings = Vec::new();
    let mut read_only = false;
    let mut ended = false;
    let mut line = Vec::new();
    let mut line_number = 0usize;

    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        line_number += 1;
        let terminated = line.last() == Some(&b'\n');
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut record: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("line {line_number}: malformed JSON: {error}"));
                read_only = true;
                if !terminated {
                    break;
                }
                continue;
            }
        };
        let original_prompt = record
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        redact_trail_content(&mut record);
        let Some(event) = record.get("event").and_then(Value::as_str) else {
            continue;
        };
        let known = matches!(
            event,
            "session_start"
                | "session_end"
                | "request"
                | "tool_round"
                | "tool_call"
                | "tool_result"
                | "response_ok"
                | "response_error"
        );
        if !known {
            // Unknown events from the current exchange schema are forward
            // compatible and do not hide known history.
            match record.get("schema").and_then(Value::as_str) {
                Some(EXCHANGE_SCHEMA) => {}
                Some(_) => {
                    warnings.push(format!(
                        "line {line_number}: unsupported exchange schema; entry is read-only"
                    ));
                    read_only = true;
                }
                None => {
                    warnings.push(format!(
                        "line {line_number}: unknown event has no exchange schema"
                    ));
                    read_only = true;
                }
            }
            continue;
        }
        if record.get("schema").and_then(Value::as_str) != Some(EXCHANGE_SCHEMA) {
            warnings.push(format!(
                "line {line_number}: unsupported or missing exchange schema; entry is read-only"
            ));
            read_only = true;
            continue;
        }
        if matches!(event, "session_start" | "session_end") {
            if record.get("session_id").and_then(Value::as_str) != Some(expected_id) {
                warnings.push(format!(
                    "line {line_number}: session id does not match the trail filename"
                ));
                read_only = true;
                continue;
            }
            if event == "session_end" {
                ended = true;
            }
            continue;
        }

        let session_id = record.get("session_id").and_then(Value::as_str);
        let turn_index = record.get("turn_index").and_then(Value::as_u64);
        if session_id != Some(expected_id) || turn_index.is_none() {
            warnings.push(format!(
                "line {line_number}: {event} is missing valid session coordinates"
            ));
            read_only = true;
            continue;
        }
        let turn_index = turn_index.expect("checked above");
        match event {
            "request" => {
                let Some(prompt) = record.get("prompt").and_then(Value::as_str) else {
                    warnings.push(format!("line {line_number}: request has no prompt"));
                    read_only = true;
                    continue;
                };
                if positions.contains_key(&turn_index) {
                    warnings.push(format!(
                        "line {line_number}: duplicate request for turn {turn_index}"
                    ));
                    read_only = true;
                    continue;
                }
                positions.insert(turn_index, turns.len());
                turns.push(TrailTurn {
                    turn_index,
                    prompt: prompt.to_string(),
                    timestamp_ms: record.get("ts_ms").and_then(Value::as_u64),
                    retry_prompt: original_prompt,
                    outcome: TrailOutcome::Incomplete,
                    outcome_timestamp_ms: None,
                    stop_reason: None,
                    reply: None,
                    failure_kind: None,
                    failure_message: None,
                    tool_rounds: Vec::new(),
                    tools: Vec::new(),
                });
            }
            "tool_round" => {
                let Some(turn) = turn_mut(&mut turns, &positions, turn_index) else {
                    warnings.push(format!(
                        "line {line_number}: tool_round has no matching request"
                    ));
                    read_only = true;
                    continue;
                };
                if let Some(content) = record.get("content").filter(|value| value.is_array()) {
                    turn.tool_rounds.push(content.clone());
                } else {
                    warnings.push(format!(
                        "line {line_number}: tool_round content is malformed"
                    ));
                    read_only = true;
                }
            }
            "tool_call" => {
                let Some(turn) = turn_mut(&mut turns, &positions, turn_index) else {
                    warnings.push(format!(
                        "line {line_number}: tool_call has no matching request"
                    ));
                    read_only = true;
                    continue;
                };
                let Some(tool_use_id) = record.get("tool_use_id").and_then(Value::as_str) else {
                    warnings.push(format!("line {line_number}: tool_call has no id"));
                    read_only = true;
                    continue;
                };
                turn.tools.push(TrailTool {
                    tool_use_id: tool_use_id.to_string(),
                    tool_name: record
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: record.get("input").cloned().unwrap_or(Value::Null),
                    timestamp_ms: record.get("ts_ms").and_then(Value::as_u64),
                    result: None,
                });
            }
            "tool_result" => {
                let Some(turn) = turn_mut(&mut turns, &positions, turn_index) else {
                    warnings.push(format!(
                        "line {line_number}: tool_result has no matching request"
                    ));
                    read_only = true;
                    continue;
                };
                let Some(tool_use_id) = record.get("tool_use_id").and_then(Value::as_str) else {
                    warnings.push(format!("line {line_number}: tool_result has no id"));
                    read_only = true;
                    continue;
                };
                let Some(status) = record.get("status").and_then(Value::as_str) else {
                    warnings.push(format!("line {line_number}: tool_result has no status"));
                    read_only = true;
                    continue;
                };
                if !matches!(status, "completed" | "failed" | "denied") {
                    warnings.push(format!("line {line_number}: tool_result status is invalid"));
                    read_only = true;
                    continue;
                }
                let paired = turn
                    .tools
                    .iter_mut()
                    .find(|tool| tool.tool_use_id == tool_use_id && tool.result.is_none());
                if let Some(tool) = paired {
                    tool.result = Some(TrailToolResult {
                        status: status.to_string(),
                        timestamp_ms: record.get("ts_ms").and_then(Value::as_u64),
                        result: record
                            .get("result")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        error: record
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                } else {
                    warnings.push(format!(
                        "line {line_number}: tool_result {tool_use_id} has no unmatched call"
                    ));
                    read_only = true;
                }
            }
            "response_ok" => {
                let Some(turn) = turn_mut(&mut turns, &positions, turn_index) else {
                    warnings.push(format!(
                        "line {line_number}: response_ok has no matching request"
                    ));
                    read_only = true;
                    continue;
                };
                let Some(reply) = record.get("reply").and_then(Value::as_str) else {
                    warnings.push(format!("line {line_number}: response_ok has no reply"));
                    read_only = true;
                    continue;
                };
                turn.outcome = TrailOutcome::Succeeded;
                turn.outcome_timestamp_ms = record.get("ts_ms").and_then(Value::as_u64);
                turn.stop_reason = record
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                turn.reply = Some(reply.to_string());
            }
            "response_error" => {
                let Some(turn) = turn_mut(&mut turns, &positions, turn_index) else {
                    warnings.push(format!(
                        "line {line_number}: response_error has no matching request"
                    ));
                    read_only = true;
                    continue;
                };
                let (Some(kind), Some(message)) = (
                    record.get("kind").and_then(Value::as_str),
                    record.get("message").and_then(Value::as_str),
                ) else {
                    warnings.push(format!(
                        "line {line_number}: response_error is missing its kind or message"
                    ));
                    read_only = true;
                    continue;
                };
                let kind = kind.to_string();
                turn.outcome = if kind == "interrupted" {
                    TrailOutcome::Interrupted
                } else {
                    TrailOutcome::Failed
                };
                turn.outcome_timestamp_ms = record.get("ts_ms").and_then(Value::as_u64);
                turn.failure_kind = Some(kind);
                turn.failure_message = Some(message.to_string());
            }
            _ => unreachable!("known events were matched above"),
        }
    }

    Ok(TrailSnapshot {
        turns,
        warnings,
        read_only,
        ended,
    })
}

fn turn_mut<'a>(
    turns: &'a mut [TrailTurn],
    positions: &HashMap<u64, usize>,
    turn_index: u64,
) -> Option<&'a mut TrailTurn> {
    positions
        .get(&turn_index)
        .and_then(|position| turns.get_mut(*position))
}

fn redact_trail_content(record: &mut Value) {
    for field in [
        "prompt", "reply", "message", "content", "input", "result", "error",
    ] {
        if let Some(value) = record.get_mut(field) {
            crate::client::redact_value(value);
        }
    }
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn private_open_rw_create(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn private_create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn sample_trail(id: &str, event_suffix: &str) -> String {
        let events = [
            json!({"schema":EXCHANGE_SCHEMA,"event":"session_start","ts_ms":1,"session_id":id}),
            json!({"schema":EXCHANGE_SCHEMA,"event":"request","ts_ms":2,"model":"m","base_url":"local","prompt":"first","session_id":id,"turn_index":0}),
            json!({"schema":EXCHANGE_SCHEMA,"event":"tool_round","ts_ms":3,"content":[{"type":"text","text":"thinking"},{"type":"tool_use","id":"call-1","name":"bash","input":{"command":"pwd"}}],"session_id":id,"turn_index":0}),
            json!({"schema":EXCHANGE_SCHEMA,"event":"tool_call","ts_ms":4,"tool_use_id":"call-1","tool_name":"bash","input":{"command":"pwd"},"session_id":id,"turn_index":0}),
            json!({"schema":EXCHANGE_SCHEMA,"event":"tool_result","ts_ms":5,"tool_use_id":"call-1","tool_name":"bash","status":"completed","result":"here","session_id":id,"turn_index":0}),
            json!({"schema":EXCHANGE_SCHEMA,"event":"response_ok","ts_ms":6,"reply":"done","stop_reason":"end_turn","session_id":id,"turn_index":0}),
            json!({"schema":EXCHANGE_SCHEMA,"event":"future_event","ts_ms":7,"new_field":true}),
            json!({"schema":EXCHANGE_SCHEMA,"event":event_suffix,"ts_ms":8,"session_id":id,"turn_index":1,"prompt":"second"}),
        ];
        events
            .into_iter()
            .map(|event| serde_json::to_string(&event).unwrap() + "\n")
            .collect()
    }

    #[test]
    fn trail_browser_pairs_tools_and_keeps_known_history_for_future_events() {
        let scratch = tempdir().unwrap();
        let id = "sess-12-34";
        let path = scratch.path().join(format!("{id}.jsonl"));
        fs::write(&path, sample_trail(id, "request")).unwrap();
        let snapshot = read_trail(&path, id).unwrap();
        assert_eq!(snapshot.turns.len(), 2);
        assert_eq!(snapshot.turns[0].outcome, TrailOutcome::Succeeded);
        assert_eq!(snapshot.turns[0].timestamp_ms, Some(2));
        assert_eq!(snapshot.turns[0].outcome_timestamp_ms, Some(6));
        assert_eq!(snapshot.turns[0].stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(snapshot.turns[0].tools[0].timestamp_ms, Some(4));
        assert_eq!(
            snapshot.turns[0].tools[0]
                .result
                .as_ref()
                .unwrap()
                .result
                .as_deref(),
            Some("here")
        );
        assert_eq!(
            snapshot.turns[0].tools[0]
                .result
                .as_ref()
                .unwrap()
                .timestamp_ms,
            Some(5)
        );
        assert_eq!(snapshot.turns[1].outcome, TrailOutcome::Incomplete);
        assert!(snapshot.warnings.is_empty());
        assert!(!snapshot.read_only);
    }

    #[test]
    fn malformed_or_partial_trail_is_visible_and_read_only() {
        let scratch = tempdir().unwrap();
        let id = "sess-12-35";
        let path = scratch.path().join(format!("{id}.jsonl"));
        let request = json!({
            "schema": EXCHANGE_SCHEMA,
            "event": "request",
            "ts_ms": 2,
            "model": "m",
            "base_url": "local",
            "prompt": "known",
            "session_id": id,
            "turn_index": 0
        });
        let malformed_success = json!({
            "schema": EXCHANGE_SCHEMA,
            "event": "response_ok",
            "ts_ms": 3,
            "session_id": id,
            "turn_index": 0
        });
        fs::write(
            &path,
            format!(
                "{}\n{}\n{{\"event\":\"request\"",
                serde_json::to_string(&request).unwrap(),
                serde_json::to_string(&malformed_success).unwrap(),
            ),
        )
        .unwrap();
        let snapshot = read_trail(&path, id).unwrap();
        assert_eq!(snapshot.turns.len(), 1);
        assert_eq!(snapshot.turns[0].outcome, TrailOutcome::Incomplete);
        assert!(snapshot.read_only);
        assert_eq!(snapshot.warnings.len(), 2);
    }

    fn attempt(token: &str, candidate: &str, owner: ProcessOwner) -> PendingAttempt {
        PendingAttempt {
            token: token.to_string(),
            revision: 0,
            controller: Some(owner),
            supervisor: None,
            candidate_session_id: candidate.to_string(),
            native_session_id: None,
            controller_released: false,
            recovery_warning: None,
        }
    }

    #[test]
    fn stale_recovery_revision_cannot_clear_a_new_supervisor_handoff() {
        let scratch = tempdir().unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(scratch.path().join("state")),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let draft = catalog
            .create_draft(SessionInterface::Tui, Some("held".into()), None)
            .unwrap();
        let candidate = "sess-91-109";
        let owner = ProcessOwner::current().unwrap();
        let mut stale_owner = owner.clone();
        stale_owner.birth_token.push_str("-old-incarnation");
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt = Some(attempt("token-1", candidate, stale_owner));
                Ok(())
            })
            .unwrap();
        let observed_revision = catalog.read_index().unwrap().sessions[&draft.id]
            .pending_attempt
            .as_ref()
            .unwrap()
            .revision;

        catalog
            .record_supervisor_owner(&draft.id, "token-1", candidate, owner)
            .unwrap();
        catalog
            .clear_pending_attempt_if_current(&draft.id, "token-1", observed_revision)
            .unwrap();

        let pending = catalog.get(&draft.id).unwrap();
        assert!(pending.pending_new_turn);
        let stored = catalog.read_index().unwrap();
        let handoff = stored.sessions[&draft.id].pending_attempt.as_ref().unwrap();
        assert_eq!(handoff.revision, 1);
        assert!(handoff.supervisor.is_some());
        assert!(!catalog.trail_path(candidate).exists());
    }

    #[test]
    fn old_attempt_updates_cannot_change_a_newer_reservation() {
        let scratch = tempdir().unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(scratch.path().join("state")),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let draft = catalog
            .create_draft(SessionInterface::Web, Some("newer".into()), None)
            .unwrap();
        let owner = ProcessOwner::current().unwrap();
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt = Some(attempt("old-token", "sess-91-1", owner.clone()));
                Ok(())
            })
            .unwrap();
        catalog
            .clear_pending_attempt_if_current(&draft.id, "old-token", 0)
            .unwrap();
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt = Some(attempt("new-token", "sess-91-2", owner));
                Ok(())
            })
            .unwrap();

        assert!(
            catalog
                .record_supervisor_owner(
                    &draft.id,
                    "old-token",
                    "sess-91-1",
                    ProcessOwner::current().unwrap()
                )
                .is_err()
        );
        catalog
            .release_controller_attempt(&draft.id, "old-token")
            .unwrap();

        let stored = catalog.read_index().unwrap();
        let current = stored.sessions[&draft.id].pending_attempt.as_ref().unwrap();
        assert_eq!(current.token, "new-token");
        assert_eq!(current.candidate_session_id, "sess-91-2");
        assert!(current.supervisor.is_none());
        assert!(!catalog.trail_path("sess-91-1").exists());
        assert!(!catalog.trail_path("sess-91-2").exists());
    }

    #[test]
    fn candidate_trail_without_supervisor_handoff_is_never_adopted() {
        let scratch = tempdir().unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(scratch.path().join("state")),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let draft = catalog
            .create_draft(SessionInterface::Tui, Some("preserve me".into()), None)
            .unwrap();
        let candidate = "sess-91-4";
        let mut reused = ProcessOwner::current().unwrap();
        reused.birth_token.push_str("-reused");
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt = Some(attempt("token-4", candidate, reused));
                Ok(())
            })
            .unwrap();
        fs::write(
            catalog.trail_path(candidate),
            sample_trail(candidate, "session_end"),
        )
        .unwrap();

        catalog.reconcile_pending_attempts(None).unwrap();
        let pending = catalog.get(&draft.id).unwrap();
        assert!(pending.pending_new_turn);
        assert_eq!(pending.run_state, CatalogRunState::Unknown);
        assert!(
            pending
                .warnings
                .iter()
                .any(|warning| { warning.contains("without a token-matched supervisor handoff") })
        );
        assert!(catalog.trail_path(candidate).exists());
        assert!(
            !catalog
                .read_index()
                .unwrap()
                .sessions
                .contains_key(candidate)
        );
    }

    #[test]
    fn concurrent_recovery_adopts_one_token_matched_trail_with_metadata() {
        let scratch = tempdir().unwrap();
        let state_dir = scratch.path().join("state");
        let workspace = scratch.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state_dir),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let draft = catalog
            .create_draft(
                SessionInterface::Web,
                Some("exact attempt".into()),
                Some(&workspace),
            )
            .unwrap();
        catalog
            .save_draft(&draft.id, SessionInterface::Tui, "TUI draft".into())
            .unwrap();
        catalog
            .save_draft(&draft.id, SessionInterface::Web, "Web draft".into())
            .unwrap();
        catalog
            .save_display_metadata(&draft.id, "accent".into(), json!("amber"))
            .unwrap();

        let candidate = "sess-91-3";
        let owner = ProcessOwner::current().unwrap();
        let mut old_owner = owner;
        old_owner.birth_token.push_str("-old-incarnation");
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt = Some(attempt("token-3", candidate, old_owner.clone()));
                Ok(())
            })
            .unwrap();
        fs::write(
            catalog.trail_path(candidate),
            sample_trail(candidate, "session_end"),
        )
        .unwrap();
        catalog
            .record_supervisor_owner(&draft.id, "token-3", candidate, old_owner.clone())
            .unwrap();
        catalog
            .record_native_session_handoff(&draft.id, "token-3", candidate, candidate, &old_owner)
            .unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut recoveries = Vec::new();
        for _ in 0..2 {
            let catalog = catalog.clone();
            let barrier = Arc::clone(&barrier);
            recoveries.push(std::thread::spawn(move || {
                barrier.wait();
                catalog.reconcile_pending_attempts(None)
            }));
        }
        barrier.wait();
        for recovery in recoveries {
            recovery.join().unwrap().unwrap();
        }

        let entries = catalog.list().unwrap();
        assert_eq!(
            entries.iter().filter(|entry| entry.id == candidate).count(),
            1
        );
        assert!(!entries.iter().any(|entry| entry.id == draft.id));
        let adopted = catalog.get(candidate).unwrap();
        assert_eq!(adopted.name.as_deref(), Some("exact attempt"));
        assert_eq!(
            adopted.cwd.as_deref(),
            Some(fs::canonicalize(workspace).unwrap().as_path())
        );
        assert_eq!(adopted.drafts[&SessionInterface::Tui], "TUI draft");
        assert_eq!(adopted.drafts[&SessionInterface::Web], "Web draft");
        assert_eq!(adopted.display["accent"], json!("amber"));
    }

    #[test]
    fn concurrent_pending_drafts_bind_only_their_token_matched_sessions() {
        let scratch = tempdir().unwrap();
        let workspace = scratch.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(scratch.path().join("state")),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let first = catalog
            .create_draft(
                SessionInterface::Web,
                Some("first attempt".into()),
                Some(&workspace),
            )
            .unwrap();
        let second = catalog
            .create_draft(
                SessionInterface::Tui,
                Some("second attempt".into()),
                Some(&workspace),
            )
            .unwrap();
        catalog
            .save_draft(&first.id, SessionInterface::Web, "first prompt".into())
            .unwrap();
        catalog
            .save_draft(&second.id, SessionInterface::Tui, "second prompt".into())
            .unwrap();

        let first_session = "sess-91-51";
        let second_session = "sess-91-52";
        let current_owner = ProcessOwner::current().unwrap();
        let mut first_owner = current_owner.clone();
        first_owner.birth_token.push_str("-first-old");
        let mut second_owner = current_owner;
        second_owner.birth_token.push_str("-second-old");
        catalog
            .update_index(|index| {
                index.sessions.get_mut(&first.id).unwrap().pending_new_turn = true;
                index.sessions.get_mut(&first.id).unwrap().pending_attempt =
                    Some(attempt("first-token", first_session, first_owner.clone()));
                index.sessions.get_mut(&second.id).unwrap().pending_new_turn = true;
                index.sessions.get_mut(&second.id).unwrap().pending_attempt = Some(attempt(
                    "second-token",
                    second_session,
                    second_owner.clone(),
                ));
                Ok(())
            })
            .unwrap();
        for (draft_id, token, session_id, owner) in [
            (&first.id, "first-token", first_session, &first_owner),
            (&second.id, "second-token", second_session, &second_owner),
        ] {
            fs::write(
                catalog.trail_path(session_id),
                sample_trail(session_id, "session_end"),
            )
            .unwrap();
            catalog
                .record_supervisor_owner(draft_id, token, session_id, owner.clone())
                .unwrap();
            catalog
                .record_native_session_handoff(draft_id, token, session_id, session_id, owner)
                .unwrap();
        }

        catalog.reconcile_pending_attempts(None).unwrap();
        assert!(matches!(
            catalog.get(&first.id),
            Err(CatalogError::NotFound(_))
        ));
        assert!(matches!(
            catalog.get(&second.id),
            Err(CatalogError::NotFound(_))
        ));
        let first_bound = catalog.get(first_session).unwrap();
        let second_bound = catalog.get(second_session).unwrap();
        assert_eq!(first_bound.name.as_deref(), Some("first attempt"));
        assert_eq!(second_bound.name.as_deref(), Some("second attempt"));
        assert_eq!(first_bound.drafts[&SessionInterface::Web], "first prompt");
        assert_eq!(second_bound.drafts[&SessionInterface::Tui], "second prompt");
        let index = catalog.read_index().unwrap();
        assert_eq!(
            index.sessions[first_session].bound_attempt_token.as_deref(),
            Some("first-token")
        );
        assert_eq!(
            index.sessions[second_session]
                .bound_attempt_token
                .as_deref(),
            Some("second-token")
        );
    }

    #[test]
    fn pending_handoff_merges_metadata_written_to_recovered_candidate() {
        let scratch = tempdir().unwrap();
        let state = scratch.path().join("state");
        let draft_workspace = scratch.path().join("draft-workspace");
        let candidate_workspace = scratch.path().join("candidate-workspace");
        fs::create_dir_all(&draft_workspace).unwrap();
        fs::create_dir_all(&candidate_workspace).unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state.clone()),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let draft = catalog
            .create_draft(
                SessionInterface::Web,
                Some("draft title".into()),
                Some(&draft_workspace),
            )
            .unwrap();
        catalog
            .save_draft(&draft.id, SessionInterface::Web, "saved web prompt".into())
            .unwrap();
        catalog
            .save_display_metadata(&draft.id, "draft-color".into(), serde_json::json!("blue"))
            .unwrap();

        let candidate = "sess-91-78";
        let owner = ProcessOwner::current().unwrap();
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt = Some(attempt("token-merge", candidate, owner.clone()));
                Ok(())
            })
            .unwrap();
        fs::write(
            catalog.trail_path(candidate),
            sample_trail(candidate, "session_end"),
        )
        .unwrap();
        catalog
            .record_supervisor_owner(&draft.id, "token-merge", candidate, owner.clone())
            .unwrap();
        catalog
            .record_native_session_handoff(&draft.id, "token-merge", candidate, candidate, &owner)
            .unwrap();

        let visible = catalog.list().unwrap();
        assert!(visible.iter().any(|session| session.id == candidate));
        catalog
            .rename(candidate, Some("candidate title".into()))
            .unwrap();
        catalog
            .save_draft(candidate, SessionInterface::Tui, "edited TUI draft".into())
            .unwrap();
        catalog
            .save_display_metadata(
                candidate,
                "candidate-color".into(),
                serde_json::json!("red"),
            )
            .unwrap();
        catalog
            .set_workspace(candidate, &candidate_workspace)
            .unwrap();

        let mut dead_owner = owner;
        dead_owner.birth_token.push_str("-old-incarnation");
        catalog
            .update_index(|index| {
                let attempt = index
                    .sessions
                    .get_mut(&draft.id)
                    .unwrap()
                    .pending_attempt
                    .as_mut()
                    .unwrap();
                attempt.controller = Some(dead_owner.clone());
                attempt.supervisor = Some(dead_owner);
                Ok(())
            })
            .unwrap();

        let reopened = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state),
            ..SessionCatalogConfig::default()
        })
        .expect("metadata on a visible trail must not brick catalog recovery");
        let adopted = reopened
            .get(candidate)
            .expect("adopted session remains readable");
        assert_eq!(adopted.name.as_deref(), Some("candidate title"));
        assert_eq!(
            adopted.cwd.as_deref(),
            Some(fs::canonicalize(candidate_workspace).unwrap().as_path())
        );
        assert_eq!(adopted.drafts[&SessionInterface::Web], "saved web prompt");
        assert_eq!(adopted.drafts[&SessionInterface::Tui], "edited TUI draft");
        assert_eq!(adopted.display["draft-color"], serde_json::json!("blue"));
        assert_eq!(adopted.display["candidate-color"], serde_json::json!("red"));
        assert!(!adopted.pending_new_turn);
        assert!(!adopted.recovered);
        let index = reopened.read_index().unwrap();
        assert!(!index.sessions.contains_key(&draft.id));
        assert_eq!(
            index.sessions[candidate].bound_attempt_token.as_deref(),
            Some("token-merge")
        );
    }

    #[test]
    fn pending_recovery_failure_degrades_only_its_draft_to_unknown() {
        let scratch = tempdir().unwrap();
        let state = scratch.path().join("state");
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state.clone()),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let blocked = catalog
            .create_draft(SessionInterface::Web, Some("blocked".into()), None)
            .unwrap();
        let recoverable = catalog
            .create_draft(SessionInterface::Tui, Some("recoverable".into()), None)
            .unwrap();
        let current_owner = ProcessOwner::current().unwrap();
        let mut dead_owner = current_owner;
        dead_owner.birth_token.push_str("-old-incarnation");

        for (draft_id, token, candidate) in [
            (&blocked.id, "blocked-token", "sess-91-79"),
            (&recoverable.id, "recoverable-token", "sess-91-80"),
        ] {
            catalog
                .update_index(|index| {
                    let record = index.sessions.get_mut(draft_id).unwrap();
                    record.pending_new_turn = true;
                    record.pending_attempt = Some(attempt(token, candidate, dead_owner.clone()));
                    Ok(())
                })
                .unwrap();
            fs::write(
                catalog.trail_path(candidate),
                sample_trail(candidate, "session_end"),
            )
            .unwrap();
            catalog
                .record_supervisor_owner(draft_id, token, candidate, dead_owner.clone())
                .unwrap();
            catalog
                .record_native_session_handoff(draft_id, token, candidate, candidate, &dead_owner)
                .unwrap();
        }
        catalog
            .rename("sess-91-79", Some("conflicting record".into()))
            .unwrap();
        catalog
            .update_index(|index| {
                index
                    .sessions
                    .get_mut("sess-91-79")
                    .unwrap()
                    .bound_attempt_token = Some("different-token".into());
                Ok(())
            })
            .unwrap();

        let reopened = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state),
            ..SessionCatalogConfig::default()
        })
        .expect("one conflicting recovery must not prevent catalog startup");
        let blocked_session = reopened
            .get(&blocked.id)
            .expect("the unrecoverable draft remains readable");
        assert_eq!(blocked_session.run_state, CatalogRunState::Unknown);
        assert!(blocked_session.warnings.iter().any(|warning| {
            warning.contains("start a new conversation and copy them")
                && warning.contains("No prompt was replayed")
        }));
        assert!(reopened.list().is_ok());
        assert!(reopened.get("sess-91-79").is_ok());
        let adopted = reopened
            .get("sess-91-80")
            .expect("another draft must still recover");
        assert_eq!(
            reopened.read_index().unwrap().sessions["sess-91-80"]
                .bound_attempt_token
                .as_deref(),
            Some("recoverable-token")
        );
        assert_eq!(adopted.id, "sess-91-80");
    }

    #[test]
    fn delayed_old_handoff_and_failure_cannot_change_a_new_bound_attempt() {
        let scratch = tempdir().unwrap();
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(scratch.path().join("state")),
            ..SessionCatalogConfig::default()
        })
        .unwrap();
        let draft = catalog
            .create_draft(SessionInterface::Web, Some("latest work".into()), None)
            .unwrap();
        let old_session = "sess-91-61";
        let new_session = "sess-91-62";
        let current_owner = ProcessOwner::current().unwrap();
        let mut dead_owner = current_owner;
        dead_owner.birth_token.push_str("-old-incarnation");
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt =
                    Some(attempt("old-token", old_session, dead_owner.clone()));
                Ok(())
            })
            .unwrap();
        catalog
            .record_supervisor_owner(&draft.id, "old-token", old_session, dead_owner.clone())
            .unwrap();

        let ready = Arc::new(std::sync::Barrier::new(2));
        let release_update = Arc::new(std::sync::Barrier::new(2));
        let delayed_catalog = catalog.clone();
        let delayed_draft = draft.id.clone();
        let delayed_owner = dead_owner.clone();
        let update_ready = Arc::clone(&ready);
        let update_release = Arc::clone(&release_update);
        let delayed_update = std::thread::spawn(move || {
            update_ready.wait();
            update_release.wait();
            let handoff = delayed_catalog.record_native_session_handoff(
                &delayed_draft,
                "old-token",
                old_session,
                old_session,
                &delayed_owner,
            );
            let failure = delayed_catalog.release_controller_attempt(&delayed_draft, "old-token");
            (handoff, failure)
        });

        ready.wait();
        catalog.reconcile_pending_attempts(Some(&draft.id)).unwrap();
        assert!(!catalog.get(&draft.id).unwrap().pending_new_turn);
        catalog
            .update_index(|index| {
                let record = index.sessions.get_mut(&draft.id).unwrap();
                record.pending_new_turn = true;
                record.pending_attempt =
                    Some(attempt("new-token", new_session, dead_owner.clone()));
                Ok(())
            })
            .unwrap();
        fs::write(
            catalog.trail_path(new_session),
            sample_trail(new_session, "session_end"),
        )
        .unwrap();
        catalog
            .record_supervisor_owner(&draft.id, "new-token", new_session, dead_owner.clone())
            .unwrap();
        catalog
            .record_native_session_handoff(
                &draft.id,
                "new-token",
                new_session,
                new_session,
                &dead_owner,
            )
            .unwrap();
        catalog.reconcile_pending_attempts(Some(&draft.id)).unwrap();

        release_update.wait();
        let (stale_handoff, stale_failure) = delayed_update.join().unwrap();
        assert!(stale_handoff.is_err());
        stale_failure.unwrap();
        assert!(matches!(
            catalog.get(&draft.id),
            Err(CatalogError::NotFound(_))
        ));
        let bound = catalog.get(new_session).unwrap();
        assert_eq!(bound.name.as_deref(), Some("latest work"));
        let index = catalog.read_index().unwrap();
        assert_eq!(
            index.sessions[new_session].bound_attempt_token.as_deref(),
            Some("new-token")
        );
        assert!(!catalog.trail_path(old_session).exists());
    }
}
