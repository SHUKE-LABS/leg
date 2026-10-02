mod editor;
mod sanitize;
mod transcript;

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use editor::{ComposerEditor, normalize_paste};
use leg_ui_client::{
    CatalogRunState, CatalogSession, CatalogTurn, RetryIntent, SessionCatalog,
    SessionCatalogConfig, SessionInterface, StreamEvent, TurnOutcome, TurnStopHandle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use signal_hook::low_level::unregister;
use transcript::TranscriptTurn;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const WARNING: &str = "Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.";
const WARNING_ACK_KEY: &str = "tui_first_run_warning_acknowledged";
const HELP: &str = "Ctrl-S send · F3 sessions · Ctrl-F search · F4 inspect · F5 copy · F6 export · F7 save copy · F1 help · F2 actions · Ctrl-C stop/exit";
const NARROW_HELP: &str = "Ctrl-S send · Ctrl-C stop/exit · F1 help · F2 actions · F3 sessions";
const MIN_TERMINAL_COLUMNS: u16 = 80;
const MIN_TERMINAL_ROWS: u16 = 24;
const SMALL_TERMINAL_REJECTION: &str = "Send rejected: terminal must be at least 80x24.";
static EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Args {
    leg_bin: Option<PathBuf>,
    supervisor_bin: Option<PathBuf>,
}

enum ParsedArgs {
    Run(Args),
    Help,
}

fn main() {
    if let Err(error) = entry() {
        eprintln!("leg-tui: {error}");
        process::exit(1);
    }
}

fn entry() -> Result<(), Box<dyn Error>> {
    match parse_args()? {
        ParsedArgs::Help => {
            print_help();
            Ok(())
        }
        ParsedArgs::Run(args) => run(args),
    }
}

fn parse_args() -> Result<ParsedArgs, String> {
    let mut leg_bin = None;
    let mut supervisor_bin = None;
    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--help" | "-h") => return Ok(ParsedArgs::Help),
            Some("--leg-bin") => {
                leg_bin = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| "--leg-bin requires a path".to_string())?,
                );
            }
            Some("--supervisor-bin") => {
                supervisor_bin = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| "--supervisor-bin requires a path".to_string())?,
                );
            }
            _ => return Err(format!("unknown argument {argument:?}; use --help")),
        }
    }
    Ok(ParsedArgs::Run(Args {
        leg_bin,
        supervisor_bin,
    }))
}

fn print_help() {
    println!(
        "leg-tui — experimental terminal interface for leg\n\n\
         Usage: leg-tui [--leg-bin PATH] [--supervisor-bin PATH]\n\n\
         Choose an existing workspace, review the first-run warning, then compose a prompt.\n\
         Ctrl-S sends. Ctrl-C stops an active turn or exits when idle.\n\
         Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox."
    );
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    require_interactive_terminal()?;
    let signals = ProcessSignals::install()?;
    let _terminal_guard = TerminalGuard::enter()?;
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        leg_bin: args.leg_bin,
        supervisor_bin: args.supervisor_bin,
        ..SessionCatalogConfig::default()
    })?;
    let mut app = App::new(catalog);
    app.refresh_sessions();
    if !app.sessions.is_empty() {
        app.screen = Screen::SessionPicker;
        app.status = "Choose an existing session or create a new one".to_string();
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    while !app.quit {
        let area = terminal.size()?;
        app.set_terminal_size(area.width, area.height);
        terminal.draw(|frame| draw(frame, &mut app))?;
        app.receive_turn_messages();
        if let Some(signal) = signals.take() {
            app.handle_external_signal(signal);
        }
        if app.quit {
            break;
        }
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Paste(text) => app.handle_paste(&text),
                Event::Resize(columns, rows) => app.set_terminal_size(columns, rows),
                _ => {}
            }
        }
    }
    app.persist_all_drafts()?;
    Ok(())
}

fn require_interactive_terminal() -> io::Result<()> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "leg-tui requires a terminal for both stdin and stdout; run it in a terminal or use --help for usage",
    ))
}

fn should_use_color(no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    no_color.is_none_or(|value| value.is_empty()) && term != Some(OsStr::new("dumb"))
}

fn terminal_color_enabled() -> bool {
    let no_color = env::var_os("NO_COLOR");
    let term = env::var_os("TERM");
    should_use_color(no_color.as_deref(), term.as_deref())
}

#[derive(Clone, Copy)]
enum ExternalSignal {
    Interrupt,
    Terminate,
}

struct ProcessSignals {
    interrupt: Arc<AtomicBool>,
    terminate: Arc<AtomicBool>,
    registrations: Vec<SigId>,
}

impl ProcessSignals {
    fn install() -> io::Result<Self> {
        let interrupt = Arc::new(AtomicBool::new(false));
        let terminate = Arc::new(AtomicBool::new(false));
        let interrupt_id = flag::register(SIGINT, Arc::clone(&interrupt))?;
        let terminate_id = match flag::register(SIGTERM, Arc::clone(&terminate)) {
            Ok(id) => id,
            Err(error) => {
                unregister(interrupt_id);
                return Err(error);
            }
        };
        Ok(Self {
            interrupt,
            terminate,
            registrations: vec![interrupt_id, terminate_id],
        })
    }

    fn take(&self) -> Option<ExternalSignal> {
        if self.terminate.swap(false, Ordering::Relaxed) {
            Some(ExternalSignal::Terminate)
        } else if self.interrupt.swap(false, Ordering::Relaxed) {
            Some(ExternalSignal::Interrupt)
        } else {
            None
        }
    }
}

impl Drop for ProcessSignals {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            unregister(id);
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Workspace,
    Warning,
    Conversation,
    SessionPicker,
    Search,
    Rename,
    Export,
    CopyFallback,
}

#[derive(Clone)]
enum WorkspaceFlow {
    Initial,
    NewSession,
    ReplaceSession(String),
}

enum TurnMessage {
    Event {
        owner: String,
        event: StreamEvent,
    },
    Finished {
        owner: String,
        result: Result<TurnOutcome, String>,
    },
}

struct App {
    catalog: SessionCatalog,
    screen: Screen,
    workspace_flow: WorkspaceFlow,
    warning_acknowledged: bool,
    workspace_input: String,
    view: ConversationState,
    views: HashMap<String, ConversationState>,
    view_aliases: HashMap<String, String>,
    sessions: Vec<CatalogSession>,
    picker_index: usize,
    session_filter: String,
    picker_searching: bool,
    dialog_input: String,
    dialog_target: Option<String>,
    search_query: String,
    search_entries: Vec<SearchEntry>,
    search_hits: Vec<SearchHit>,
    search_index: usize,
    inspector_open: bool,
    inspector_turn: usize,
    inspector_field: usize,
    inspector_scroll: usize,
    inspector_scroll_step: usize,
    overwrite_export: Option<PathBuf>,
    copy_text: Option<String>,
    terminal_columns: u16,
    terminal_rows: u16,
    use_color: bool,
    show_help: bool,
    show_menu: bool,
    quit: bool,
    exit_after_turn: bool,
    turn_tx: Sender<TurnMessage>,
    turn_rx: Receiver<TurnMessage>,
}

struct ConversationState {
    state_key: String,
    workspace: Option<PathBuf>,
    draft_id: Option<String>,
    session_id: Option<String>,
    composer: ComposerEditor,
    transcript: Vec<TranscriptTurn>,
    status: String,
    active_tool: Option<String>,
    turn_status: TurnStatus,
    started_at: Option<Instant>,
    last_elapsed: Duration,
    terminal_detail: Option<String>,
    active_prompt: Option<String>,
    active_draft_edited: bool,
    retry_confirmation: Option<RetryIntent>,
    transcript_follow_tail: bool,
    transcript_scroll: usize,
    transcript_max_scroll: usize,
    transcript_page_rows: usize,
    transcript_new_content: bool,
    model: String,
    stop_handle: Option<TurnStopHandle>,
    read_only: bool,
    recovered: bool,
    external_run_state: String,
}

struct SearchHit {
    session_id: String,
    turn_index: Option<usize>,
    label: String,
    excerpt: String,
}

struct SearchEntry {
    session_id: String,
    turn_index: Option<usize>,
    label: String,
    text: String,
}

impl ConversationState {
    fn empty() -> Self {
        Self {
            state_key: String::new(),
            workspace: None,
            draft_id: None,
            session_id: None,
            composer: ComposerEditor::default(),
            transcript: Vec::new(),
            status: "Choose a workspace".to_string(),
            active_tool: None,
            turn_status: TurnStatus::Idle,
            started_at: None,
            last_elapsed: Duration::ZERO,
            terminal_detail: None,
            active_prompt: None,
            active_draft_edited: false,
            retry_confirmation: None,
            transcript_follow_tail: true,
            transcript_scroll: 0,
            transcript_max_scroll: 0,
            transcript_page_rows: 1,
            transcript_new_content: false,
            model: env::var("LEG_MODEL").unwrap_or_else(|_| "provider default".to_string()),
            stop_handle: None,
            read_only: false,
            recovered: false,
            external_run_state: "idle".to_string(),
        }
    }

    fn from_catalog(session: &CatalogSession) -> Self {
        let is_draft = session.id.starts_with("draft-");
        let mut composer = ComposerEditor::default();
        composer.set_text(
            session
                .drafts
                .get(&SessionInterface::Tui)
                .cloned()
                .unwrap_or_default(),
        );
        let mut state = Self::empty();
        state.state_key = session.id.clone();
        state.workspace = session.cwd.clone();
        state.draft_id = Some(session.id.clone());
        state.session_id = (!is_draft).then(|| session.id.clone());
        state.composer = composer;
        let persisted_warnings = session
            .display
            .get("tui_completion_warnings")
            .and_then(serde_json::Value::as_object);
        state.transcript = session
            .turns
            .iter()
            .map(|turn| {
                let mut transcript = TranscriptTurn::from_trail(turn);
                if let Some(warning) = persisted_warnings
                    .and_then(|warnings| warnings.get(&turn.turn_index.to_string()))
                    .and_then(serde_json::Value::as_str)
                {
                    transcript.set_completion_warning(warning);
                }
                transcript
            })
            .collect();
        state.read_only = session.read_only;
        state.recovered = session.recovered;
        state.external_run_state = format!("{:?}", session.run_state).to_lowercase();
        state.transcript_follow_tail = true;
        state.transcript_scroll = 0;
        state.status = session_action_status(session);
        state
    }
}

impl Deref for App {
    type Target = ConversationState;

    fn deref(&self) -> &Self::Target {
        &self.view
    }
}

impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.view
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TurnStatus {
    Idle,
    Starting,
    Running,
    Stopping,
    Succeeded,
    Failed,
    Interrupted,
    Incomplete { forced: bool },
    Capped,
    Truncated,
}

impl TurnStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    fn is_stopping(&self) -> bool {
        matches!(self, Self::Stopping)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
            Self::Incomplete { .. } => "Incomplete",
            Self::Capped => "Capped",
            Self::Truncated => "Succeeded (truncated)",
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Incomplete { forced: true } => "Incomplete (forced cleanup)".to_string(),
            _ => self.label().to_string(),
        }
    }

    fn mark_running_unless_stopping(&mut self) {
        if !self.is_stopping() {
            *self = Self::Running;
        }
    }
}

impl App {
    fn new(catalog: SessionCatalog) -> Self {
        let (turn_tx, turn_rx) = mpsc::channel();
        Self {
            catalog,
            screen: Screen::Workspace,
            workspace_flow: WorkspaceFlow::Initial,
            warning_acknowledged: false,
            workspace_input: String::new(),
            view: ConversationState::empty(),
            views: HashMap::new(),
            view_aliases: HashMap::new(),
            sessions: Vec::new(),
            picker_index: 0,
            session_filter: String::new(),
            picker_searching: false,
            dialog_input: String::new(),
            dialog_target: None,
            search_query: String::new(),
            search_entries: Vec::new(),
            search_hits: Vec::new(),
            search_index: 0,
            inspector_open: false,
            inspector_turn: 0,
            inspector_field: 0,
            inspector_scroll: 0,
            inspector_scroll_step: 1,
            overwrite_export: None,
            copy_text: None,
            terminal_columns: MIN_TERMINAL_COLUMNS,
            terminal_rows: MIN_TERMINAL_ROWS,
            use_color: terminal_color_enabled(),
            show_help: false,
            show_menu: false,
            quit: false,
            exit_after_turn: false,
            turn_tx,
            turn_rx,
        }
    }

    fn refresh_sessions(&mut self) {
        match self.catalog.list() {
            Ok(sessions) => {
                self.sessions = sessions;
                self.warning_acknowledged |= self.sessions.iter().any(|session| {
                    session
                        .display
                        .get(WARNING_ACK_KEY)
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                });
                if let Some(session_id) = self.session_id.as_ref() {
                    self.view_aliases
                        .insert(session_id.clone(), self.state_key.clone());
                }
                self.rebuild_search_entries();
            }
            Err(error) => self.status = format!("Could not read session catalog: {error}"),
        }
    }

    fn rebuild_search_entries(&mut self) {
        let mut entries = Vec::new();
        for session in &self.sessions {
            let title = session
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty());
            if let Some(title) = title {
                entries.push(SearchEntry {
                    session_id: session.id.clone(),
                    turn_index: None,
                    label: format!("Title · {title}"),
                    text: sanitize::terminal_safe_text(title),
                });
            }
            for (index, turn) in session.turns.iter().enumerate() {
                let transcript = TranscriptTurn::from_trail(turn);
                entries.push(SearchEntry {
                    session_id: session.id.clone(),
                    turn_index: Some(index),
                    label: format!(
                        "{} · turn {}",
                        title.unwrap_or("Conversation"),
                        turn.turn_index + 1
                    ),
                    text: transcript.searchable_text(),
                });
            }
        }
        self.search_entries = entries;
    }

    fn update_search(&mut self) {
        let query = self.search_query.trim().to_lowercase();
        self.search_hits.clear();
        if query.is_empty() {
            self.search_index = 0;
            return;
        }
        self.search_hits = self
            .search_entries
            .iter()
            .filter(|entry| entry.text.to_lowercase().contains(&query))
            .map(|entry| SearchHit {
                session_id: entry.session_id.clone(),
                turn_index: entry.turn_index,
                label: entry.label.clone(),
                excerpt: search_excerpt(&entry.text, &query),
            })
            .collect();
        self.search_index = self
            .search_index
            .min(self.search_hits.len().saturating_sub(1));
    }

    fn open_session(&mut self, session_id: &str, turn_index: Option<usize>) -> bool {
        let Some(session) = self
            .sessions
            .iter()
            .find(|entry| entry.id == session_id)
            .cloned()
        else {
            match self.catalog.get(session_id) {
                Ok(session) => self.sessions.push(session),
                Err(error) => {
                    self.status = format!("Could not reopen session: {error}");
                    return false;
                }
            }
            self.rebuild_search_entries();
            return self.open_session(session_id, turn_index);
        };
        let target_key = self
            .view_aliases
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.to_string());
        if target_key == self.state_key {
            if let Some(index) = turn_index {
                self.inspector_turn = index.min(self.transcript.len().saturating_sub(1));
                self.transcript_follow_tail = false;
                self.transcript_scroll = self.inspector_turn;
                self.inspector_open = true;
                self.inspector_field = 0;
                self.inspector_scroll = 0;
                self.inspector_scroll_step = 1;
            }
            self.screen = Screen::Conversation;
            return true;
        }
        if let Err(error) = self.persist_draft() {
            self.status = format!("Could not save the current draft: {error}");
            self.screen = Screen::Conversation;
            return false;
        }
        let incoming = if let Some(saved) = self.views.remove(&target_key) {
            saved
        } else {
            ConversationState::from_catalog(&session)
        };
        self.view_aliases
            .insert(session.id.clone(), incoming.state_key.clone());
        let outgoing = std::mem::replace(&mut self.view, incoming);
        self.views.insert(outgoing.state_key.clone(), outgoing);
        self.screen = Screen::Conversation;
        self.show_help = false;
        self.show_menu = false;
        self.inspector_open = turn_index.is_some();
        if let Some(index) = turn_index {
            self.inspector_turn = index.min(self.transcript.len().saturating_sub(1));
            self.transcript_follow_tail = false;
            self.transcript_scroll = self.inspector_turn;
        }
        self.inspector_field = 0;
        self.inspector_scroll = 0;
        self.inspector_scroll_step = 1;
        self.status = if self.turn_status.is_active() {
            "This session's turn is still running in the background; its stream continues here."
                .to_string()
        } else {
            session_action_status(&session)
        };
        true
    }

    fn create_session(&mut self) {
        let Some(workspace) = self.workspace.clone() else {
            self.workspace_input.clear();
            self.workspace_flow = WorkspaceFlow::NewSession;
            self.screen = Screen::Workspace;
            self.status = "Choose a workspace for the new session".to_string();
            return;
        };
        if let Err(error) = self.persist_draft() {
            self.status = format!("Could not save the current draft: {error}");
            self.screen = Screen::Conversation;
            return;
        }
        match self
            .catalog
            .create_draft(SessionInterface::Tui, None, Some(&workspace))
        {
            Ok(session) => self.activate_new_session(session),
            Err(error) => self.status = format!("Could not create a session: {error}"),
        }
    }

    fn activate_new_session(&mut self, session: CatalogSession) {
        let mut incoming = ConversationState::from_catalog(&session);
        incoming.state_key = session.id.clone();
        let outgoing = std::mem::replace(&mut self.view, incoming);
        if let Some(session_id) = outgoing.session_id.as_ref() {
            self.view_aliases
                .insert(session_id.clone(), outgoing.state_key.clone());
        }
        self.views.insert(outgoing.state_key.clone(), outgoing);
        self.workspace_flow = WorkspaceFlow::Initial;
        self.screen = if self.warning_acknowledged {
            Screen::Conversation
        } else {
            Screen::Warning
        };
        self.inspector_open = false;
        self.refresh_sessions();
        self.status = if self.warning_acknowledged {
            "New session draft. Rename it with F3, then type a prompt.".to_string()
        } else {
            "Review and acknowledge the first-run warning".to_string()
        };
    }

    fn visible_session_indices(&self) -> Vec<usize> {
        let filter = self.session_filter.trim().to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                filter.is_empty()
                    || session
                        .name
                        .as_deref()
                        .unwrap_or(&session.id)
                        .to_lowercase()
                        .contains(&filter)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_session(&self) -> Option<&CatalogSession> {
        let visible = self.visible_session_indices();
        visible
            .get(self.picker_index)
            .and_then(|index| self.sessions.get(*index))
    }

    fn begin_rename_selected(&mut self) {
        let Some((id, name)) = self
            .selected_session()
            .map(|session| (session.id.clone(), session.name.clone()))
        else {
            self.status = "Choose a session to rename".to_string();
            return;
        };
        self.dialog_target = Some(id);
        self.dialog_input = name.unwrap_or_default();
        self.screen = Screen::Rename;
    }

    fn begin_workspace_replacement(&mut self, session_id: &str) {
        if !self.open_session(session_id, None) {
            return;
        }
        if let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        {
            self.workspace_input = session
                .cwd
                .as_deref()
                .map(Path::display)
                .map(|path| path.to_string())
                .unwrap_or_default();
        } else {
            self.workspace_input.clear();
        }
        self.workspace_flow = WorkspaceFlow::ReplaceSession(session_id.to_string());
        self.screen = Screen::Workspace;
        self.status = "Choose an existing replacement workspace, then press Enter".to_string();
    }

    fn begin_session_picker(&mut self) {
        self.refresh_sessions();
        self.session_filter.clear();
        self.picker_searching = false;
        self.picker_index = self
            .sessions
            .iter()
            .position(|session| {
                session.id == self.session_id.as_deref().unwrap_or_default()
                    || session.id == self.draft_id.as_deref().unwrap_or_default()
            })
            .unwrap_or(0);
        self.screen = Screen::SessionPicker;
    }

    fn begin_search(&mut self) {
        self.refresh_sessions();
        self.search_query.clear();
        self.search_hits.clear();
        self.search_index = 0;
        self.screen = Screen::Search;
    }

    fn begin_export(&mut self) {
        if self.transcript.is_empty() {
            self.status = "There is no transcript to export".to_string();
            return;
        }
        self.dialog_input = "leg-transcript.json".to_string();
        self.overwrite_export = None;
        self.screen = Screen::Export;
    }

    fn selected_detail_field(&self) -> Option<(String, String)> {
        let turn = self.transcript.get(self.inspector_turn)?;
        let fields = turn.detail_fields();
        fields.get(self.inspector_field).cloned()
    }

    fn copy_selected_detail(&mut self) {
        if !self.inspector_open {
            self.status =
                "Open the per-turn inspector with F4, then select a field to copy".to_string();
            return;
        }
        let Some((label, text)) = self.selected_detail_field() else {
            self.status = "Open the inspector and select a transcript field first".to_string();
            return;
        };
        self.copy_text = Some(text.clone());
        if text.len() > 100_000 || std::env::var("TERM").as_deref() == Ok("dumb") {
            self.begin_copy_fallback(&label);
            return;
        }
        let sequence = osc52_sequence(&text);
        let result = io::stdout()
            .write_all(sequence.as_bytes())
            .and_then(|()| io::stdout().flush());
        match result {
            Ok(()) => {
                self.status = format!(
                    "Clipboard request sent for {label}. If blocked by the terminal, press F7 to save it to a file."
                );
            }
            Err(error) => {
                self.status =
                    format!("Clipboard failed: {error}; press F7 to save the selected text");
                self.begin_copy_fallback(&label);
            }
        }
    }

    fn begin_copy_fallback(&mut self, label: &str) {
        self.dialog_input = "leg-copy.txt".to_string();
        self.overwrite_export = None;
        self.screen = Screen::CopyFallback;
        self.status = format!("Save selected text from {label} to a file");
    }

    fn export_text(&self) -> String {
        let turns = self
            .transcript
            .iter()
            .enumerate()
            .map(|(index, turn)| turn.export_value(index as u64))
            .collect::<Vec<_>>();
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "leg-tui.transcript/v1",
            "turns": turns,
        }))
        .unwrap_or_else(|_| "{\"schema\":\"leg-tui.transcript/v1\",\"turns\":[]}".to_string())
    }

    fn handle_session_picker_key(&mut self, key: KeyEvent) {
        if self.picker_searching {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
                self.session_filter.clear();
                self.picker_index = 0;
                return;
            }
            match key.code {
                KeyCode::Esc => {
                    self.picker_searching = false;
                    self.session_filter.clear();
                    self.picker_index = 0;
                }
                KeyCode::Enter => self.picker_searching = false,
                KeyCode::Backspace => {
                    self.session_filter.pop();
                    self.picker_index = 0;
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !character.is_control() =>
                {
                    self.session_filter.push(character);
                    self.picker_index = 0;
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::F(3) => self.screen = Screen::Conversation,
            KeyCode::Up => self.picker_index = self.picker_index.saturating_sub(1),
            KeyCode::Down => {
                self.picker_index = (self.picker_index + 1)
                    .min(self.visible_session_indices().len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(session_id) = self.selected_session().map(|session| session.id.clone())
                {
                    self.open_session(&session_id, None);
                }
            }
            KeyCode::Char('/') => self.picker_searching = true,
            KeyCode::Char('n' | 'N') => self.create_session(),
            KeyCode::Char('r' | 'R') => self.begin_rename_selected(),
            KeyCode::Char('w' | 'W') => {
                if let Some(session_id) = self.selected_session().map(|session| session.id.clone())
                {
                    self.begin_workspace_replacement(&session_id);
                }
            }
            KeyCode::Char('s' | 'S') => self.begin_search(),
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_query.clear();
                self.search_hits.clear();
                self.screen = Screen::Conversation;
            }
            KeyCode::Up => self.search_index = self.search_index.saturating_sub(1),
            KeyCode::Down => {
                self.search_index =
                    (self.search_index + 1).min(self.search_hits.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(hit) = self.search_hits.get(self.search_index) {
                    let session_id = hit.session_id.clone();
                    let turn_index = hit.turn_index;
                    self.open_session(&session_id, turn_index);
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.update_search();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_query.clear();
                self.update_search();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.search_query.push(character);
                self.update_search();
            }
            _ => {}
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.dialog_input.clear();
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.dialog_target = None;
                self.screen = Screen::SessionPicker;
            }
            KeyCode::Enter => {
                let Some(session_id) = self.dialog_target.take() else {
                    self.screen = Screen::SessionPicker;
                    return;
                };
                let name = (!self.dialog_input.trim().is_empty())
                    .then(|| self.dialog_input.trim().to_string());
                match self.catalog.rename(&session_id, name) {
                    Ok(()) => {
                        self.refresh_sessions();
                        self.status = "Session renamed".to_string();
                        self.screen = Screen::SessionPicker;
                    }
                    Err(error) => self.status = format!("Could not rename session: {error}"),
                }
            }
            KeyCode::Backspace => {
                self.dialog_input.pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.dialog_input.push(character);
            }
            _ => {}
        }
    }

    fn handle_export_key(&mut self, key: KeyEvent) {
        if let Some(path) = self.overwrite_export.clone() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.overwrite_export = None;
                    self.finish_file_write(path, self.export_text(), true);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.overwrite_export = None;
                    self.screen = Screen::Conversation;
                    self.status = "Export cancelled".to_string();
                }
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.dialog_input.clear();
            return;
        }
        match key.code {
            KeyCode::Esc => self.screen = Screen::Conversation,
            KeyCode::Enter => {
                let path = self.resolve_output_path();
                if path.exists() {
                    self.overwrite_export = Some(path.clone());
                    self.status = format!(
                        "{} exists. Replace it? Press Y to confirm, N to cancel.",
                        path.display()
                    );
                } else {
                    self.finish_file_write(path, self.export_text(), false);
                }
            }
            KeyCode::Backspace => {
                self.dialog_input.pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.dialog_input.push(character);
            }
            _ => {}
        }
    }

    fn handle_copy_fallback_key(&mut self, key: KeyEvent) {
        if let Some(path) = self.overwrite_export.clone() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.overwrite_export = None;
                    self.finish_file_write(path, self.copy_text.clone().unwrap_or_default(), true);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.overwrite_export = None;
                    self.screen = Screen::Conversation;
                    self.status = "Copy fallback cancelled".to_string();
                }
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.dialog_input.clear();
            return;
        }
        match key.code {
            KeyCode::Esc => self.screen = Screen::Conversation,
            KeyCode::Enter => {
                let path = self.resolve_output_path();
                if path.exists() {
                    self.overwrite_export = Some(path.clone());
                    self.status = format!(
                        "{} exists. Replace it? Press Y to confirm, N to cancel.",
                        path.display()
                    );
                } else {
                    self.finish_file_write(path, self.copy_text.clone().unwrap_or_default(), false);
                }
            }
            KeyCode::Backspace => {
                self.dialog_input.pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.dialog_input.push(character);
            }
            _ => {}
        }
    }

    fn resolve_output_path(&self) -> PathBuf {
        let path = PathBuf::from(self.dialog_input.trim());
        if path.is_absolute() {
            path
        } else {
            self.workspace
                .as_ref()
                .map(|workspace| workspace.join(&path))
                .unwrap_or(path)
        }
    }

    fn finish_file_write(&mut self, path: PathBuf, content: String, replace: bool) {
        match write_export_file(&path, content.as_bytes(), replace) {
            Ok(()) => {
                self.status = format!("Saved {}", path.display());
                self.screen = Screen::Conversation;
            }
            Err(error) => {
                self.status = format!("Could not save {}: {error}", path.display());
                self.overwrite_export = None;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.show_help = false;
            self.show_menu = false;
            self.handle_ctrl_c();
            return;
        }

        match self.screen {
            Screen::Workspace => self.handle_workspace_key(key),
            Screen::Warning => self.handle_warning_key(key),
            Screen::Conversation => self.handle_conversation_key(key),
            Screen::SessionPicker => self.handle_session_picker_key(key),
            Screen::Search => self.handle_search_key(key),
            Screen::Rename => self.handle_rename_key(key),
            Screen::Export => self.handle_export_key(key),
            Screen::CopyFallback => self.handle_copy_fallback_key(key),
        }
    }

    fn handle_workspace_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.select_workspace(),
            KeyCode::Backspace => {
                self.workspace_input.pop();
                self.status = "Choose an existing directory".to_string();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.workspace_input.push(character);
                self.status = "Choose an existing directory".to_string();
            }
            KeyCode::Esc => self.status = "Choose an existing directory".to_string(),
            _ => {}
        }
    }

    fn select_workspace(&mut self) {
        let candidate = PathBuf::from(self.workspace_input.trim());
        if !candidate.is_dir() {
            self.status = format!(
                "Workspace is not an existing directory: {}",
                candidate.display()
            );
            return;
        }
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) => {
                self.status = format!("Could not open workspace: {error}");
                return;
            }
        };
        let flow = self.workspace_flow.clone();
        if matches!(&flow, WorkspaceFlow::NewSession)
            && let Err(error) = self.persist_draft()
        {
            self.status = format!("Could not save the current draft: {error}");
            return;
        }
        let result = match &flow {
            WorkspaceFlow::ReplaceSession(session_id) => self
                .catalog
                .set_workspace(session_id, canonical.as_path())
                .and_then(|()| self.catalog.get(session_id)),
            WorkspaceFlow::Initial | WorkspaceFlow::NewSession => {
                self.catalog
                    .create_draft(SessionInterface::Tui, None, Some(canonical.as_path()))
            }
        };
        match result {
            Ok(session) => match flow {
                WorkspaceFlow::Initial => {
                    self.workspace = session.cwd.clone();
                    self.draft_id = Some(session.id.clone());
                    self.session_id = None;
                    self.state_key = session.id;
                    self.workspace_flow = WorkspaceFlow::Initial;
                    self.screen = Screen::Warning;
                    self.status = "Review and acknowledge the first-run warning".to_string();
                    self.refresh_sessions();
                }
                WorkspaceFlow::NewSession => self.activate_new_session(session),
                WorkspaceFlow::ReplaceSession(expected_id) => {
                    debug_assert_eq!(session.id, expected_id);
                    self.workspace = session.cwd.clone();
                    self.draft_id = Some(session.id.clone());
                    self.session_id =
                        (!session.id.starts_with("draft-")).then(|| session.id.clone());
                    self.read_only = session.read_only;
                    self.recovered = session.recovered;
                    self.view_aliases
                        .insert(session.id.clone(), self.state_key.clone());
                    self.workspace_flow = if self.warning_acknowledged {
                        WorkspaceFlow::Initial
                    } else {
                        WorkspaceFlow::ReplaceSession(expected_id)
                    };
                    self.screen = if self.warning_acknowledged {
                        Screen::Conversation
                    } else {
                        Screen::Warning
                    };
                    self.status = if self.warning_acknowledged {
                        "Replacement workspace selected".to_string()
                    } else {
                        "Review and acknowledge the first-run warning".to_string()
                    };
                    self.refresh_sessions();
                }
            },
            Err(error) => self.status = format!("Could not create a session draft: {error}"),
        }
    }

    fn handle_warning_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.acknowledge_warning(),
            KeyCode::Esc => {
                self.screen = Screen::Workspace;
                self.status = "Choose a workspace".to_string();
            }
            _ => {}
        }
    }

    fn acknowledge_warning(&mut self) {
        self.warning_acknowledged = true;
        self.workspace_flow = WorkspaceFlow::Initial;
        self.screen = Screen::Conversation;
        self.status = "Ready".to_string();
        let record_id = self
            .session_id
            .as_deref()
            .or(self.draft_id.as_deref())
            .map(str::to_owned);
        let Some(record_id) = record_id else {
            return;
        };
        match self.catalog.save_display_metadata(
            &record_id,
            WARNING_ACK_KEY.to_string(),
            serde_json::Value::Bool(true),
        ) {
            Ok(()) => self.refresh_sessions(),
            Err(error) => {
                self.status =
                    format!("Warning acknowledged for this run but could not be saved: {error}");
            }
        }
    }

    fn handle_conversation_key(&mut self, key: KeyEvent) {
        if self.retry_confirmation.is_some() {
            self.handle_retry_confirmation_key(key);
            return;
        }
        if self.show_help {
            if key.code == KeyCode::Esc {
                self.show_help = false;
                self.status = "Composer focused".to_string();
            }
            return;
        }
        if self.show_menu {
            if key.code == KeyCode::Esc {
                self.show_menu = false;
                self.status = "Composer focused".to_string();
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.submit();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.begin_search();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
            let changed = self.composer.undo();
            self.mark_active_draft_edited(changed);
            self.status = "Ready".to_string();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            let changed = self.composer.redo();
            self.mark_active_draft_edited(changed);
            self.status = "Ready".to_string();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::End {
            self.scroll_to_newest();
            return;
        }
        match key.code {
            KeyCode::Esc if self.inspector_open => self.inspector_open = false,
            KeyCode::Esc => self.status = "Composer focused".to_string(),
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.show_menu = true,
            KeyCode::F(3) => self.begin_session_picker(),
            KeyCode::F(4) => {
                self.inspector_open = !self.inspector_open;
                self.inspector_turn = self.transcript.len().saturating_sub(1);
                self.inspector_field = 0;
                self.inspector_scroll = 0;
                self.inspector_scroll_step = 1;
            }
            KeyCode::F(5) => self.copy_selected_detail(),
            KeyCode::F(6) => self.begin_export(),
            KeyCode::F(7) => {
                if self.copy_text.is_some() {
                    self.begin_copy_fallback("selected transcript text");
                } else {
                    self.status = "Copy a selected inspector field with F5 first".to_string();
                }
            }
            KeyCode::Up if self.inspector_open => {
                self.inspector_field = self.inspector_field.saturating_sub(1);
                self.inspector_scroll = 0;
                self.inspector_scroll_step = 1;
            }
            KeyCode::Down if self.inspector_open => {
                let fields = self
                    .transcript
                    .get(self.inspector_turn)
                    .map(TranscriptTurn::detail_fields)
                    .unwrap_or_default();
                self.inspector_field =
                    (self.inspector_field + 1).min(fields.len().saturating_sub(1));
                self.inspector_scroll = 0;
                self.inspector_scroll_step = 1;
            }
            KeyCode::PageUp
                if self.inspector_open && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.inspector_scroll = self
                    .inspector_scroll
                    .saturating_sub(self.inspector_scroll_step);
            }
            KeyCode::PageDown
                if self.inspector_open && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some((_, value)) = self.selected_detail_field() {
                    self.inspector_scroll = next_inspector_scroll(
                        self.inspector_scroll,
                        self.inspector_scroll_step,
                        value.len(),
                    );
                }
            }
            KeyCode::Char('[') if self.inspector_open => {
                self.inspector_turn = self.inspector_turn.saturating_sub(1);
                self.inspector_field = 0;
                self.inspector_scroll = 0;
                self.inspector_scroll_step = 1;
            }
            KeyCode::Char(']') if self.inspector_open => {
                self.inspector_turn =
                    (self.inspector_turn + 1).min(self.transcript.len().saturating_sub(1));
                self.inspector_field = 0;
                self.inspector_scroll = 0;
                self.inspector_scroll_step = 1;
            }
            KeyCode::Char('r' | 'R')
                if self.inspector_open && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.retry_selected_turn();
            }
            KeyCode::Char('w' | 'W')
                if self.workspace.is_none()
                    || !self.workspace.as_ref().is_some_and(|path| path.is_dir()) =>
            {
                let id = self.session_id.clone().or_else(|| self.draft_id.clone());
                if let Some(id) = id {
                    self.begin_workspace_replacement(&id);
                } else {
                    self.screen = Screen::Workspace;
                }
            }
            KeyCode::PageUp => self.scroll_transcript_up(),
            KeyCode::PageDown => self.scroll_transcript_down(),
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            KeyCode::Home => self.composer.move_home(),
            KeyCode::End => self.composer.move_end(),
            KeyCode::Backspace => {
                let changed = self.composer.backspace();
                self.mark_active_draft_edited(changed);
                self.status = "Ready".to_string();
            }
            KeyCode::Delete => {
                let changed = self.composer.delete();
                self.mark_active_draft_edited(changed);
                self.status = "Ready".to_string();
            }
            KeyCode::Enter => {
                let changed = self.composer.insert("\n");
                self.mark_active_draft_edited(changed);
                self.status = "Ready".to_string();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                let changed = self.composer.insert(&character.to_string());
                self.mark_active_draft_edited(changed);
                self.status = "Ready".to_string();
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, pasted: &str) {
        if self.screen == Screen::Search {
            self.search_query.push_str(&normalize_paste(pasted));
            self.update_search();
            return;
        }
        if self.screen == Screen::SessionPicker && self.picker_searching {
            self.session_filter.push_str(&normalize_paste(pasted));
            self.picker_index = 0;
            return;
        }
        if matches!(
            self.screen,
            Screen::Rename | Screen::Export | Screen::CopyFallback
        ) {
            self.dialog_input.push_str(&normalize_paste(pasted));
            return;
        }
        if self.screen != Screen::Conversation
            || self.show_help
            || self.show_menu
            || self.retry_confirmation.is_some()
        {
            return;
        }
        let text = normalize_paste(pasted);
        let changed = self.composer.insert(&text);
        self.mark_active_draft_edited(changed);
        if changed {
            self.status = "Ready".to_string();
        }
    }

    fn mark_active_draft_edited(&mut self, changed: bool) {
        if self.turn_status.is_active() && changed {
            self.active_draft_edited = true;
        }
    }

    fn handle_ctrl_c(&mut self) {
        if self.turn_status.is_active() {
            if self.turn_status.is_stopping() {
                return;
            }
            match &self.stop_handle {
                Some(handle) => match handle.stop() {
                    Ok(()) => {
                        self.turn_status = TurnStatus::Stopping;
                        self.status.clear();
                    }
                    Err(error) => self.status = format!("Stop failed: {error}"),
                },
                None => self.status = "Active turn has no Stop control".to_string(),
            }
            return;
        }
        if let Some(active) = self
            .views
            .values_mut()
            .find(|view| view.turn_status.is_active())
            && let Some(handle) = &active.stop_handle
        {
            match handle.stop() {
                Ok(()) => {
                    active.turn_status = TurnStatus::Stopping;
                    self.status = "Stopping the active turn in another session".to_string();
                }
                Err(error) => self.status = format!("Stop failed: {error}"),
            }
            return;
        }
        self.quit = true;
    }

    fn set_terminal_size(&mut self, columns: u16, rows: u16) {
        let was_too_small = self.terminal_too_small();
        self.terminal_columns = columns;
        self.terminal_rows = rows;
        if was_too_small && !self.terminal_too_small() && self.status == SMALL_TERMINAL_REJECTION {
            self.status = "Ready".to_string();
        }
    }

    fn terminal_too_small(&self) -> bool {
        self.terminal_columns < MIN_TERMINAL_COLUMNS || self.terminal_rows < MIN_TERMINAL_ROWS
    }

    fn handle_external_signal(&mut self, signal: ExternalSignal) {
        self.exit_after_turn = true;
        if self.turn_status.is_active() {
            let name = match signal {
                ExternalSignal::Interrupt => "SIGINT",
                ExternalSignal::Terminate => "SIGTERM",
            };
            if self.turn_status.is_stopping() {
                self.status = format!("{name} received; waiting for turn cleanup");
            } else {
                match &self.stop_handle {
                    Some(handle) => match handle.stop() {
                        Ok(()) => {
                            self.turn_status = TurnStatus::Stopping;
                            self.status = format!("{name} received; waiting for turn cleanup");
                        }
                        Err(error) => {
                            self.status = format!(
                                "{name} received; stop request failed: {error}; waiting for turn cleanup"
                            );
                        }
                    },
                    None => {
                        self.status = format!("{name} received; waiting for turn cleanup");
                    }
                }
            }
            return;
        }
        self.quit = true;
    }

    fn submit(&mut self) {
        if self.terminal_too_small() {
            self.status = SMALL_TERMINAL_REJECTION.to_string();
            return;
        }
        if self.turn_status.is_active() {
            self.status = "Busy: wait for the active turn".to_string();
            return;
        }
        if self.composer.text().trim().is_empty() {
            self.status = "Blank prompts are not sent".to_string();
            return;
        }
        if self.draft_id.is_none() {
            self.status = "Choose a workspace before sending".to_string();
            return;
        }
        if self.read_only {
            self.status = "This session is read-only. Review its catalog warning or press W to choose a workspace.".to_string();
            return;
        }
        if !self.workspace.as_ref().is_some_and(|path| path.is_dir()) {
            self.status =
                "Recorded workspace is unavailable. Press W to choose a replacement.".to_string();
            return;
        }
        let prompt = self.composer.text().to_string();
        if let Some(session_id) = self.session_id.as_deref()
            && let Ok(intent) = self.catalog.prepare_retry(session_id)
            && intent.prompt() == prompt
        {
            self.retry_confirmation = Some(intent);
            self.status.clear();
            return;
        }
        self.start_prompt(prompt, None);
    }

    fn retry_selected_turn(&mut self) {
        let Some(session_id) = self.session_id.as_deref() else {
            self.status = "Only a saved failed turn can be retried".to_string();
            return;
        };
        let selected_is_latest = self.inspector_turn + 1 == self.transcript.len();
        let retryable = self
            .transcript
            .get(self.inspector_turn)
            .is_some_and(TranscriptTurn::retryable);
        if !selected_is_latest || !retryable {
            self.status =
                "Select the latest failed, interrupted, or incomplete turn to retry".to_string();
            return;
        }
        match self.catalog.prepare_retry(session_id) {
            Ok(intent) => {
                self.retry_confirmation = Some(intent);
                self.status.clear();
            }
            Err(error) => self.status = format!("Retry unavailable: {error}"),
        }
    }

    fn handle_retry_confirmation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y' | 'Y')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(intent) = self.retry_confirmation.take() {
                    let prompt = intent.prompt().to_string();
                    self.start_prompt(prompt, Some(intent));
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.retry_confirmation = None;
                self.status = "Retry cancelled".to_string();
            }
            _ => {}
        }
    }

    fn start_prompt(&mut self, prompt: String, retry: Option<RetryIntent>) {
        if !self.warning_acknowledged {
            self.retry_confirmation = None;
            self.screen = Screen::Warning;
            self.status = "Review and acknowledge the first-run warning".to_string();
            return;
        }
        let Some(draft_id) = self.draft_id.as_deref() else {
            self.status = "Choose a workspace before sending".to_string();
            return;
        };
        let session_id = self.session_id.clone();
        let record_id = session_id.as_deref().unwrap_or(draft_id);
        let is_retry = retry.is_some();
        if retry.is_none()
            && let Err(error) =
                self.catalog
                    .save_draft(record_id, SessionInterface::Tui, prompt.clone())
        {
            self.status = format!("Could not save the prompt draft: {error}");
            return;
        }
        let started = match retry {
            Some(intent) => self.catalog.confirm_retry(&intent, SessionInterface::Tui),
            None => match session_id {
                Some(session_id) => {
                    self.catalog
                        .start_existing(&session_id, SessionInterface::Tui, prompt.clone())
                }
                None => self
                    .catalog
                    .start_new(draft_id, SessionInterface::Tui, prompt.clone()),
            },
        };
        let turn = match started {
            Ok(turn) => turn,
            Err(error) => {
                self.status = format!("Could not start the turn: {error}");
                return;
            }
        };
        let stop_handle = turn.stop_handle();
        let owner = self.state_key.clone();
        match spawn_turn_reader(turn, self.turn_tx.clone(), owner) {
            Ok(()) => {
                self.transcript.push(TranscriptTurn::new(&prompt));
                self.note_transcript_change();
                self.stop_handle = Some(stop_handle);
                self.turn_status = TurnStatus::Starting;
                self.started_at = Some(Instant::now());
                self.last_elapsed = Duration::ZERO;
                self.terminal_detail = None;
                self.active_prompt = Some(prompt);
                self.active_draft_edited = is_retry;
                if !is_retry {
                    self.composer.clear();
                }
                self.status.clear();
                self.active_tool = None;
            }
            Err(error) => {
                let _ = stop_handle.stop();
                self.status = format!("Could not observe the turn: {error}");
            }
        }
    }

    fn note_transcript_change(&mut self) {
        if !self.transcript_follow_tail {
            self.transcript_new_content = true;
        }
    }

    fn scroll_transcript_up(&mut self) {
        if self.transcript_follow_tail {
            self.transcript_follow_tail = false;
            self.transcript_scroll = self
                .transcript_max_scroll
                .saturating_sub(self.transcript_page_rows);
        } else {
            self.transcript_scroll = self
                .transcript_scroll
                .saturating_sub(self.transcript_page_rows);
        }
    }

    fn scroll_transcript_down(&mut self) {
        let next = self
            .transcript_scroll
            .saturating_add(self.transcript_page_rows);
        if next >= self.transcript_max_scroll {
            self.scroll_to_newest();
        } else {
            self.transcript_follow_tail = false;
            self.transcript_scroll = next;
        }
    }

    fn scroll_to_newest(&mut self) {
        self.transcript_follow_tail = true;
        self.transcript_new_content = false;
    }

    fn receive_turn_messages(&mut self) {
        loop {
            match self.turn_rx.try_recv() {
                Ok(TurnMessage::Event { owner, event }) => {
                    self.with_conversation(&owner, |app| app.handle_stream_event(event));
                }
                Ok(TurnMessage::Finished { owner, result }) => {
                    self.with_conversation(&owner, |app| app.finish_turn(result));
                    self.refresh_sessions();
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn with_conversation(&mut self, owner: &str, apply: impl FnOnce(&mut Self)) {
        if self.state_key == owner {
            apply(self);
            return;
        }
        let Some(saved) = self.views.remove(owner) else {
            return;
        };
        let current = std::mem::replace(&mut self.view, saved);
        apply(self);
        let updated = std::mem::replace(&mut self.view, current);
        self.views.insert(owner.to_string(), updated);
    }

    fn handle_stream_event(&mut self, event: StreamEvent) {
        let visible = self
            .transcript
            .last_mut()
            .is_some_and(|turn| turn.observe(&event));
        if visible {
            self.note_transcript_change();
        }
        match event {
            StreamEvent::TurnStart {
                provider,
                model,
                session_id,
                turn_index,
                ..
            } => {
                self.model = format!("{provider}/{model}");
                if let Some(turn_index) = turn_index
                    && let Some(turn) = self.transcript.last_mut()
                {
                    turn.set_turn_index(turn_index);
                }
                if let Some(session_id) = session_id {
                    self.view_aliases
                        .insert(session_id.clone(), self.state_key.clone());
                    self.session_id = Some(session_id);
                    self.draft_id = self.session_id.clone();
                }
                self.external_run_state = "active".to_string();
                self.turn_status.mark_running_unless_stopping();
                self.status.clear();
            }
            StreamEvent::TextDelta { .. } => {
                self.turn_status.mark_running_unless_stopping();
            }
            StreamEvent::ToolCall { tool_name, .. } => {
                let name = sanitize::terminal_safe_text(&tool_name);
                self.active_tool = Some(name.clone());
                self.turn_status.mark_running_unless_stopping();
                if !self.turn_status.is_stopping() {
                    self.status = format!("Tool running: {name}");
                }
            }
            StreamEvent::ToolResult { .. } => {
                self.active_tool = None;
                self.turn_status.mark_running_unless_stopping();
                self.status.clear();
            }
            StreamEvent::ToolRound { .. } => {
                self.turn_status.mark_running_unless_stopping();
            }
            StreamEvent::TurnEnd { response, .. } => {
                if response.get("kind").and_then(serde_json::Value::as_str) == Some("response")
                    && let Some(turn) = self.transcript.last_mut()
                {
                    turn.reconcile_final(&response);
                    self.note_transcript_change();
                }
            }
            StreamEvent::Unknown { .. } => {}
        }
    }

    fn finish_turn(&mut self, result: Result<TurnOutcome, String>) {
        let submitted_prompt = self.active_prompt.take();
        let draft_edited = self.active_draft_edited;
        self.active_draft_edited = false;
        self.stop_handle = None;
        self.active_tool = None;
        self.last_elapsed = self
            .started_at
            .take()
            .map(|started| started.elapsed())
            .unwrap_or(Duration::ZERO);
        if let Some(turn) = self.transcript.last_mut() {
            turn.finish_stream();
        }
        self.external_run_state = "idle".to_string();
        self.status.clear();
        match result {
            Ok(TurnOutcome::Succeeded { response, capped }) => {
                let truncated = response_stop_reason(&response) == Some("max_tokens");
                self.turn_status = successful_status(capped, truncated);
                self.terminal_detail = match (capped, truncated) {
                    (true, true) => Some(
                        "Tool-round limit reached; reply also truncated at max tokens.".to_string(),
                    ),
                    (true, false) => Some("Tool-round limit reached.".to_string()),
                    (false, true) => Some("Reply truncated at max tokens.".to_string()),
                    (false, false) => None,
                };
                if let Some(warning) = self.terminal_detail.clone() {
                    if let Some(turn) = self.transcript.last_mut() {
                        turn.set_completion_warning(&warning);
                    }
                    self.persist_completion_warning(&warning);
                }
                if let Some(turn) = self.transcript.last_mut() {
                    turn.set_outcome("succeeded", None);
                }
            }
            Ok(TurnOutcome::Stopped { .. }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Interrupted;
                self.terminal_detail = Some("Turn interrupted gracefully.".to_string());
                if let Some(turn) = self.transcript.last_mut() {
                    turn.set_outcome("interrupted", None);
                }
            }
            Ok(TurnOutcome::Failed { message, .. }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Failed;
                self.terminal_detail = Some(sanitize::terminal_safe_text(&message));
                if let Some(turn) = self.transcript.last_mut() {
                    turn.set_outcome("failed", Some(&message));
                }
            }
            Ok(TurnOutcome::Incomplete { message, forced }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Incomplete { forced };
                self.terminal_detail = Some(sanitize::terminal_safe_text(&message));
                if let Some(turn) = self.transcript.last_mut() {
                    turn.set_outcome("incomplete", Some(&message));
                }
            }
            Err(message) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Incomplete { forced: false };
                self.terminal_detail = Some(sanitize::terminal_safe_text(&message));
                if let Some(turn) = self.transcript.last_mut() {
                    turn.set_outcome("incomplete", Some(&message));
                }
            }
        }
        let _ = self.persist_draft();
        if self.exit_after_turn {
            self.quit = true;
        }
    }

    fn restore_unedited_prompt(&mut self, prompt: Option<String>, draft_edited: bool) {
        let restored = restored_draft(self.composer.text(), prompt, draft_edited);
        if restored != self.composer.text() {
            self.composer.set_text(restored);
        }
    }

    fn elapsed(&self) -> Duration {
        self.started_at
            .map(|started| started.elapsed())
            .unwrap_or(self.last_elapsed)
    }

    fn persist_draft(&self) -> Result<(), Box<dyn Error>> {
        let record_id = self.session_id.as_deref().or(self.draft_id.as_deref());
        if let Some(record_id) = record_id {
            self.catalog.save_draft(
                record_id,
                SessionInterface::Tui,
                self.composer.text().to_string(),
            )?;
        }
        Ok(())
    }

    fn persist_all_drafts(&self) -> Result<(), Box<dyn Error>> {
        self.persist_draft()?;
        for view in self.views.values() {
            let record_id = view.session_id.as_deref().or(view.draft_id.as_deref());
            if let Some(record_id) = record_id {
                self.catalog.save_draft(
                    record_id,
                    SessionInterface::Tui,
                    view.composer.text().to_string(),
                )?;
            }
        }
        Ok(())
    }

    fn persist_completion_warning(&self, warning: &str) {
        let Some(session_id) = self.session_id.as_deref() else {
            return;
        };
        let Some(turn_index) = self.transcript.last().and_then(TranscriptTurn::turn_index) else {
            return;
        };
        let mut warnings = self
            .catalog
            .get(session_id)
            .ok()
            .and_then(|session| {
                session
                    .display
                    .get("tui_completion_warnings")
                    .and_then(serde_json::Value::as_object)
                    .cloned()
            })
            .unwrap_or_default();
        warnings.insert(
            turn_index.to_string(),
            serde_json::Value::String(warning.to_string()),
        );
        let _ = self.catalog.save_display_metadata(
            session_id,
            "tui_completion_warnings".to_string(),
            serde_json::Value::Object(warnings),
        );
    }
}

fn spawn_turn_reader(
    mut turn: CatalogTurn,
    tx: Sender<TurnMessage>,
    owner: String,
) -> io::Result<()> {
    thread::Builder::new()
        .name("leg-tui-turn-reader".to_string())
        .spawn(move || {
            let result = loop {
                match turn.observe() {
                    Ok(Some(event)) => {
                        if tx
                            .send(TurnMessage::Event {
                                owner: owner.clone(),
                                event,
                            })
                            .is_err()
                        {
                            let _ = turn.stop();
                            let _ = turn.wait();
                            return;
                        }
                    }
                    Ok(None) => break turn.wait().map_err(|error| error.to_string()),
                    Err(error) => {
                        let _ = turn.stop();
                        break Err(error.to_string());
                    }
                }
            };
            let _ = tx.send(TurnMessage::Finished { owner, result });
        })
        .map(|_| ())
}

fn response_stop_reason(response: &serde_json::Value) -> Option<&str> {
    response
        .pointer("/exchange/exchange/outcome/stop_reason")
        .and_then(serde_json::Value::as_str)
}

fn successful_status(capped: bool, truncated: bool) -> TurnStatus {
    if capped {
        TurnStatus::Capped
    } else if truncated {
        TurnStatus::Truncated
    } else {
        TurnStatus::Succeeded
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

fn wrap_transcript(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for logical_line in text.split('\n') {
        let mut row = String::new();
        let mut row_width = 0_usize;
        for grapheme in UnicodeSegmentation::graphemes(logical_line, true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push_str(grapheme);
            row_width = row_width.saturating_add(grapheme_width);
        }
        rows.push(row);
    }
    rows.into_iter().map(Line::from).collect()
}

fn byte_offset_after_wrapped_rows(text: &str, start: usize, width: usize, rows: usize) -> usize {
    let start = char_boundary_at_or_before(text, start);
    if rows == 0 || start >= text.len() {
        return start;
    }

    let width = width.max(1);
    let mut rows_used = 1;
    let mut row_width = 0_usize;
    for (relative_start, grapheme) in UnicodeSegmentation::grapheme_indices(&text[start..], true) {
        let absolute_start = start + relative_start;
        let grapheme_end = absolute_start + grapheme.len();
        if grapheme == "\n" {
            if rows_used >= rows {
                return grapheme_end;
            }
            rows_used += 1;
            row_width = 0;
            continue;
        }

        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
            if rows_used >= rows {
                return absolute_start;
            }
            rows_used += 1;
            row_width = 0;
        }
        row_width = row_width.saturating_add(grapheme_width);
    }
    text.len()
}

fn restored_draft(current: &str, submitted: Option<String>, draft_edited: bool) -> String {
    if draft_edited {
        current.to_string()
    } else {
        submitted.unwrap_or_else(|| current.to_string())
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    if app.terminal_too_small() {
        draw_small_terminal(frame, app);
        return;
    }
    match app.screen {
        Screen::Workspace => draw_workspace(frame, app),
        Screen::Warning => draw_warning(frame, app.use_color),
        Screen::Conversation => draw_conversation(frame, app),
        Screen::SessionPicker => draw_session_picker(frame, app),
        Screen::Search => draw_search(frame, app),
        Screen::Rename => draw_text_dialog(frame, app, "Rename session", "New title:"),
        Screen::Export => draw_text_dialog(frame, app, "Export transcript", "Save transcript to:"),
        Screen::CopyFallback => {
            draw_text_dialog(frame, app, "Save selected text", "Save copied text to:")
        }
    }
}

fn draw_workspace(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(88, 42, frame.area());
    let lines = vec![
        Line::from("Select an existing workspace directory before starting a turn."),
        Line::from("Enter a path and press Enter to select it."),
        Line::from(""),
        Line::from(format!("Path: {}", app.workspace_input)),
        Line::from(""),
        Line::from(sanitize::terminal_safe_text(&app.status)),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title("Workspace"))
            .wrap(Wrap { trim: false }),
        area,
    );
    let x = area
        .x
        .saturating_add(8 + app.workspace_input.chars().count() as u16);
    let y = area.y.saturating_add(4);
    frame.set_cursor_position((
        x.min(area.right().saturating_sub(2)),
        y.min(area.bottom().saturating_sub(2)),
    ));
}

fn draw_warning(frame: &mut Frame<'_>, use_color: bool) {
    let terminal_area = frame.area();
    let area = if terminal_area.height < 32 {
        centered_rect(96, 92, terminal_area)
    } else {
        centered_rect(96, 46, terminal_area)
    };
    let lines = vec![
        warning_line(use_color),
        Line::from(""),
        Line::from("Press Enter to acknowledge and open the composer."),
        Line::from("Press Esc to choose another workspace."),
        Line::from(""),
        Line::from(
            "Composer: Enter newline; Ctrl-S send; arrows, Home/End, Backspace/Delete edit.",
        ),
        Line::from("Ctrl-Z undo; Ctrl-Y redo. Ctrl-C stops a turn or exits when idle."),
        Line::from(
            "F1 help; F2 actions; F3 sessions; Ctrl-F search; F4 inspect; F5 copy; F6 export.",
        ),
        Line::from("PageUp/PageDown scroll history; Ctrl-End newest text."),
        Line::from(
            "Bracketed paste preserves Unicode and lines; other control characters are removed.",
        ),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Before your first turn"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn warning_line(use_color: bool) -> Line<'static> {
    let line = Line::from(WARNING);
    if use_color {
        line.style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        line
    }
}

fn draw_small_terminal(frame: &mut Frame<'_>, app: &App) {
    let mut lines = vec![
        Line::from("Terminal too small"),
        Line::from("Minimum supported size: 80 columns x 24 rows."),
        Line::from("Resize to continue. Draft and active-turn state are preserved."),
    ];
    if app.status == SMALL_TERMINAL_REJECTION {
        lines.push(Line::from(app.status.clone()));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Resize terminal"),
            )
            .wrap(Wrap { trim: false }),
        frame.area(),
    );
}

fn draw_session_picker(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(94, 90, frame.area());
    let visible = app.visible_session_indices();
    let height = area.height.saturating_sub(8) as usize;
    let page_rows = (height / 2).max(1);
    let start = app
        .picker_index
        .saturating_sub(page_rows.saturating_sub(1))
        .min(visible.len().saturating_sub(page_rows));
    let end = (start + page_rows).min(visible.len());
    let mut lines = vec![Line::from(format!(
        "Filter: {}{}",
        app.session_filter,
        if app.picker_searching { "▏" } else { "" }
    ))];
    if visible.is_empty() {
        lines.push(Line::from(if app.sessions.is_empty() {
            "No sessions yet. Press N to create one."
        } else {
            "No titles match. Press / to edit the filter or Esc to return."
        }));
    }
    for (position, index) in visible.iter().enumerate().take(end).skip(start) {
        let session = &app.sessions[*index];
        let title = session
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("Untitled conversation");
        let active_id = app.session_id.as_deref().or(app.draft_id.as_deref());
        let selected = position == app.picker_index;
        let current = active_id == Some(session.id.as_str());
        let marker = if selected {
            ">"
        } else if current {
            "*"
        } else {
            " "
        };
        let first = Line::from(format!(
            "{marker} {}  [{}]  {}",
            sanitize::terminal_safe_text(title),
            session_state_label(session),
            relative_time(session.updated_at_ms),
        ));
        let workspace = session
            .cwd
            .as_deref()
            .map(Path::display)
            .map(|path| path.to_string())
            .unwrap_or_else(|| "(workspace not set)".to_string());
        let second = Line::from(format!(
            "    {}  ·  {} turns  ·  {}",
            sanitize::terminal_safe_text(&workspace),
            session.turns.len(),
            sanitize::terminal_safe_text(&session.id),
        ));
        lines.push(if selected && app.use_color {
            first.style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            first
        });
        lines.push(second);
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "/ filter titles  ·  ↑/↓ select  ·  Enter reopen  ·  N new  ·  R rename  ·  W workspace",
    ));
    lines.push(Line::from(
        "S search transcript  ·  F3/Esc back  ·  browse is read-only and never sends",
    ));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Sessions · title · workspace · recent · status"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_search(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(90, 82, frame.area());
    let mut lines = vec![Line::from(format!("Search: {}▏", app.search_query))];
    if app.search_query.is_empty() {
        lines.push(Line::from(
            "Search session titles and displayed transcript text.",
        ));
        lines.push(Line::from(
            "Type to search; Ctrl-U clears; Up/Down select; Enter opens; Esc closes.",
        ));
    } else if app.search_hits.is_empty() {
        lines.push(Line::from(
            "No matches. Backspace edits the query; Ctrl-U clears it.",
        ));
    } else {
        lines.push(Line::from(format!(
            "Match {} of {} · Up/Down select · Enter opens · Ctrl-U clears · Esc closes",
            app.search_index + 1,
            app.search_hits.len()
        )));
        lines.push(Line::from(""));
        let page_size = area.height.saturating_sub(8) as usize;
        let start = app.search_index.saturating_sub(page_size.saturating_sub(1));
        for (index, hit) in app
            .search_hits
            .iter()
            .enumerate()
            .skip(start)
            .take(page_size)
        {
            let prefix = if index == app.search_index {
                "> "
            } else {
                "  "
            };
            lines.push(Line::from(format!(
                "{prefix}{}",
                sanitize::terminal_safe_text(&hit.label)
            )));
            lines.push(Line::from(format!(
                "    {}",
                sanitize::terminal_safe_text(&hit.excerpt)
            )));
        }
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Search history"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_text_dialog(frame: &mut Frame<'_>, app: &App, title: &str, prompt: &str) {
    let area = centered_rect(82, 40, frame.area());
    let confirming = app.overwrite_export.as_ref();
    let lines = if let Some(path) = confirming {
        vec![
            Line::from(sanitize::terminal_safe_text(&path.display().to_string())),
            Line::from("This file already exists."),
            Line::from("Press Y to replace it after confirmation, N or Esc to cancel."),
        ]
    } else {
        vec![
            Line::from(prompt),
            Line::from(format!(
                "{}▏",
                sanitize::terminal_safe_text(&app.dialog_input)
            )),
            Line::from("Enter confirms · Esc cancels"),
            Line::from(sanitize::terminal_safe_text(&app.status)),
        ]
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_conversation(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let active_id = app.session_id.as_deref().or(app.draft_id.as_deref());
    let active = active_id.and_then(|id| app.sessions.iter().find(|session| session.id == id));
    let title = active
        .and_then(|session| session.name.as_deref())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Untitled conversation");
    let workspace = app
        .workspace
        .as_deref()
        .map(Path::display)
        .map(|path| path.to_string())
        .unwrap_or_else(|| "(workspace not set)".to_string());
    let activity = app
        .active_tool
        .as_deref()
        .map(|tool| format!("  |  tool: {tool}"))
        .unwrap_or_default();
    let elapsed = format_duration(app.elapsed());
    let background_turns = app
        .views
        .values()
        .filter(|view| view.turn_status.is_active())
        .count();
    let background = if background_turns > 0 {
        format!("{background_turns} turn(s) active in other session(s)")
    } else {
        String::new()
    };
    let catalog_state = active
        .map(|session| session_state_label(session).to_string())
        .unwrap_or_else(|| app.external_run_state.clone());
    let detail = app
        .terminal_detail
        .as_deref()
        .or_else(|| (!app.status.is_empty()).then_some(app.status.as_str()))
        .unwrap_or_default();
    let detail = if background.is_empty() {
        detail.to_string()
    } else if detail.is_empty() {
        background
    } else {
        format!("{background} · {detail}")
    };
    let header = format!(
        "leg-tui  |  status: {}  |  {}  |  model: {}  |  catalog: {}  |  elapsed: {elapsed}{activity}\nworkspace: {workspace}\n{detail}",
        app.turn_status.display(),
        sanitize::terminal_safe_text(title),
        sanitize::terminal_safe_text(&app.model),
        catalog_state,
    );
    let header = Paragraph::new(sanitize::terminal_safe_text(&header))
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(
        if app.use_color {
            header.style(Style::default().fg(Color::Cyan))
        } else {
            header
        },
        chunks[0],
    );

    let body_constraints = if app.inspector_open {
        vec![
            Constraint::Length(25),
            Constraint::Min(24),
            Constraint::Length(40),
        ]
    } else {
        vec![Constraint::Length(25), Constraint::Min(1)]
    };
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(body_constraints)
        .split(chunks[1]);
    let mut rail_lines = vec![
        Line::from("F3 sessions"),
        Line::from(sanitize::terminal_safe_text(title)),
        Line::from(format!("Status: {catalog_state}")),
        Line::from(""),
    ];
    for session in app.sessions.iter().take(4) {
        let name = session
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("Untitled");
        let marker = if Some(session.id.as_str()) == active_id {
            "▶"
        } else {
            " "
        };
        rail_lines.push(Line::from(format!(
            "{marker} {} · {}",
            sanitize::terminal_safe_text(name),
            session_state_label(session)
        )));
        rail_lines.push(Line::from(format!(
            "  {}",
            relative_time(session.updated_at_ms)
        )));
    }
    frame.render_widget(
        Paragraph::new(rail_lines)
            .block(Block::default().borders(Borders::ALL).title("Sessions"))
            .wrap(Wrap { trim: false }),
        body[0],
    );

    let transcript_area = body[1];
    let transcript_height = transcript_area.height.saturating_sub(2).max(1) as usize;
    let transcript_len = app.transcript.len();
    let transcript_width = transcript_area.width.saturating_sub(2).max(1) as usize;
    let wrap_turn = |index: usize, turn: &TranscriptTurn| {
        let text = format!("Turn {}\n{}", index + 1, turn.lines().join("\n"));
        wrap_transcript(&sanitize::terminal_safe_text(&text), transcript_width)
    };
    let mut tail_window = Vec::new();
    let mut tail_rows = 0;
    let mut tail_start = 0;
    for index in (0..transcript_len).rev() {
        let lines = wrap_turn(index, &app.transcript[index]);
        let required_rows = lines.len() + usize::from(!tail_window.is_empty());
        if !tail_window.is_empty() && tail_rows + required_rows > transcript_height {
            break;
        }
        tail_rows += required_rows;
        tail_start = index;
        tail_window.push((index, lines));
    }
    tail_window.reverse();
    app.transcript_max_scroll = tail_start;
    if app.transcript_follow_tail {
        app.transcript_scroll = tail_start;
    } else {
        app.transcript_scroll = app.transcript_scroll.min(app.transcript_max_scroll);
    }
    app.inspector_turn = app
        .inspector_turn
        .min(app.transcript.len().saturating_sub(1));
    let start_turn = app.transcript_scroll.min(transcript_len.saturating_sub(1));
    let page_window = if transcript_len == 0 {
        Vec::new()
    } else if start_turn == tail_start {
        tail_window
    } else {
        let mut page = Vec::new();
        let mut used_rows = 0;
        for index in start_turn..transcript_len {
            let lines = wrap_turn(index, &app.transcript[index]);
            let required_rows = lines.len() + usize::from(!page.is_empty());
            if !page.is_empty() && used_rows + required_rows > transcript_height {
                break;
            }
            used_rows += required_rows;
            page.push((index, lines));
        }
        page
    };
    let end_turn = page_window
        .last()
        .map(|(index, _)| index + 1)
        .unwrap_or(start_turn);
    app.transcript_page_rows = page_window.len().max(1);
    let wrapped_transcript = if transcript_len == 0 {
        wrap_transcript(
            "Choose a workspace, acknowledge the warning, then send a prompt with Ctrl-S.",
            transcript_width,
        )
    } else {
        let mut lines = Vec::new();
        for (_, turn_lines) in page_window {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.extend(turn_lines);
        }
        lines
    };
    let transcript_title = if transcript_len == 0 {
        "Conversation".to_string()
    } else if app.transcript_new_content {
        format!(
            "Turns {}-{} of {} · new content below (Ctrl-End)",
            start_turn + 1,
            end_turn,
            transcript_len
        )
    } else {
        format!(
            "Turns {}-{} of {}",
            start_turn + 1,
            end_turn,
            transcript_len
        )
    };
    frame.render_widget(
        Paragraph::new(wrapped_transcript).block(
            Block::default()
                .borders(Borders::ALL)
                .title(transcript_title),
        ),
        transcript_area,
    );
    if app.inspector_open {
        draw_inspector(frame, app, body[2]);
    }

    let composer_title = if app.turn_status.is_active() {
        "Composer (turn active; Ctrl-S disabled)"
    } else if app.read_only {
        "Composer (read-only session)"
    } else {
        "Composer (Ctrl-S send)"
    };
    let composer_width = chunks[2].width.saturating_sub(2).max(1) as usize;
    let composer_height = chunks[2].height.saturating_sub(2).max(1) as usize;
    let (composer_lines, (cursor_row, cursor_column)) = app.composer.layout(composer_width);
    let composer_text = composer_lines
        .into_iter()
        .map(|line| Line::from(sanitize::terminal_safe_text(&line)))
        .collect::<Vec<_>>();
    let composer_scroll = cursor_row.saturating_sub(composer_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(composer_text)
            .block(Block::default().borders(Borders::ALL).title(composer_title))
            .scroll((composer_scroll.min(u16::MAX as usize) as u16, 0)),
        chunks[2],
    );
    let help = if app.terminal_columns < 120 {
        NARROW_HELP
    } else {
        HELP
    };
    frame.render_widget(Paragraph::new(help), chunks[3]);

    if app.retry_confirmation.is_some() {
        let area = centered_rect(76, 28, frame.area());
        frame.render_widget(Clear, area);
        let warning = app
            .retry_confirmation
            .as_ref()
            .map(RetryIntent::warning)
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Explicit retry confirmation"),
                Line::from(""),
                Line::from(sanitize::terminal_safe_text(warning)),
                Line::from(""),
                Line::from("Press Y to resend, N or Esc to cancel. Enter does not confirm."),
            ])
            .block(Block::default().borders(Borders::ALL).title("Retry"))
            .wrap(Wrap { trim: false }),
            area,
        );
    } else if app.show_help {
        let area = centered_rect(96, 92, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Keyboard help"),
                Line::from("Enter newline · Ctrl-S send · Ctrl-C stop/exit · Ctrl-Z/Y undo/redo."),
                Line::from("Left/Right move · Home/End line · Backspace/Delete remove · paste keeps Unicode/newlines."),
                Line::from("PageUp/Down browse turns · Ctrl-End newest · Esc closes the current overlay."),
                Line::from("F1 help · F2 actions · F3 sessions: / filter · N new · R rename · W workspace · Enter reopen."),
                Line::from("F3 picker: S search · Ctrl-F searches titles and transcript · Up/Down move through results."),
                Line::from("Search: Enter opens · Ctrl-U clears · Esc closes. F4 inspects the current or selected turn."),
                Line::from("Inspector: Up/Down fields · [/] previous/next turn · Shift-PageUp/Down scrolls long fields."),
                Line::from("F5 copies the selected prompt/reply/code/tool field through terminal OSC 52; it never runs it."),
                Line::from("If clipboard access is blocked, F7 saves the selected field to a file. F6 exports transcript data only."),
                Line::from("F6 asks before replacing an existing export. Viewing and searching never send."),
                Line::from("Ctrl-R in the inspector retries the latest failed turn only after this warning:"),
                Line::from("Retry sends this prompt again and may repeat tool side effects."),
                Line::from("Press Y to retry; N/Esc cancels. Enter never confirms a retry."),
            ])
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .wrap(Wrap { trim: false }),
            area,
        );
    } else if app.show_menu {
        let area = centered_rect(82, 74, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Keyboard actions"),
                Line::from("F3  Browse/create/rename/reopen sessions; slash filters titles."),
                Line::from("Ctrl-F  Search titles and displayed prompt/reply/tool text."),
                Line::from("F4  Expand the selected turn's tool inspector."),
                Line::from("Up/Down  Choose inspector field; [/]  Select previous/next turn."),
                Line::from("F5  Send selected field with terminal OSC 52 clipboard."),
                Line::from("F7  Save the copied field to a file if clipboard is unavailable."),
                Line::from("F6  Export transcript only; Y explicitly confirms overwrite."),
                Line::from(
                    "Ctrl-R in inspector  Retry latest failed turn after the side-effect warning.",
                ),
                Line::from(
                    "PageUp/Down  Browse history; Shift-PageUp/Down  Scroll inspector text.",
                ),
                Line::from(
                    "Enter newline · Ctrl-S send · Ctrl-C stop or exit · F1 help · F2 actions.",
                ),
                Line::from("Esc closes this menu."),
            ])
            .block(Block::default().borders(Borders::ALL).title("Actions"))
            .wrap(Wrap { trim: false }),
            area,
        );
    } else {
        let visible_row = cursor_row.saturating_sub(composer_scroll);
        let x = chunks[2]
            .x
            .saturating_add(1 + cursor_column.min(u16::MAX as usize) as u16)
            .min(chunks[2].right().saturating_sub(2));
        let y = chunks[2]
            .y
            .saturating_add(1 + visible_row.min(u16::MAX as usize) as u16)
            .min(chunks[2].bottom().saturating_sub(2));
        frame.set_cursor_position((x, y));
    }
}

fn draw_inspector(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(turn) = app.transcript.get(app.inspector_turn) else {
        frame.render_widget(
            Paragraph::new("No turn selected").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Tool inspector"),
            ),
            area,
        );
        return;
    };
    let fields = turn.detail_fields();
    if fields.is_empty() {
        frame.render_widget(
            Paragraph::new("No inspectable details").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Tool inspector"),
            ),
            area,
        );
        return;
    }
    app.inspector_field = app.inspector_field.min(fields.len().saturating_sub(1));
    let (label, value) = &fields[app.inspector_field];
    let width = area.width.saturating_sub(2).max(1) as usize;
    let start = char_boundary_at_or_before(value, app.inspector_scroll.min(value.len()));
    app.inspector_scroll = start;
    let end = char_boundary_at_or_before(value, (start + 3_000).min(value.len()));
    let visible_rows = area.height.saturating_sub(5) as usize;
    let page_rows = visible_rows.saturating_sub(2).max(1);
    let page_end = byte_offset_after_wrapped_rows(value, start, width, page_rows).min(end);
    app.inspector_scroll_step =
        inspector_page_step(app.inspector_scroll_step, start, page_end, value.len());
    let before = if start > 0 {
        format!("[… {} bytes above …]\n", start)
    } else {
        String::new()
    };
    let after = if end < value.len() {
        format!(
            "\n[… {} bytes below; Shift-PageDown to continue …]",
            value.len() - end
        )
    } else {
        String::new()
    };
    let display = format!("{before}{}{after}", &value[start..end]);
    let lines = vec![
        Line::from(format!(
            "Turn {} · field {} of {}",
            app.inspector_turn + 1,
            app.inspector_field + 1,
            fields.len()
        )),
        Line::from(sanitize::terminal_safe_text(label)),
        Line::from(""),
    ];
    let mut lines = lines;
    lines.extend(wrap_transcript(
        &sanitize::terminal_safe_text(&display),
        width,
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Inspector · Up/Down field · F5 copy"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn session_state_label(session: &CatalogSession) -> &'static str {
    match session.run_state {
        CatalogRunState::Active => "Busy",
        CatalogRunState::Unknown => "Ownership unknown",
        CatalogRunState::Idle if session.recovered => "Recovered",
        CatalogRunState::Idle if session.read_only => "Read-only",
        CatalogRunState::Idle if session.cwd.as_deref().is_some_and(|path| !path.is_dir()) => {
            "Workspace missing"
        }
        CatalogRunState::Idle if session.cwd.is_none() => "Workspace required",
        CatalogRunState::Idle => "Ready",
    }
}

fn session_action_status(session: &CatalogSession) -> String {
    match session.run_state {
        CatalogRunState::Active => {
            "Busy: another interface owns this session. Browse history or wait before sending."
                .to_string()
        }
        CatalogRunState::Unknown => {
            "Ownership could not be verified; this session is read-only until it can be checked."
                .to_string()
        }
        CatalogRunState::Idle if session.cwd.as_deref().is_none_or(|path| !path.is_dir()) => {
            "Workspace is missing. Press W to choose an existing replacement.".to_string()
        }
        CatalogRunState::Idle if session.recovered => {
            "Recovered history. Choose a workspace with W before continuing.".to_string()
        }
        CatalogRunState::Idle if session.read_only => session
            .warnings
            .first()
            .cloned()
            .unwrap_or_else(|| "This session is read-only.".to_string()),
        CatalogRunState::Idle => {
            "Session reopened. History browsing did not start an exchange.".to_string()
        }
    }
}

fn relative_time(timestamp_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now_ms.saturating_sub(timestamp_ms) / 1_000;
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn search_excerpt(text: &str, query: &str) -> String {
    let query = query.to_lowercase().chars().collect::<Vec<_>>();
    let characters = text.chars().collect::<Vec<_>>();
    let lowered = text.to_lowercase().chars().collect::<Vec<_>>();
    let position = lowered
        .windows(query.len().max(1))
        .position(|window| window == query.as_slice())
        .unwrap_or(0);
    let start = position.saturating_sub(45);
    let end = (position + query.len() + 85).min(characters.len());
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < characters.len() { "…" } else { "" };
    format!(
        "{prefix}{}{suffix}",
        characters[start..end].iter().collect::<String>()
    )
}

fn osc52_sequence(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        encoded.push(TABLE[(a >> 2) as usize] as char);
        encoded.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

fn write_export_file(path: &Path, content: &[u8], replace: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("parent directory {} does not exist", parent.display()),
        ));
    }
    if path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "target is a directory",
        ));
    }
    if path.exists() && !replace {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "confirmation required before overwrite",
        ));
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let sequence = EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.leg-tui-{}-{sequence}.tmp", process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let write_result = file.write_all(content).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let result = if replace {
        fs::rename(&temporary, path)
    } else {
        fs::hard_link(&temporary, path).and_then(|()| fs::remove_file(&temporary))
    };
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn char_boundary_at_or_before(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn inspector_page_step(
    previous_step: usize,
    start: usize,
    page_end: usize,
    value_len: usize,
) -> usize {
    let current_step = page_end.saturating_sub(start).max(1);
    if page_end >= value_len {
        previous_step.max(current_step)
    } else {
        current_step
    }
}

fn next_inspector_scroll(current: usize, step: usize, value_len: usize) -> usize {
    let next = current.saturating_add(step);
    if next >= value_len { current } else { next }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::Duration;

    use serde_json::json;

    use super::{
        TurnStatus, response_stop_reason, restored_draft, should_use_color, successful_status,
    };

    #[test]
    fn color_policy_respects_no_color_and_dumb_term() {
        assert!(should_use_color(None, Some(OsStr::new("xterm-256color"))));
        assert!(!should_use_color(
            Some(OsStr::new("1")),
            Some(OsStr::new("xterm"))
        ));
        assert!(!should_use_color(None, Some(OsStr::new("dumb"))));
        assert!(should_use_color(
            Some(OsStr::new("")),
            Some(OsStr::new("xterm"))
        ));
    }

    #[test]
    fn queued_stream_events_cannot_replace_stopping_with_running() {
        let mut status = TurnStatus::Stopping;
        status.mark_running_unless_stopping();
        assert_eq!(status, TurnStatus::Stopping);
        status = TurnStatus::Starting;
        status.mark_running_unless_stopping();
        assert_eq!(status, TurnStatus::Running);
    }

    #[test]
    fn lifecycle_and_completion_warnings_have_distinct_text_states() {
        assert_eq!(TurnStatus::Idle.label(), "Idle");
        assert_eq!(TurnStatus::Starting.label(), "Starting");
        assert_eq!(TurnStatus::Running.label(), "Running");
        assert_eq!(TurnStatus::Stopping.label(), "Stopping");
        assert_eq!(TurnStatus::Succeeded.label(), "Succeeded");
        assert_eq!(TurnStatus::Failed.label(), "Failed");
        assert_eq!(TurnStatus::Interrupted.label(), "Interrupted");
        assert_eq!(
            TurnStatus::Incomplete { forced: false }.display(),
            "Incomplete"
        );
        assert_eq!(
            TurnStatus::Incomplete { forced: true }.display(),
            "Incomplete (forced cleanup)"
        );
        assert_eq!(TurnStatus::Capped.label(), "Capped");

        assert_eq!(successful_status(false, false), TurnStatus::Succeeded);
        assert_eq!(successful_status(false, true), TurnStatus::Truncated);
        assert_eq!(successful_status(true, false), TurnStatus::Capped);
    }

    #[test]
    fn max_token_truncation_comes_from_the_authoritative_response() {
        let response = json!({
            "exchange": {"exchange": {"outcome": {"stop_reason": "max_tokens"}}}
        });
        assert_eq!(response_stop_reason(&response), Some("max_tokens"));
        assert_eq!(successful_status(false, true), TurnStatus::Truncated);
        assert_eq!(successful_status(true, false), TurnStatus::Capped);
        assert_eq!(successful_status(false, false), TurnStatus::Succeeded);
    }

    #[test]
    fn restoration_keeps_a_newer_editable_draft() {
        assert_eq!(
            restored_draft("", Some("failed prompt".into()), false),
            "failed prompt"
        );
        assert_eq!(
            restored_draft("newer draft", Some("failed prompt".into()), true),
            "newer draft"
        );
    }

    #[test]
    fn transcript_wrap_keeps_long_and_wide_graphemes_navigable() {
        let rows = super::wrap_transcript("ab界cd\n", 4);
        let rows = rows
            .into_iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>();
        assert_eq!(rows, ["ab界", "cd", ""]);
    }

    #[test]
    fn inspector_pages_advance_by_visible_wrapped_rows() {
        let text = "ab界cd\nefghijk";
        let offset = super::byte_offset_after_wrapped_rows(text, 0, 4, 2);
        assert_eq!(&text[..offset], "ab界cd\n");

        let offset = super::byte_offset_after_wrapped_rows(text, offset, 4, 1);
        assert_eq!(&text[..offset], "ab界cd\nefgh");
    }

    #[test]
    fn inspector_final_page_keeps_a_full_step_for_page_up() {
        let step = super::inspector_page_step(1200, 3999, 4000, 4000);
        assert_eq!(step, 1200);
        assert_eq!(3999_usize.saturating_sub(step), 2799);

        assert_eq!(super::inspector_page_step(1200, 2400, 3200, 4000), 800);
    }

    #[test]
    fn inspector_page_down_stays_at_end_instead_of_showing_a_blank_page() {
        assert_eq!(super::next_inspector_scroll(0, 1200, 4000), 1200);
        assert_eq!(super::next_inspector_scroll(3600, 1200, 4000), 3600);
    }

    #[test]
    fn elapsed_time_is_rendered_as_minutes_and_seconds() {
        assert_eq!(super::format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(super::format_duration(Duration::from_secs(83)), "1:23");
    }
}
