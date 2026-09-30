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
    Client, ClientConfig, ClientError, LegSession, StartError, TurnHandle, TurnOutcome, TurnRequest,
};
use crate::protocol::StreamEvent;
use crate::supervisor;

const INDEX_NAME: &str = "catalog.json";
const LOCK_NAME: &str = ".catalog.lock";
const INDEX_VERSION: u32 = 1;
const EXCHANGE_SCHEMA: &str = "baton.exchange/v1";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    pub result: Option<TrailToolResult>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub struct TrailTurn {
    pub turn_index: u64,
    pub prompt: String,
    #[serde(skip)]
    retry_prompt: Option<String>,
    pub outcome: TrailOutcome,
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
        "Retry can run tools again; side effects may be repeated."
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
    #[serde(default)]
    recovered: bool,
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
        let _guard = catalog.lock_index()?;
        if !catalog.index_path().exists() {
            catalog.write_index_unlocked(&CatalogIndex {
                version: INDEX_VERSION,
                sessions: BTreeMap::new(),
            })?;
        } else {
            let _ = catalog.read_index_unlocked()?;
        }
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
        validate_session_id(draft_id)?;
        if !is_draft_id(draft_id) {
            return Err(CatalogError::NotFound(draft_id.into()));
        }
        let prompt = prompt.into();
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        let record = index
            .sessions
            .get_mut(draft_id)
            .ok_or_else(|| CatalogError::NotFound(draft_id.into()))?;
        if record.pending_new_turn {
            return Err(CatalogError::Busy);
        }
        let cwd = self.validated_cwd(draft_id, record)?;
        record.pending_new_turn = true;
        record.updated_at_ms = now_ms();
        self.write_index_unlocked(&index)?;

        let client = self.client();
        let request = TurnRequest::new(prompt, cwd, LegSession::New);
        match client.start(request) {
            Ok(turn) => Ok(CatalogTurn {
                turn,
                catalog: self.clone(),
                draft_id: Some(draft_id.to_string()),
                session_id: None,
                pending_event: VecDeque::new(),
                finished: None,
            }),
            Err(error) => {
                if let Some(record) = index.sessions.get_mut(draft_id) {
                    record.pending_new_turn = false;
                    record.updated_at_ms = now_ms();
                }
                self.write_index_unlocked(&index)?;
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
            if record.pending_new_turn {
                match supervisor::creation_lock_is_held(&self.inner.sessions_dir) {
                    Ok(true) => CatalogRunState::Active,
                    Ok(false) => CatalogRunState::Unknown,
                    Err(error) => {
                        warnings.push(error);
                        read_only = true;
                        CatalogRunState::Unknown
                    }
                }
            } else {
                CatalogRunState::Idle
            }
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

    fn bind_session_id(&self, draft_id: &str, session_id: &str) -> Result<(), CatalogError> {
        validate_session_id(session_id)?;
        if is_draft_id(session_id) {
            return Err(CatalogError::InvalidSessionId);
        }
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if index.sessions.contains_key(session_id) {
            return Err(CatalogError::AlreadyExists(session_id.into()));
        }
        let mut record = index
            .sessions
            .remove(draft_id)
            .ok_or_else(|| CatalogError::NotFound(draft_id.into()))?;
        record.pending_new_turn = false;
        record.updated_at_ms = now_ms();
        index.sessions.insert(session_id.to_string(), record);
        self.write_index_unlocked(&index)
    }

    fn clear_pending_draft(&self, draft_id: &str) -> Result<(), CatalogError> {
        let _guard = self.lock_index()?;
        let mut index = self.read_index_unlocked()?;
        if let Some(record) = index.sessions.get_mut(draft_id) {
            record.pending_new_turn = false;
            record.updated_at_ms = now_ms();
            self.write_index_unlocked(&index)?;
        }
        Ok(())
    }
}

pub struct CatalogTurn {
    turn: TurnHandle,
    catalog: SessionCatalog,
    draft_id: Option<String>,
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
            match self.catalog.bind_session_id(draft_id, session_id) {
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

    pub fn wait(&mut self) -> Result<TurnOutcome, CatalogTurnError> {
        if let Some(outcome) = &self.finished {
            return Ok(outcome.clone());
        }
        while self.observe()?.is_some() {}
        let outcome = self.turn.wait()?;
        self.finished = Some(outcome.clone());
        if let Some(draft_id) = self.draft_id.take() {
            self.catalog
                .clear_pending_draft(&draft_id)
                .map_err(|error| CatalogTurnError::Catalog(error.to_string()))?;
        }
        Ok(outcome)
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
                    retry_prompt: original_prompt,
                    outcome: TrailOutcome::Incomplete,
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
            json!({"schema":EXCHANGE_SCHEMA,"event":"response_ok","ts_ms":6,"reply":"done","session_id":id,"turn_index":0}),
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
        assert_eq!(
            snapshot.turns[0].tools[0]
                .result
                .as_ref()
                .unwrap()
                .result
                .as_deref(),
            Some("here")
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
}
