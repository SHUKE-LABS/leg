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
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use signal_hook::low_level::unregister;
use transcript::{TranscriptBlockKind, TranscriptSourceId, TranscriptTurn};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const WARNING: &str = "Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.";
const WARNING_ACK_KEY: &str = "tui_first_run_warning_acknowledged";
const MIN_TERMINAL_COLUMNS: u16 = 80;
const MIN_TERMINAL_ROWS: u16 = 24;
const SESSION_RAIL_WIDTH: u16 = 25;
const SESSION_RAIL_MIN_COLUMNS: u16 = 105;
const MIN_DOCKED_CONVERSATION_WIDTH: u16 = 60;
const MIN_DOCKED_INSPECTOR_WIDTH: u16 = 36;
const MAX_COMPOSER_CONTENT_ROWS: usize = 4;
const MAX_TURN_MESSAGES_PER_CYCLE: usize = 64;
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(30);
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(34);
const ACTIVE_CLOCK_INTERVAL: Duration = Duration::from_secs(1);
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
    let area = terminal.size()?;
    app.set_terminal_size(area.width, area.height);
    let started = Instant::now();
    let mut redraw = RedrawScheduler::default();
    redraw.request();

    while !app.quit {
        app.begin_turn_message_cycle();
        let received_turn_messages = app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
        if received_turn_messages > 0 {
            redraw.request();
        }
        if let Some(signal) = signals.take() {
            app.handle_external_signal(signal);
            redraw.request();
        }
        if app.quit {
            break;
        }

        let now = started.elapsed();
        redraw.update_clock(now, app.turn_status.is_active());
        if redraw.is_due_after_batch(now, app.turn_messages_this_cycle) {
            let area = terminal.size()?;
            app.set_terminal_size(area.width, area.height);
            draw_terminal(&mut terminal, &mut app)?;
            redraw.mark_drawn(now);
            continue;
        }

        if event::poll(redraw.poll_timeout(now))? {
            let changed = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                    true
                }
                Event::Paste(text) => {
                    app.handle_paste(&text);
                    true
                }
                Event::Resize(columns, rows) => {
                    app.set_terminal_size(columns, rows);
                    true
                }
                _ => false,
            };
            if changed {
                redraw.request();
            }
        }
    }
    app.persist_all_drafts()?;
    Ok(())
}

fn draw_terminal<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    terminal.draw(|frame| draw(frame, app))?;
    app.render_counters.draws = app.render_counters.draws.saturating_add(1);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteAction {
    Sessions,
    NewConversation,
    Rename,
    Workspace,
    Search,
    Inspect,
    CopyField,
    SaveField,
    Export,
    Retry,
    ToggleRail,
    Stop,
    Help,
    Exit,
    Send,
    FocusTools,
}

struct PaletteEntry {
    action: PaletteAction,
    label: String,
    shortcut: &'static str,
    disabled_reason: Option<String>,
}

struct PaletteState {
    query: String,
    selected: usize,
    notice: Option<String>,
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
    focused_tool: Option<TranscriptAnchor>,
    overwrite_export: Option<PathBuf>,
    copy_text: Option<String>,
    terminal_columns: u16,
    terminal_rows: u16,
    rail_requested_visible: bool,
    use_color: bool,
    show_help: bool,
    palette: Option<PaletteState>,
    stop_chooser_open: bool,
    stop_targets: Vec<StopTarget>,
    stop_picker_index: usize,
    quit: bool,
    exit_after_turn: bool,
    render_counters: RenderCounters,
    turn_messages_this_cycle: usize,
    turn_tx: Sender<TurnMessage>,
    turn_rx: Receiver<TurnMessage>,
}

#[derive(Default)]
struct RenderCounters {
    draws: u64,
    turn_messages: u64,
    transcript_turn_rebuilds: u64,
    transcript_rows_built: u64,
}

#[derive(Default)]
struct CachedTranscriptTurn {
    revision: Option<u64>,
    width: usize,
    use_color: bool,
    row_start: usize,
    build_count: u64,
    rows: Vec<TranscriptRow>,
}

#[derive(Default)]
struct TranscriptRowsCache {
    turns: Vec<CachedTranscriptTurn>,
    total_rows: usize,
}

#[derive(Default)]
struct RedrawScheduler {
    requested: bool,
    deferred_full_batch: bool,
    last_draw: Option<Duration>,
    next_clock_tick: Option<Duration>,
    draws: u64,
}

impl RedrawScheduler {
    fn request(&mut self) {
        self.requested = true;
    }

    fn update_clock(&mut self, now: Duration, active: bool) {
        if !active {
            self.next_clock_tick = None;
            return;
        }
        let Some(next_tick) = self.next_clock_tick else {
            self.next_clock_tick = Some(now + ACTIVE_CLOCK_INTERVAL);
            return;
        };
        if now >= next_tick {
            self.request();
            let missed_ticks = now.saturating_sub(next_tick).as_secs().saturating_add(1);
            self.next_clock_tick = Some(next_tick + Duration::from_secs(missed_ticks));
        }
    }

    fn poll_timeout(&self, now: Duration) -> Duration {
        let mut timeout = EVENT_POLL_INTERVAL;
        if let Some(next_tick) = self.next_clock_tick {
            timeout = timeout.min(next_tick.saturating_sub(now));
        }
        if self.requested {
            if let Some(last_draw) = self.last_draw {
                let until_frame = MIN_RENDER_INTERVAL.saturating_sub(now.saturating_sub(last_draw));
                timeout = timeout.min(until_frame);
            } else {
                return Duration::ZERO;
            }
        }
        timeout
    }

    fn is_due(&self, now: Duration) -> bool {
        self.requested
            && self
                .last_draw
                .is_none_or(|last_draw| now.saturating_sub(last_draw) >= MIN_RENDER_INTERVAL)
    }

    fn is_due_after_batch(&mut self, now: Duration, received: usize) -> bool {
        if !self.is_due(now) {
            return false;
        }
        if received >= MAX_TURN_MESSAGES_PER_CYCLE && !self.deferred_full_batch {
            self.deferred_full_batch = true;
            return false;
        }
        true
    }

    fn mark_drawn(&mut self, now: Duration) {
        self.requested = false;
        self.deferred_full_batch = false;
        self.last_draw = Some(now);
        self.draws = self.draws.saturating_add(1);
    }
}

struct ConversationState {
    state_key: String,
    workspace: Option<PathBuf>,
    draft_id: Option<String>,
    session_id: Option<String>,
    composer: ComposerEditor,
    transcript: Vec<TranscriptTurn>,
    transcript_rows_cache: TranscriptRowsCache,
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
    transcript_anchor: Option<TranscriptAnchor>,
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
    anchor: Option<TranscriptAnchor>,
    label: String,
    excerpt: String,
}

struct SearchEntry {
    session_id: String,
    turn_index: Option<usize>,
    label: String,
    text: String,
    sources: Vec<SearchSourceSpan>,
}

#[derive(Clone, Debug)]
struct TranscriptAnchor {
    turn_index: usize,
    source_id: TranscriptSourceId,
    source_order: usize,
    byte_offset: usize,
}

struct SearchSourceSpan {
    start: usize,
    end: usize,
    source_id: TranscriptSourceId,
    source_order: usize,
}

#[derive(Clone)]
struct StopTarget {
    state_key: String,
    record_id: String,
    title: String,
    status: String,
    turn_index: Option<u64>,
}

struct ActionDisabledInputs {
    current_busy: bool,
    current_owner_unverified: bool,
    current_read_only: bool,
    workspace_available: bool,
    has_any_active_session: bool,
    has_unverified_session_owner: bool,
    background_stop_targets: Vec<StopTarget>,
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
            transcript_rows_cache: TranscriptRowsCache::default(),
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
            transcript_anchor: None,
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
            focused_tool: None,
            overwrite_export: None,
            copy_text: None,
            terminal_columns: MIN_TERMINAL_COLUMNS,
            terminal_rows: MIN_TERMINAL_ROWS,
            rail_requested_visible: true,
            use_color: terminal_color_enabled(),
            show_help: false,
            palette: None,
            stop_chooser_open: false,
            stop_targets: Vec::new(),
            stop_picker_index: 0,
            quit: false,
            exit_after_turn: false,
            render_counters: RenderCounters::default(),
            turn_messages_this_cycle: 0,
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

    fn session_rail_visible(&self) -> bool {
        self.rail_requested_visible && self.terminal_columns >= SESSION_RAIL_MIN_COLUMNS
    }

    fn toggle_session_rail(&mut self) {
        if self.terminal_columns < SESSION_RAIL_MIN_COLUMNS {
            self.status = format!(
                "Session rail is available at {SESSION_RAIL_MIN_COLUMNS} columns and wider"
            );
            return;
        }
        self.rail_requested_visible = !self.rail_requested_visible;
        self.status = if self.rail_requested_visible {
            "Session rail shown".to_string()
        } else {
            "Session rail hidden".to_string()
        };
    }

    fn session_title_for_view(&self, key: &str, view: &ConversationState) -> String {
        let record_id = view
            .session_id
            .as_deref()
            .or(view.draft_id.as_deref())
            .unwrap_or(key);
        self.sessions
            .iter()
            .find(|session| session.id == record_id || session.id == key)
            .and_then(|session| session.name.as_deref())
            .filter(|name| !name.trim().is_empty())
            .map(sanitize::terminal_safe_text)
            .unwrap_or_else(|| "Untitled conversation".to_string())
    }

    fn background_stop_targets(&self) -> Vec<StopTarget> {
        let mut targets = self
            .views
            .iter()
            .filter(|(_, view)| view.turn_status.is_active())
            .map(|(state_key, view)| {
                let record_id = view
                    .session_id
                    .as_deref()
                    .or(view.draft_id.as_deref())
                    .unwrap_or(state_key);
                StopTarget {
                    state_key: state_key.clone(),
                    record_id: record_id.to_string(),
                    title: self.session_title_for_view(state_key, view),
                    status: view.turn_status.display(),
                    turn_index: view.transcript.last().and_then(TranscriptTurn::turn_index),
                }
            })
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.record_id.cmp(&right.record_id))
        });
        targets
    }

    fn background_activity_text(&self) -> String {
        self.background_stop_targets()
            .iter()
            .map(|target| format!("{} ({})", target.title, target.status))
            .collect::<Vec<_>>()
            .join(", ")
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
                    sources: Vec::new(),
                });
            }
            for (index, turn) in session.turns.iter().enumerate() {
                let transcript = TranscriptTurn::from_trail(turn);
                let mut text = String::new();
                let mut sources = Vec::new();
                for (source_order, (source_id, source_text)) in
                    transcript.searchable_sources().into_iter().enumerate()
                {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    let start = text.len();
                    text.push_str(&source_text);
                    sources.push(SearchSourceSpan {
                        start,
                        end: text.len(),
                        source_id,
                        source_order,
                    });
                }
                entries.push(SearchEntry {
                    session_id: session.id.clone(),
                    turn_index: Some(index),
                    label: format!(
                        "{} · turn {}",
                        title.unwrap_or("Conversation"),
                        turn.turn_index + 1
                    ),
                    text,
                    sources,
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
            .filter_map(|entry| {
                let match_offset = find_case_insensitive(&entry.text, &query)?;
                let source = entry
                    .sources
                    .iter()
                    .find(|source| source.start <= match_offset && match_offset < source.end)
                    .or_else(|| entry.sources.last());
                let anchor =
                    entry
                        .turn_index
                        .zip(source)
                        .map(|(turn_index, source)| TranscriptAnchor {
                            turn_index,
                            source_id: source.source_id.clone(),
                            source_order: source.source_order,
                            byte_offset: match_offset
                                .saturating_sub(source.start)
                                .min(source.end.saturating_sub(source.start)),
                        });
                Some(SearchHit {
                    session_id: entry.session_id.clone(),
                    turn_index: entry.turn_index,
                    anchor,
                    label: entry.label.clone(),
                    excerpt: search_excerpt(&entry.text, &query),
                })
            })
            .collect();
        self.search_index = self
            .search_index
            .min(self.search_hits.len().saturating_sub(1));
    }

    fn open_session(
        &mut self,
        session_id: &str,
        turn_index: Option<usize>,
        anchor: Option<TranscriptAnchor>,
    ) -> bool {
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
            return self.open_session(session_id, turn_index, anchor);
        };
        let target_key = self
            .view_aliases
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.to_string());
        if target_key == self.state_key {
            if let Some(index) = turn_index {
                let turn_index = index.min(self.transcript.len().saturating_sub(1));
                let anchor = anchor.unwrap_or(TranscriptAnchor {
                    turn_index,
                    source_id: TranscriptSourceId::Prompt,
                    source_order: 0,
                    byte_offset: 0,
                });
                self.open_inspector_at_anchor(turn_index, anchor);
            } else {
                self.focused_tool = None;
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
        self.palette = None;
        self.inspector_open = turn_index.is_some();
        if let Some(index) = turn_index {
            let turn_index = index.min(self.transcript.len().saturating_sub(1));
            let anchor = anchor.unwrap_or(TranscriptAnchor {
                turn_index,
                source_id: TranscriptSourceId::Prompt,
                source_order: 0,
                byte_offset: 0,
            });
            self.open_inspector_at_anchor(turn_index, anchor);
        } else {
            self.focused_tool = None;
        }
        self.status = if self.turn_status.is_active() {
            "This session's turn is still running in the background; its stream continues here."
                .to_string()
        } else {
            session_action_status(&session)
        };
        true
    }

    fn open_inspector_at_anchor(&mut self, turn_index: usize, anchor: TranscriptAnchor) {
        self.inspector_open = true;
        self.inspector_turn = turn_index.min(self.transcript.len().saturating_sub(1));
        self.transcript_follow_tail = false;
        self.transcript_anchor = Some(anchor.clone());
        self.transcript_new_content = false;
        self.focused_tool =
            matches!(&anchor.source_id, TranscriptSourceId::Tool { .. }).then_some(anchor);
        self.inspector_field = self
            .focused_tool
            .as_ref()
            .and_then(|focused| match &focused.source_id {
                TranscriptSourceId::Tool {
                    round_index,
                    tool_use_id,
                } => self
                    .transcript
                    .get(self.inspector_turn)
                    .and_then(|turn| turn.tool_detail_field_index(*round_index, tool_use_id)),
                _ => None,
            })
            .unwrap_or(0);
        self.inspector_scroll = 0;
        self.inspector_scroll_step = 1;
    }

    fn focus_tool_row(&mut self, direction: isize) {
        let tools = self
            .transcript
            .iter()
            .enumerate()
            .flat_map(|(turn_index, turn)| {
                turn.source_blocks().into_iter().enumerate().filter_map(
                    move |(source_order, block)| {
                        matches!(&block.source_id, TranscriptSourceId::Tool { .. }).then_some(
                            TranscriptAnchor {
                                turn_index,
                                source_id: block.source_id,
                                source_order,
                                byte_offset: 0,
                            },
                        )
                    },
                )
            })
            .collect::<Vec<_>>();
        if tools.is_empty() {
            self.status = "No tool rows in this conversation".to_string();
            return;
        }

        let current_index = self.focused_tool.as_ref().and_then(|focused| {
            tools
                .iter()
                .position(|candidate| same_tool_anchor(candidate, focused))
        });
        let target_index = if let Some(current_index) = current_index {
            if direction < 0 {
                current_index.checked_sub(1)
            } else {
                (current_index + 1 < tools.len()).then_some(current_index + 1)
            }
        } else if direction < 0 {
            let current = self.transcript_anchor.as_ref();
            tools.iter().rposition(|candidate| {
                current.is_none_or(|anchor| anchor_precedes(candidate, anchor))
            })
        } else {
            let current = self.transcript_anchor.as_ref();
            tools.iter().position(|candidate| {
                current.is_none_or(|anchor| anchor_precedes(anchor, candidate))
            })
        };
        let Some(target) = target_index.and_then(|index| tools.get(index)).cloned() else {
            self.status = if direction < 0 {
                "No earlier tool rows".to_string()
            } else {
                "No later tool rows".to_string()
            };
            return;
        };
        self.inspector_open = false;
        self.focused_tool = Some(target.clone());
        self.transcript_follow_tail = false;
        self.transcript_anchor = Some(target);
        self.transcript_new_content = false;
        self.status = "Tool row focused · Ctrl-Up/Down move · F4 inspect".to_string();
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
        if !self.open_session(session_id, None, None) {
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

    fn current_record_id(&self) -> Option<&str> {
        self.session_id.as_deref().or(self.draft_id.as_deref())
    }

    fn current_catalog_session(&self) -> Option<&CatalogSession> {
        let id = self.current_record_id()?;
        self.sessions.iter().find(|session| session.id == id)
    }

    fn current_session_title(&self) -> String {
        self.current_catalog_session()
            .and_then(|session| session.name.as_deref())
            .filter(|name| !name.trim().is_empty())
            .map(sanitize::terminal_safe_text)
            .unwrap_or_else(|| "Untitled conversation".to_string())
    }

    fn has_any_active_session(&self) -> bool {
        self.turn_status.is_active()
            || self.views.values().any(|view| view.turn_status.is_active())
            || self
                .sessions
                .iter()
                .any(|session| session.run_state == CatalogRunState::Active)
    }

    fn has_unverified_session_owner(&self) -> bool {
        self.sessions
            .iter()
            .any(|session| session.run_state == CatalogRunState::Unknown)
    }

    fn has_tool_rows(&self) -> bool {
        self.transcript.iter().any(TranscriptTurn::has_tool_rows)
    }

    fn action_disabled_inputs(&self) -> ActionDisabledInputs {
        let current_session = self.current_catalog_session();
        ActionDisabledInputs {
            current_busy: self.turn_status.is_active()
                || current_session
                    .is_some_and(|session| session.run_state == CatalogRunState::Active),
            current_owner_unverified: current_session
                .is_some_and(|session| session.run_state == CatalogRunState::Unknown),
            current_read_only: self.read_only
                || current_session.is_some_and(|session| session.read_only),
            workspace_available: self.workspace.as_ref().is_some_and(|path| path.is_dir()),
            has_any_active_session: self.has_any_active_session(),
            has_unverified_session_owner: self.has_unverified_session_owner(),
            background_stop_targets: self.background_stop_targets(),
        }
    }

    fn action_disabled_reason(&self, action: PaletteAction) -> Option<String> {
        let inputs = self.action_disabled_inputs();
        self.action_disabled_reason_with_inputs(action, &inputs)
    }

    fn action_disabled_reason_with_inputs(
        &self,
        action: PaletteAction,
        inputs: &ActionDisabledInputs,
    ) -> Option<String> {
        match action {
            PaletteAction::Sessions if self.sessions.is_empty() => {
                Some("No sessions to browse.".to_string())
            }
            PaletteAction::Rename if self.current_record_id().is_none() => {
                Some("No session is open.".to_string())
            }
            PaletteAction::Rename if inputs.current_busy => {
                Some("Session has an active turn.".to_string())
            }
            PaletteAction::Rename if inputs.current_owner_unverified => {
                Some("Session ownership could not be verified.".to_string())
            }
            PaletteAction::Workspace if inputs.current_busy => {
                Some("Session has an active turn.".to_string())
            }
            PaletteAction::Workspace if inputs.current_owner_unverified => {
                Some("Session ownership could not be verified.".to_string())
            }
            PaletteAction::Search if self.sessions.is_empty() => {
                Some("No saved session data to search.".to_string())
            }
            PaletteAction::Inspect if self.transcript.is_empty() => {
                Some("No transcript fields to inspect.".to_string())
            }
            PaletteAction::CopyField
                if !self.inspector_open || self.selected_detail_field().is_none() =>
            {
                Some("Open the inspector and select a field.".to_string())
            }
            PaletteAction::SaveField
                if (!self.inspector_open || self.selected_detail_field().is_none())
                    && self.copy_text.is_none() =>
            {
                Some("Open the inspector or copy a field first.".to_string())
            }
            PaletteAction::Export if self.transcript.is_empty() => {
                Some("No transcript to export.".to_string())
            }
            PaletteAction::Retry if self.session_id.is_none() => {
                Some("No saved session to retry.".to_string())
            }
            PaletteAction::Retry if inputs.current_busy => {
                Some("Session has an active turn.".to_string())
            }
            PaletteAction::Retry if inputs.current_owner_unverified => {
                Some("Session ownership could not be verified.".to_string())
            }
            PaletteAction::Retry if inputs.current_read_only => {
                Some("Session is read-only.".to_string())
            }
            PaletteAction::Retry if !inputs.workspace_available => {
                Some("Recorded workspace is missing.".to_string())
            }
            PaletteAction::Retry
                if !self
                    .transcript
                    .last()
                    .is_some_and(TranscriptTurn::retryable) =>
            {
                Some("No eligible failed or incomplete latest turn.".to_string())
            }
            PaletteAction::ToggleRail if self.terminal_columns < SESSION_RAIL_MIN_COLUMNS => Some(
                format!("Available at {SESSION_RAIL_MIN_COLUMNS} columns and wider."),
            ),
            PaletteAction::Stop if inputs.current_owner_unverified => {
                Some("Session ownership could not be verified.".to_string())
            }
            PaletteAction::Stop if self.turn_status.is_stopping() => {
                Some("Current session is already stopping.".to_string())
            }
            PaletteAction::Stop if self.turn_status.is_active() && self.stop_handle.is_none() => {
                Some("Current session has no Stop control.".to_string())
            }
            PaletteAction::Stop
                if !self.turn_status.is_active()
                    && inputs.background_stop_targets.is_empty()
                    && self
                        .sessions
                        .iter()
                        .any(|session| session.run_state == CatalogRunState::Active) =>
            {
                Some("Active turn is owned by another interface.".to_string())
            }
            PaletteAction::Stop
                if !self.turn_status.is_active() && inputs.background_stop_targets.is_empty() =>
            {
                Some("No active TUI session to stop.".to_string())
            }
            PaletteAction::Exit if inputs.has_any_active_session => {
                let catalog_title = self
                    .sessions
                    .iter()
                    .find(|session| session.run_state == CatalogRunState::Active)
                    .and_then(|session| session.name.as_deref())
                    .filter(|name| !name.trim().is_empty())
                    .map(sanitize::terminal_safe_text);
                let active_title = catalog_title.unwrap_or_else(|| {
                    if self.turn_status.is_active() {
                        self.current_session_title()
                    } else {
                        inputs
                            .background_stop_targets
                            .first()
                            .map(|target| target.title.clone())
                            .unwrap_or_else(|| "session".to_string())
                    }
                });
                Some(format!("Active turn in {active_title}."))
            }
            PaletteAction::Exit if inputs.has_unverified_session_owner => {
                Some("Session ownership could not be verified.".to_string())
            }
            PaletteAction::Send if self.terminal_too_small() => {
                Some("Terminal must be at least 80x24.".to_string())
            }
            PaletteAction::Send if inputs.current_busy => {
                Some("Session has an active turn.".to_string())
            }
            PaletteAction::Send if inputs.current_owner_unverified => {
                Some("Session ownership could not be verified.".to_string())
            }
            PaletteAction::Send if inputs.current_read_only => {
                Some("Session is read-only.".to_string())
            }
            PaletteAction::Send if !inputs.workspace_available => {
                Some("Choose an existing workspace.".to_string())
            }
            PaletteAction::Send if self.composer.text().trim().is_empty() => {
                Some("Prompt is empty.".to_string())
            }
            PaletteAction::FocusTools if !self.has_tool_rows() => {
                Some("No tool rows in this conversation.".to_string())
            }
            _ => None,
        }
    }

    fn palette_entries(&self) -> Vec<PaletteEntry> {
        let inputs = self.action_disabled_inputs();
        let mut entries = vec![
            (
                PaletteAction::Sessions,
                "Browse/switch sessions".to_string(),
                "F3",
            ),
            (
                PaletteAction::NewConversation,
                "New conversation".to_string(),
                "F3 → N",
            ),
            (
                PaletteAction::Rename,
                "Rename current session".to_string(),
                "F3 → R",
            ),
            (
                PaletteAction::Workspace,
                "Choose/replace workspace".to_string(),
                "F3 → W",
            ),
            (
                PaletteAction::Search,
                "Search sessions and transcript".to_string(),
                "Ctrl-F",
            ),
            (
                PaletteAction::Inspect,
                "Inspect transcript".to_string(),
                "F4",
            ),
            (
                PaletteAction::CopyField,
                "Copy selected field".to_string(),
                "F5",
            ),
            (
                PaletteAction::SaveField,
                "Save selected field".to_string(),
                "F7",
            ),
            (PaletteAction::Export, "Export transcript".to_string(), "F6"),
            (
                PaletteAction::Retry,
                "Retry latest failed turn".to_string(),
                "Ctrl-R",
            ),
            (
                PaletteAction::ToggleRail,
                "Show/hide session rail".to_string(),
                "F8",
            ),
            (
                PaletteAction::Stop,
                if self.turn_status.is_active() {
                    format!("Stop {}", self.current_session_title())
                } else {
                    "Choose background session to stop".to_string()
                },
                "Ctrl-C",
            ),
            (PaletteAction::Help, "Keyboard help".to_string(), "F1"),
            (
                PaletteAction::Exit,
                "Exit and save drafts".to_string(),
                "Ctrl-C",
            ),
        ]
        .into_iter()
        .map(|(action, label, shortcut)| PaletteEntry {
            action,
            label,
            shortcut,
            disabled_reason: self.action_disabled_reason_with_inputs(action, &inputs),
        })
        .collect::<Vec<_>>();
        for entry in &mut entries {
            entry.label = sanitize::terminal_safe_text(&entry.label);
        }
        entries
    }

    fn filtered_palette_entries(&self) -> Vec<PaletteEntry> {
        let query = self
            .palette
            .as_ref()
            .map(|palette| palette.query.trim().to_lowercase())
            .unwrap_or_default();
        self.palette_entries()
            .into_iter()
            .filter(|entry| query.is_empty() || entry.label.to_lowercase().contains(&query))
            .collect()
    }

    fn begin_palette(&mut self) {
        self.refresh_sessions();
        self.palette = Some(PaletteState {
            query: String::new(),
            selected: 0,
            notice: None,
        });
        self.status = "Command palette open".to_string();
    }

    fn idle_footer_text(&self, width: usize) -> String {
        let inputs = self.action_disabled_inputs();
        let enabled = |action| {
            self.action_disabled_reason_with_inputs(action, &inputs)
                .is_none()
        };
        let mut hints = vec!["F2/Ctrl-P actions".to_string()];
        if enabled(PaletteAction::Send) {
            hints.push("Ctrl-S send".to_string());
        }
        hints.push("F1 help".to_string());
        if enabled(PaletteAction::Stop) {
            hints.push("Ctrl-C stop".to_string());
        } else if enabled(PaletteAction::Exit) {
            hints.push("Ctrl-C exit".to_string());
        } else if !inputs.background_stop_targets.is_empty() {
            hints.push("Ctrl-C choose Stop".to_string());
        }
        let mut optional = Vec::new();
        if enabled(PaletteAction::Sessions) {
            optional.push("F3 sessions".to_string());
        }
        if enabled(PaletteAction::Inspect) {
            optional.push("F4 inspect".to_string());
        }
        if enabled(PaletteAction::ToggleRail) {
            optional.push("F8 rail".to_string());
        }
        if enabled(PaletteAction::FocusTools) {
            optional.push("Ctrl-↑/↓ tools".to_string());
        }
        if enabled(PaletteAction::CopyField) {
            optional.push("F5 copy".to_string());
        }
        if enabled(PaletteAction::SaveField) {
            optional.push("F7 save".to_string());
        }
        if enabled(PaletteAction::Export) {
            optional.push("F6 export".to_string());
        }
        if enabled(PaletteAction::Retry) {
            optional.push("Ctrl-R retry".to_string());
        }
        for hint in optional {
            let candidate = format!("{} · {hint}", hints.join(" · "));
            if UnicodeWidthStr::width(candidate.as_str()) <= width {
                hints.push(hint);
            }
        }
        hints.join(" · ")
    }

    fn move_palette_selection(&mut self, direction: isize) {
        let count = self.filtered_palette_entries().len();
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        if count == 0 {
            palette.selected = 0;
        } else if direction < 0 {
            palette.selected = palette.selected.saturating_sub(1);
        } else {
            palette.selected = (palette.selected + 1).min(count - 1);
        }
        palette.notice = None;
    }

    fn normalize_palette_selection(&mut self) {
        let count = self.filtered_palette_entries().len();
        if let Some(palette) = self.palette.as_mut() {
            palette.selected = palette.selected.min(count.saturating_sub(1));
            if count == 0 {
                palette.selected = 0;
            }
        }
    }

    fn handle_palette_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            if let Some(palette) = self.palette.as_mut() {
                palette.query.clear();
                palette.selected = 0;
                palette.notice = None;
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.palette = None;
                self.status = "Composer focused".to_string();
            }
            KeyCode::Up => self.move_palette_selection(-1),
            KeyCode::Down => self.move_palette_selection(1),
            KeyCode::Enter => {
                let Some(action) = self
                    .filtered_palette_entries()
                    .get(self.palette.as_ref().map_or(0, |palette| palette.selected))
                    .map(|entry| entry.action)
                else {
                    return;
                };

                // Process a bounded batch before refreshing ownership and dispatching.
                self.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
                self.refresh_sessions();
                if let Some(reason) = self.action_disabled_reason(action) {
                    self.status = reason.clone();
                    if let Some(palette) = self.palette.as_mut() {
                        palette.notice = Some(reason);
                    }
                    self.normalize_palette_selection();
                    return;
                }
                self.palette = None;
                self.invoke_palette_action(action);
            }
            KeyCode::Backspace => {
                if let Some(palette) = self.palette.as_mut()
                    && let Some((start, _)) = palette.query.grapheme_indices(true).next_back()
                {
                    palette.query.truncate(start);
                    palette.selected = 0;
                    palette.notice = None;
                }
                self.normalize_palette_selection();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                if let Some(palette) = self.palette.as_mut() {
                    palette.query.push(character);
                    palette.selected = 0;
                    palette.notice = None;
                }
                self.normalize_palette_selection();
            }
            _ => {}
        }
    }

    fn invoke_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Sessions => self.begin_session_picker(),
            PaletteAction::NewConversation => self.create_session(),
            PaletteAction::Rename => {
                self.begin_session_picker();
                self.begin_rename_selected();
            }
            PaletteAction::Workspace => {
                if let Some(session_id) = self.current_record_id().map(str::to_owned) {
                    self.begin_workspace_replacement(&session_id);
                } else {
                    self.workspace_flow = WorkspaceFlow::Initial;
                    self.workspace_input.clear();
                    self.screen = Screen::Workspace;
                    self.status = "Choose an existing workspace directory".to_string();
                }
            }
            PaletteAction::Search => self.begin_search(),
            PaletteAction::Inspect => self.toggle_inspector(),
            PaletteAction::CopyField => self.copy_selected_detail(),
            PaletteAction::SaveField => self.save_selected_field(),
            PaletteAction::Export => self.begin_export(),
            PaletteAction::Retry => {
                self.inspector_turn = self.transcript.len().saturating_sub(1);
                self.retry_selected_turn();
            }
            PaletteAction::ToggleRail => self.toggle_session_rail(),
            PaletteAction::Stop => self.stop_from_palette(),
            PaletteAction::Help => self.show_help = true,
            PaletteAction::Exit => self.quit = true,
            PaletteAction::Send | PaletteAction::FocusTools => {}
        }
    }

    fn toggle_inspector(&mut self) {
        if self.inspector_open {
            self.inspector_open = false;
        } else if let Some(anchor) = self.focused_tool.clone() {
            self.open_inspector_at_anchor(anchor.turn_index, anchor);
        } else {
            self.inspector_open = true;
            self.inspector_turn = self.transcript.len().saturating_sub(1);
            self.inspector_field = 0;
            self.inspector_scroll = 0;
            self.inspector_scroll_step = 1;
        }
    }

    fn save_selected_field(&mut self) {
        if let Some((label, text)) = self.selected_detail_field() {
            self.copy_text = Some(text);
            self.begin_copy_fallback(&label);
        } else if self.copy_text.is_some() {
            self.begin_copy_fallback("selected transcript text");
        } else {
            self.status = "Open the inspector and select a transcript field first".to_string();
        }
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
                    self.open_session(&session_id, None, None);
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
                    let anchor = hit.anchor.clone();
                    self.open_session(&session_id, turn_index, anchor);
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
        if self.stop_chooser_open {
            self.handle_stop_chooser_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.show_help = false;
            self.palette = None;
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
        if self.palette.is_some() {
            self.handle_palette_key(key);
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.begin_palette();
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Up {
            self.focus_tool_row(-1);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Down {
            self.focus_tool_row(1);
            return;
        }
        match key.code {
            KeyCode::Esc if self.inspector_open => self.inspector_open = false,
            KeyCode::Esc => self.status = "Composer focused".to_string(),
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.begin_palette(),
            KeyCode::F(3) => self.begin_session_picker(),
            KeyCode::F(4) => self.toggle_inspector(),
            KeyCode::F(5) => self.copy_selected_detail(),
            KeyCode::F(6) => self.begin_export(),
            KeyCode::F(7) => self.save_selected_field(),
            KeyCode::F(8) => self.toggle_session_rail(),
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
        if let Some(palette) = self.palette.as_mut() {
            palette.query.push_str(&normalize_paste(pasted));
            palette.selected = 0;
            palette.notice = None;
            self.normalize_palette_selection();
            return;
        }
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
        self.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
        if self.turn_status.is_active() {
            if self.turn_status.is_stopping() {
                return;
            }
            self.stop_current_session();
            return;
        }
        self.refresh_sessions();
        if self.open_background_stop_chooser() {
            return;
        }
        if self.has_any_active_session() {
            self.status = "Cannot exit while another interface owns an active session".to_string();
            return;
        }
        if self.has_unverified_session_owner() {
            self.status = "Cannot exit while session ownership is unverified".to_string();
            return;
        }
        self.quit = true;
    }

    fn stop_from_palette(&mut self) {
        self.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
        if self.turn_status.is_active() {
            self.stop_current_session();
            return;
        }
        self.refresh_sessions();
        if !self.open_background_stop_chooser() {
            self.status = "No active TUI session to stop".to_string();
        }
    }

    fn open_background_stop_chooser(&mut self) -> bool {
        self.stop_targets = self.background_stop_targets();
        if self.stop_targets.is_empty() {
            return false;
        }
        self.stop_picker_index = 0;
        self.stop_chooser_open = true;
        self.status = "Choose a background session to stop".to_string();
        true
    }

    fn stop_current_session(&mut self) {
        if self.turn_status.is_stopping() {
            return;
        }
        let Some(handle) = self.stop_handle.clone() else {
            self.status = "Active turn has no Stop control".to_string();
            return;
        };
        let Some(record_id) = self.current_record_id().map(str::to_owned) else {
            self.status = "Could not identify the active session; nothing was stopped".to_string();
            return;
        };
        let current_turn_index = self.transcript.last().and_then(TranscriptTurn::turn_index);
        let session = match self.catalog.get(&record_id) {
            Ok(session) => session,
            Err(error) => {
                self.status =
                    format!("Could not verify the active session ({error}); nothing was stopped");
                return;
            }
        };
        if session.run_state != CatalogRunState::Active
            || current_turn_index.is_some_and(|turn_index| {
                session.turns.last().map(|turn| turn.turn_index) != Some(turn_index)
            })
        {
            self.refresh_sessions();
            self.status = "Active session changed state; nothing was stopped".to_string();
            return;
        }
        match handle.stop() {
            Ok(()) => {
                self.turn_status = TurnStatus::Stopping;
                self.status.clear();
            }
            Err(error) => self.status = format!("Stop failed: {error}"),
        }
    }

    fn handle_stop_chooser_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.stop_chooser_open = false;
                self.stop_targets.clear();
                self.status = "Background Stop cancelled".to_string();
            }
            KeyCode::Up => {
                self.stop_picker_index = self.stop_picker_index.saturating_sub(1);
            }
            KeyCode::Down => {
                self.stop_picker_index =
                    (self.stop_picker_index + 1).min(self.stop_targets.len().saturating_sub(1));
            }
            KeyCode::Enter => self.stop_selected_background(),
            _ => {}
        }
    }

    fn stop_selected_background(&mut self) {
        let Some(target) = self.stop_targets.get(self.stop_picker_index).cloned() else {
            self.stop_chooser_open = false;
            self.status = "No background Stop target is available".to_string();
            return;
        };
        let Some(view) = self.views.get(&target.state_key) else {
            self.close_stop_chooser(format!(
                "{} is no longer active; nothing was stopped",
                target.title
            ));
            return;
        };
        if !view.turn_status.is_active()
            || target.turn_index.is_some_and(|turn_index| {
                view.transcript.last().and_then(TranscriptTurn::turn_index) != Some(turn_index)
            })
        {
            self.close_stop_chooser(format!(
                "{} changed state; nothing was stopped",
                target.title
            ));
            return;
        }
        let record_id = view
            .session_id
            .as_deref()
            .or(view.draft_id.as_deref())
            .unwrap_or(&view.state_key)
            .to_string();
        let Some(handle) = view.stop_handle.clone() else {
            self.close_stop_chooser(format!(
                "{} has no Stop control; nothing was stopped",
                target.title
            ));
            return;
        };
        let current_turn_index = view.transcript.last().and_then(TranscriptTurn::turn_index);

        let session = match self.catalog.get(&record_id) {
            Ok(session) => session,
            Err(error) => {
                self.close_stop_chooser(format!(
                    "Could not verify {} ({error}); nothing was stopped",
                    target.title
                ));
                return;
            }
        };
        if session.run_state != CatalogRunState::Active
            || current_turn_index.is_some_and(|turn_index| {
                session.turns.last().map(|turn| turn.turn_index) != Some(turn_index)
            })
        {
            self.close_stop_chooser(format!(
                "{} changed state; nothing was stopped",
                target.title
            ));
            return;
        }

        self.stop_chooser_open = false;
        self.stop_targets.clear();
        match handle.stop() {
            Ok(()) => {
                if let Some(view) = self.views.get_mut(&target.state_key) {
                    view.turn_status = TurnStatus::Stopping;
                }
                self.status = format!("Stopping {}", target.title);
            }
            Err(error) => {
                self.status = format!("Stop failed for {}: {error}", target.title);
            }
        }
    }

    fn close_stop_chooser(&mut self, message: String) {
        self.stop_chooser_open = false;
        self.stop_targets.clear();
        self.refresh_sessions();
        self.status = message;
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
        self.transcript_scroll = transcript_page_up_start(
            self.transcript_scroll,
            self.transcript_max_scroll,
            self.transcript_page_rows,
            self.transcript_follow_tail,
        );
        self.transcript_follow_tail = false;
        self.transcript_anchor = None;
    }

    fn scroll_transcript_down(&mut self) {
        if let Some(next) = transcript_page_down_start(
            self.transcript_scroll,
            self.transcript_max_scroll,
            self.transcript_page_rows,
        ) {
            self.transcript_follow_tail = false;
            self.transcript_scroll = next;
            self.transcript_anchor = None;
        } else {
            self.scroll_to_newest();
        }
    }

    fn scroll_to_newest(&mut self) {
        self.transcript_follow_tail = true;
        self.transcript_anchor = None;
        self.focused_tool = None;
        self.transcript_scroll = 0;
        self.transcript_new_content = false;
    }

    fn begin_turn_message_cycle(&mut self) {
        self.turn_messages_this_cycle = 0;
    }

    fn receive_turn_messages(&mut self, limit: usize) -> usize {
        let remaining = limit.saturating_sub(self.turn_messages_this_cycle);
        let mut received = 0;
        while received < remaining {
            match self.turn_rx.try_recv() {
                Ok(TurnMessage::Event { owner, event }) => {
                    self.with_conversation(&owner, |app| app.handle_stream_event(event));
                    received += 1;
                }
                Ok(TurnMessage::Finished { owner, result }) => {
                    self.with_conversation(&owner, |app| app.finish_turn(result));
                    self.refresh_sessions();
                    received += 1;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        self.render_counters.turn_messages = self
            .render_counters
            .turn_messages
            .saturating_add(received as u64);
        self.turn_messages_this_cycle += received;
        received
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

struct TranscriptRow {
    line: Line<'static>,
    anchor: TranscriptAnchor,
}

impl TranscriptRowsCache {
    fn refresh(
        &mut self,
        turns: &[TranscriptTurn],
        width: usize,
        use_color: bool,
    ) -> (usize, usize) {
        self.turns.truncate(turns.len());
        self.turns
            .resize_with(turns.len(), CachedTranscriptTurn::default);
        let mut rebuilt_turns = 0;
        let mut built_rows = 0;
        for (turn_index, turn) in turns.iter().enumerate() {
            let revision = turn.render_revision();
            let cached = &mut self.turns[turn_index];
            if cached.revision != Some(revision)
                || cached.width != width
                || cached.use_color != use_color
            {
                cached.rows = build_transcript_turn_rows(turn, turn_index, width, use_color);
                cached.revision = Some(revision);
                cached.width = width;
                cached.use_color = use_color;
                cached.build_count = cached.build_count.saturating_add(1);
                rebuilt_turns += 1;
                built_rows += cached.rows.len();
            }
        }
        let mut row_start = 0;
        for cached in &mut self.turns {
            cached.row_start = row_start;
            row_start = row_start.saturating_add(cached.rows.len());
        }
        self.total_rows = row_start;
        (rebuilt_turns, built_rows)
    }

    fn row_for_anchor(&self, anchor: &TranscriptAnchor) -> Option<usize> {
        let cached = self.turns.get(anchor.turn_index)?;
        find_transcript_anchor_row(&cached.rows, anchor)
            .map(|row_index| cached.row_start + row_index)
    }

    fn rows_in_range(&self, start: usize, end: usize) -> Vec<&TranscriptRow> {
        let start = start.min(self.total_rows);
        let end = end.min(self.total_rows).max(start);
        let mut rows = Vec::with_capacity(end - start);
        for cached in &self.turns {
            let cached_end = cached.row_start + cached.rows.len();
            let local_start = start
                .saturating_sub(cached.row_start)
                .min(cached.rows.len());
            let local_end = end.saturating_sub(cached.row_start).min(cached.rows.len());
            if start < cached_end && local_start < local_end {
                rows.extend(cached.rows[local_start..local_end].iter());
            }
        }
        rows
    }
}

fn build_transcript_turn_rows(
    turn: &TranscriptTurn,
    turn_index: usize,
    width: usize,
    use_color: bool,
) -> Vec<TranscriptRow> {
    let mut rows =
        build_transcript_rows_with_focus(std::slice::from_ref(turn), width, use_color, None);
    for row in &mut rows {
        row.anchor.turn_index = turn_index;
    }
    if let Some(header) = rows.first_mut() {
        header.line = Line::from(Span::styled(
            format!("Turn {}", turn_index + 1),
            transcript_style(TranscriptBlockKind::Outcome, use_color),
        ));
    }
    rows
}

fn transcript_row_line(
    row: &TranscriptRow,
    focused_tool: Option<&TranscriptAnchor>,
    width: usize,
    use_color: bool,
) -> Line<'static> {
    if !focused_tool.is_some_and(|focused| same_tool_anchor(&row.anchor, focused)) {
        return row.line.clone();
    }
    let style = transcript_style(TranscriptBlockKind::Tool, use_color);
    let summary = row
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    Line::from(vec![
        Span::styled("› ", style.add_modifier(Modifier::REVERSED)),
        Span::styled(
            truncate_to_width(&summary, width.saturating_sub(2)),
            style.add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
    ])
}

fn same_tool_anchor(left: &TranscriptAnchor, right: &TranscriptAnchor) -> bool {
    left.turn_index == right.turn_index && left.source_id == right.source_id
}

fn anchor_precedes(left: &TranscriptAnchor, right: &TranscriptAnchor) -> bool {
    (left.turn_index, left.source_order) < (right.turn_index, right.source_order)
}

struct RenderRun {
    text: String,
    style: Style,
    source_offset: usize,
    tracks_source: bool,
}

#[cfg(test)]
fn build_transcript_rows(
    turns: &[TranscriptTurn],
    width: usize,
    use_color: bool,
) -> Vec<TranscriptRow> {
    build_transcript_rows_with_focus(turns, width, use_color, None)
}

fn build_transcript_rows_with_focus(
    turns: &[TranscriptTurn],
    width: usize,
    use_color: bool,
    focused_tool: Option<&TranscriptAnchor>,
) -> Vec<TranscriptRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (turn_index, turn) in turns.iter().enumerate() {
        let header_anchor = TranscriptAnchor {
            turn_index,
            source_id: TranscriptSourceId::Prompt,
            source_order: usize::MAX,
            byte_offset: 0,
        };
        rows.push(TranscriptRow {
            line: Line::from(Span::styled(
                format!("Turn {}", turn_index + 1),
                transcript_style(TranscriptBlockKind::Outcome, use_color),
            )),
            anchor: header_anchor,
        });

        for (source_order, block) in turn.source_blocks().into_iter().enumerate() {
            let anchor = TranscriptAnchor {
                turn_index,
                source_id: block.source_id.clone(),
                source_order,
                byte_offset: 0,
            };
            match block.kind {
                TranscriptBlockKind::Prompt => {
                    push_wrapped_text_block(
                        &mut rows,
                        &block.text,
                        &anchor,
                        width,
                        Some(("You: ", transcript_style(block.kind, use_color))),
                        transcript_style(block.kind, use_color),
                    );
                }
                TranscriptBlockKind::Assistant => {
                    rows.push(TranscriptRow {
                        line: Line::from(Span::styled(
                            "Assistant",
                            transcript_style(block.kind, use_color),
                        )),
                        anchor: anchor.clone(),
                    });
                    for (line_offset, runs) in markdown_display_lines(&block.text, use_color) {
                        push_wrapped_runs(&mut rows, &runs, &anchor, line_offset, width);
                    }
                }
                TranscriptBlockKind::Tool => {
                    let focused = focused_tool
                        .is_some_and(|focused_tool| same_tool_anchor(&anchor, focused_tool));
                    let style = transcript_style(block.kind, use_color);
                    let summary_width = width.saturating_sub(if focused { 2 } else { 0 });
                    let summary = truncate_to_width(&block.text, summary_width);
                    let line = if focused {
                        Line::from(vec![
                            Span::styled("› ", style.add_modifier(Modifier::REVERSED)),
                            Span::styled(
                                summary,
                                style.add_modifier(Modifier::REVERSED | Modifier::BOLD),
                            ),
                        ])
                    } else {
                        Line::from(Span::styled(summary, style))
                    };
                    rows.push(TranscriptRow {
                        line,
                        anchor: anchor.clone(),
                    });
                }
                TranscriptBlockKind::Outcome => push_labeled_text_block(
                    &mut rows,
                    "Outcome: ",
                    &block.text,
                    &anchor,
                    width,
                    transcript_style(block.kind, use_color),
                ),
                TranscriptBlockKind::Error => push_labeled_text_block(
                    &mut rows,
                    "Error: ",
                    &block.text,
                    &anchor,
                    width,
                    transcript_style(block.kind, use_color),
                ),
                TranscriptBlockKind::Warning => push_labeled_text_block(
                    &mut rows,
                    "Warning: ",
                    &block.text,
                    &anchor,
                    width,
                    transcript_style(block.kind, use_color),
                ),
            }
            if block.kind != TranscriptBlockKind::Tool {
                rows.push(TranscriptRow {
                    line: Line::from(""),
                    anchor: TranscriptAnchor {
                        byte_offset: block.text.len(),
                        ..anchor
                    },
                });
            }
        }
    }
    rows
}

fn transcript_style(kind: TranscriptBlockKind, use_color: bool) -> Style {
    let (color, modifier) = match kind {
        TranscriptBlockKind::Prompt => (Color::Blue, Modifier::BOLD),
        TranscriptBlockKind::Assistant => (Color::Green, Modifier::BOLD),
        TranscriptBlockKind::Tool => (Color::Yellow, Modifier::BOLD),
        TranscriptBlockKind::Outcome => (Color::Magenta, Modifier::BOLD),
        TranscriptBlockKind::Error => (Color::Red, Modifier::BOLD | Modifier::UNDERLINED),
        TranscriptBlockKind::Warning => (Color::Yellow, Modifier::BOLD | Modifier::UNDERLINED),
    };
    if use_color {
        Style::default().fg(color).add_modifier(modifier)
    } else {
        Style::default().add_modifier(modifier)
    }
}

fn markdown_style(use_color: bool, color: Color) -> Style {
    if use_color {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn source_run(text: &str, style: Style, source_offset: usize) -> RenderRun {
    RenderRun {
        text: text.to_string(),
        style,
        source_offset,
        tracks_source: true,
    }
}

fn visual_run(text: &str, style: Style, source_offset: usize) -> RenderRun {
    RenderRun {
        text: text.to_string(),
        style,
        source_offset,
        tracks_source: false,
    }
}

fn push_labeled_text_block(
    rows: &mut Vec<TranscriptRow>,
    label: &str,
    text: &str,
    anchor: &TranscriptAnchor,
    width: usize,
    style: Style,
) {
    let mut offset = 0;
    for (line_index, logical_line) in text.split('\n').enumerate() {
        let mut runs = Vec::new();
        if line_index == 0 {
            runs.push(visual_run(label, style, offset));
        } else {
            runs.push(visual_run("  ", style, offset));
        }
        runs.push(source_run(logical_line, style, offset));
        push_wrapped_runs(rows, &runs, anchor, offset, width);
        offset += logical_line.len() + 1;
    }
}

fn push_wrapped_text_block(
    rows: &mut Vec<TranscriptRow>,
    text: &str,
    anchor: &TranscriptAnchor,
    width: usize,
    first_prefix: Option<(&str, Style)>,
    body_style: Style,
) {
    let mut offset = 0;
    for (line_index, logical_line) in text.split('\n').enumerate() {
        let mut runs = Vec::new();
        if line_index == 0 {
            if let Some((prefix, style)) = first_prefix {
                runs.push(visual_run(prefix, style, offset));
            }
        } else if first_prefix.is_some() {
            runs.push(visual_run("  ", body_style, offset));
        }
        runs.push(source_run(logical_line, body_style, offset));
        push_wrapped_runs(rows, &runs, anchor, offset, width);
        offset += logical_line.len() + 1;
    }
}

fn push_wrapped_runs(
    rows: &mut Vec<TranscriptRow>,
    runs: &[RenderRun],
    anchor: &TranscriptAnchor,
    fallback_offset: usize,
    width: usize,
) {
    let width = width.max(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut row_width = 0_usize;
    let mut row_offset = None;
    for run in runs {
        for (relative_offset, grapheme) in
            UnicodeSegmentation::grapheme_indices(run.text.as_str(), true)
        {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
                rows.push(TranscriptRow {
                    line: Line::from(std::mem::take(&mut spans)),
                    anchor: TranscriptAnchor {
                        byte_offset: row_offset.unwrap_or(fallback_offset),
                        ..anchor.clone()
                    },
                });
                row_width = 0;
                row_offset = None;
            }
            if row_offset.is_none() {
                row_offset = Some(if run.tracks_source {
                    run.source_offset + relative_offset
                } else {
                    fallback_offset
                });
            }
            if let Some(last) = spans.last_mut()
                && last.style == run.style
            {
                last.content.to_mut().push_str(grapheme);
            } else {
                spans.push(Span::styled(grapheme.to_string(), run.style));
            }
            row_width = row_width.saturating_add(grapheme_width);
        }
    }
    rows.push(TranscriptRow {
        line: Line::from(spans),
        anchor: TranscriptAnchor {
            byte_offset: row_offset.unwrap_or(fallback_offset),
            ..anchor.clone()
        },
    });
}

fn markdown_display_lines(text: &str, use_color: bool) -> Vec<(usize, Vec<RenderRun>)> {
    let base_style = Style::default();
    let heading_style = markdown_style(use_color, Color::Cyan);
    let list_style = markdown_style(use_color, Color::Blue);
    let code_style = if use_color {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::UNDERLINED)
    };
    let lines = text
        .split('\n')
        .scan(0_usize, |offset, line| {
            let current = *offset;
            *offset += line.len() + 1;
            Some((current, line))
        })
        .collect::<Vec<_>>();
    let mut rendered = Vec::new();
    let mut fence: Option<(char, usize, String)> = None;

    for (line_offset, line) in lines {
        if let Some((marker, length, _)) = &fence {
            if is_closing_fence(line, *marker, *length) {
                fence = None;
                continue;
            }
            rendered.push((line_offset, vec![source_run(line, code_style, line_offset)]));
            continue;
        }
        if let Some((marker, length, language)) = opening_fence(line) {
            let label = if language.is_empty() {
                "Code block:".to_string()
            } else {
                format!("Code block ({language}):")
            };
            rendered.push((
                line_offset,
                vec![visual_run(&label, heading_style, line_offset)],
            ));
            fence = Some((marker, length, language));
            continue;
        }

        let (content_offset, content, style, prefix) =
            if let Some((level, offset)) = heading_content(line) {
                (
                    line_offset + offset,
                    &line[offset..],
                    heading_style,
                    format!("{} ", "#".repeat(level)),
                )
            } else if let Some((indent, marker_end)) = list_content(line) {
                let marker = if indent > 0 { "  • " } else { "• " };
                (
                    line_offset + marker_end,
                    &line[marker_end..],
                    list_style,
                    marker.to_string(),
                )
            } else {
                (line_offset, line, base_style, String::new())
            };
        let mut runs = Vec::new();
        if !prefix.is_empty() {
            runs.push(visual_run(&prefix, style, line_offset));
        }
        match inline_markdown_runs(content, content_offset, style, code_style, use_color) {
            Some(inline_runs) => runs.extend(inline_runs),
            None => runs.push(source_run(line, base_style, line_offset)),
        }
        rendered.push((line_offset, runs));
    }
    rendered
}

fn heading_content(line: &str) -> Option<(usize, usize)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let bytes = line.as_bytes();
    let mut cursor = indent;
    while bytes.get(cursor) == Some(&b'#') && cursor - indent < 6 {
        cursor += 1;
    }
    let level = cursor - indent;
    if level == 0 || !matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        return None;
    }
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    Some((level, cursor))
}

fn list_content(line: &str) -> Option<(usize, usize)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let bytes = line.as_bytes();
    let marker_start = indent;
    let marker = *bytes.get(marker_start)?;
    if matches!(marker, b'-' | b'*' | b'+')
        && matches!(bytes.get(marker_start + 1), Some(b' ' | b'\t'))
    {
        let mut end = marker_start + 2;
        while matches!(bytes.get(end), Some(b' ' | b'\t')) {
            end += 1;
        }
        return Some((indent, end));
    }
    let mut end = marker_start;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end > marker_start
        && matches!(bytes.get(end), Some(b'.' | b')'))
        && matches!(bytes.get(end + 1), Some(b' ' | b'\t'))
    {
        end += 2;
        while matches!(bytes.get(end), Some(b' ' | b'\t')) {
            end += 1;
        }
        return Some((indent, end));
    }
    None
}

fn opening_fence(line: &str) -> Option<(char, usize, String)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let length = rest
        .chars()
        .take_while(|character| *character == marker)
        .count();
    if length < 3 {
        return None;
    }
    let language = rest[length..].trim().to_string();
    if marker == '`' && language.contains('`') {
        return None;
    }
    Some((marker, length, language))
}

fn is_closing_fence(line: &str, marker: char, minimum_length: usize) -> bool {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let length = rest
        .chars()
        .take_while(|character| *character == marker)
        .count();
    length >= minimum_length && rest[length..].trim().is_empty()
}

fn inline_markdown_runs(
    text: &str,
    source_offset: usize,
    base_style: Style,
    code_style: Style,
    use_color: bool,
) -> Option<Vec<RenderRun>> {
    let mut runs = Vec::new();
    let mut cursor = 0_usize;
    let mut plain_start = 0_usize;
    while let Some(relative_start) = text[cursor..].find('`') {
        let start = cursor + relative_start;
        let delimiter_length = text[start..]
            .bytes()
            .take_while(|byte| *byte == b'`')
            .count();
        let content_start = start + delimiter_length;
        let mut search = content_start;
        let close = loop {
            let relative_close = text[search..].find('`')?;
            let close_start = search + relative_close;
            let close_length = text[close_start..]
                .bytes()
                .take_while(|byte| *byte == b'`')
                .count();
            if close_length == delimiter_length {
                break close_start;
            }
            search = close_start + close_length;
            if search >= text.len() {
                return None;
            }
        };
        if start > plain_start {
            runs.push(source_run(
                &text[plain_start..start],
                base_style,
                source_offset + plain_start,
            ));
        }
        if !use_color {
            runs.push(visual_run(
                &text[start..content_start],
                code_style,
                source_offset + start,
            ));
        }
        runs.push(source_run(
            &text[content_start..close],
            code_style,
            source_offset + content_start,
        ));
        if !use_color {
            let close_end = close + delimiter_length;
            runs.push(visual_run(
                &text[close..close_end],
                code_style,
                source_offset + close,
            ));
        }
        cursor = close + delimiter_length;
        plain_start = cursor;
    }
    if plain_start < text.len() {
        runs.push(source_run(
            &text[plain_start..],
            base_style,
            source_offset + plain_start,
        ));
    }
    Some(runs)
}

fn find_transcript_anchor_row(rows: &[TranscriptRow], anchor: &TranscriptAnchor) -> Option<usize> {
    let matches_exact_source = |row: &&TranscriptRow| {
        row.anchor.turn_index == anchor.turn_index
            && row.anchor.source_id == anchor.source_id
            && row.anchor.source_order == anchor.source_order
    };
    rows.iter()
        .enumerate()
        .filter(|(_, row)| matches_exact_source(row))
        .filter(|(_, row)| row.anchor.byte_offset <= anchor.byte_offset)
        .last()
        .or_else(|| {
            rows.iter()
                .enumerate()
                .filter(|(_, row)| matches_exact_source(row))
                .min_by_key(|(_, row)| row.anchor.byte_offset.abs_diff(anchor.byte_offset))
        })
        .or_else(|| {
            rows.iter()
                .enumerate()
                .filter(|(_, row)| {
                    row.anchor.turn_index == anchor.turn_index
                        && row.anchor.source_id == anchor.source_id
                })
                .min_by_key(|(_, row)| row.anchor.byte_offset.abs_diff(anchor.byte_offset))
        })
        .or_else(|| {
            rows.iter()
                .enumerate()
                .filter(|(_, row)| row.anchor.turn_index == anchor.turn_index)
                .min_by_key(|(_, row)| {
                    (
                        row.anchor.source_order.abs_diff(anchor.source_order),
                        row.anchor.byte_offset.abs_diff(anchor.byte_offset),
                    )
                })
        })
        .map(|(index, _)| index)
}

fn transcript_page_up_start(
    current: usize,
    max_scroll: usize,
    page_rows: usize,
    following_tail: bool,
) -> usize {
    if following_tail {
        max_scroll.saturating_sub(page_rows)
    } else {
        current.saturating_sub(page_rows)
    }
}

fn transcript_page_down_start(
    current: usize,
    max_scroll: usize,
    page_rows: usize,
) -> Option<usize> {
    let next = current.saturating_add(page_rows);
    (next < max_scroll).then_some(next)
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
            "F1 help; F2/Ctrl-P actions; F3 sessions; Ctrl-F search; F4 inspect; F5 copy; F6 export.",
        ),
        Line::from("PageUp/PageDown move by transcript rows; Ctrl-End follows new text."),
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
        let model = if current {
            format!("  ·  model: {}", sanitize::terminal_safe_text(&app.model))
        } else {
            String::new()
        };
        let second = Line::from(format!(
            "    {}  ·  {} turns  ·  {}{}",
            sanitize::terminal_safe_text(&workspace),
            session.turns.len(),
            sanitize::terminal_safe_text(&session.id),
            model,
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
            "Search titles and complete sanitized transcript source.",
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
    let frame_area = frame.area();
    let composer_width = frame_area.width.saturating_sub(2).max(1) as usize;
    let (composer_lines, (cursor_row, cursor_column)) = app.composer.layout(composer_width);
    let composer_content_rows = composer_lines.len().clamp(1, MAX_COMPOSER_CONTENT_ROWS);
    let composer_panel_height = (composer_content_rows as u16).saturating_add(2);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(composer_panel_height),
            Constraint::Length(1),
        ])
        .split(frame_area);

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
    let elapsed = format_duration(app.elapsed());
    let catalog_state = active
        .map(|session| session_state_label(session).to_string())
        .unwrap_or_else(|| app.external_run_state.clone());
    let catalog_summary = format!("{catalog_state} · {elapsed}");
    let narrow_header = frame_area.width < 120;
    let (status_width, title_width, model_width) = if narrow_header {
        (23, 14, 18)
    } else {
        (27, 32, 30)
    };
    let header_first = format!(
        "status: {}  |  {}  |  model: {}",
        truncate_to_width(&app.turn_status.display(), status_width),
        truncate_to_width(&sanitize::terminal_safe_text(title), title_width),
        truncate_to_width(&sanitize::terminal_safe_text(&app.model), model_width),
    );
    let (workspace_width, catalog_width, tool_width) = if narrow_header {
        (19, 14, 12)
    } else {
        (32, 20, 24)
    };
    let tool = app
        .active_tool
        .as_deref()
        .map(|tool| format!(" · tool: {}", truncate_to_width(tool, tool_width)))
        .unwrap_or_default();
    let header_second = format!(
        "workspace: {} · catalog: {}{}",
        truncate_to_width(&sanitize::terminal_safe_text(&workspace), workspace_width),
        truncate_to_width(&catalog_summary, catalog_width),
        tool,
    );
    let header = Paragraph::new(vec![
        Line::from(truncate_to_width(&header_first, frame_area.width as usize)),
        Line::from(truncate_to_width(&header_second, frame_area.width as usize)),
    ]);
    frame.render_widget(
        if app.use_color {
            header.style(Style::default().fg(Color::Cyan))
        } else {
            header
        },
        chunks[0],
    );

    let rail_visible = app.session_rail_visible();
    let rail_width = if rail_visible { SESSION_RAIL_WIDTH } else { 0 };
    let workbench_width = frame_area.width.saturating_sub(rail_width);
    let inspector_docked = app.inspector_open
        && workbench_width >= MIN_DOCKED_CONVERSATION_WIDTH + MIN_DOCKED_INSPECTOR_WIDTH;
    let mut body_constraints = Vec::new();
    if rail_visible {
        body_constraints.push(Constraint::Length(SESSION_RAIL_WIDTH));
    }
    if inspector_docked {
        body_constraints.push(Constraint::Min(MIN_DOCKED_CONVERSATION_WIDTH));
        body_constraints.push(Constraint::Length(MIN_DOCKED_INSPECTOR_WIDTH));
    } else {
        body_constraints.push(Constraint::Min(1));
    }
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(body_constraints)
        .split(chunks[1]);
    let transcript_index = if rail_visible { 1 } else { 0 };
    if rail_visible {
        let mut rail_lines = vec![
            Line::from("F3 picker · F8 hide"),
            Line::from(truncate_to_width(
                &sanitize::terminal_safe_text(title),
                (SESSION_RAIL_WIDTH - 2) as usize,
            )),
            Line::from(truncate_to_width(
                &format!("Status: {catalog_state}"),
                (SESSION_RAIL_WIDTH - 2) as usize,
            )),
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
                "{marker} {}",
                truncate_to_width(
                    &sanitize::terminal_safe_text(name),
                    (SESSION_RAIL_WIDTH - 4) as usize,
                )
            )));
            rail_lines.push(Line::from(truncate_to_width(
                &format!("  {}", session_state_label(session)),
                (SESSION_RAIL_WIDTH - 2) as usize,
            )));
        }
        frame.render_widget(
            Paragraph::new(rail_lines)
                .block(Block::default().borders(Borders::ALL).title("Sessions")),
            body[0],
        );
    }

    let transcript_area = body[transcript_index];
    let transcript_height = transcript_area.height.saturating_sub(2).max(1) as usize;
    let transcript_len = app.transcript.len();
    let transcript_width = transcript_area.width.saturating_sub(2).max(1) as usize;
    let (rebuilt_turns, built_rows, total_rows) = {
        let view = &mut app.view;
        let transcript = &view.transcript;
        let (rebuilt_turns, built_rows) =
            view.transcript_rows_cache
                .refresh(transcript, transcript_width, app.use_color);
        (
            rebuilt_turns,
            built_rows,
            view.transcript_rows_cache.total_rows,
        )
    };
    app.render_counters.transcript_turn_rebuilds = app
        .render_counters
        .transcript_turn_rebuilds
        .saturating_add(rebuilt_turns as u64);
    app.render_counters.transcript_rows_built = app
        .render_counters
        .transcript_rows_built
        .saturating_add(built_rows as u64);
    let max_scroll = total_rows.saturating_sub(transcript_height);
    app.transcript_max_scroll = max_scroll;
    app.transcript_page_rows = transcript_height;
    let start_row = if app.transcript_follow_tail {
        max_scroll
    } else if let Some(anchor) = &app.transcript_anchor {
        app.view
            .transcript_rows_cache
            .row_for_anchor(anchor)
            .unwrap_or(app.transcript_scroll)
            .min(max_scroll)
    } else {
        app.transcript_scroll.min(max_scroll)
    };
    app.transcript_scroll = start_row;
    if app.transcript_follow_tail {
        app.transcript_anchor = None;
    } else {
        app.transcript_anchor = app
            .view
            .transcript_rows_cache
            .rows_in_range(start_row, start_row + 1)
            .first()
            .map(|row| row.anchor.clone());
    }
    app.inspector_turn = app
        .inspector_turn
        .min(app.transcript.len().saturating_sub(1));
    let end_row = (start_row + transcript_height).min(total_rows);
    let wrapped_transcript = if transcript_len == 0 {
        wrap_transcript(
            "Choose a workspace, acknowledge the warning, then send a prompt with Ctrl-S.",
            transcript_width,
        )
    } else {
        app.view
            .transcript_rows_cache
            .rows_in_range(start_row, end_row)
            .into_iter()
            .map(|row| {
                transcript_row_line(
                    row,
                    app.focused_tool.as_ref(),
                    transcript_width,
                    app.use_color,
                )
            })
            .collect()
    };
    let transcript_title = if transcript_len == 0 {
        "Conversation".to_string()
    } else if app.transcript_new_content {
        format!(
            "Rows {}-{} of {} · new content below (Ctrl-End)",
            start_row + 1,
            end_row,
            total_rows
        )
    } else {
        format!("Rows {}-{} of {}", start_row + 1, end_row, total_rows)
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
        if inspector_docked {
            draw_inspector(frame, app, body[transcript_index + 1]);
        } else {
            let area = centered_rect(90, 72, frame_area);
            frame.render_widget(Clear, area);
            draw_inspector(frame, app, area);
        }
    }

    let composer_title = if app.turn_status.is_active() {
        "Composer (turn active; Ctrl-S disabled)"
    } else if app.read_only {
        "Composer (read-only session)"
    } else {
        "Composer (Ctrl-S send)"
    };
    let composer_height = chunks[2].height.saturating_sub(2).max(1) as usize;
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
    let footer_text = if app.stop_chooser_open {
        "↑/↓ choose background session · Enter stop selected · Esc cancel".to_string()
    } else {
        let detail = app
            .terminal_detail
            .as_deref()
            .or_else(|| (!app.status.is_empty()).then_some(app.status.as_str()))
            .unwrap_or_default();
        let blocked_reason = app.terminal_detail.is_some()
            || [
                "Busy:",
                "Send rejected:",
                "Blank prompts",
                "Choose a workspace",
                "This session is",
                "Recorded workspace",
                "Workspace is",
                "Could not",
                "Stop failed",
                "Ownership",
            ]
            .iter()
            .any(|prefix| detail.starts_with(prefix));
        let background = app.background_activity_text();
        if blocked_reason {
            detail.to_string()
        } else if !background.is_empty() {
            format!("Background: {background} · Ctrl-C choose Stop · F3 sessions")
        } else if !detail.is_empty() && detail != "Ready" {
            detail.to_string()
        } else {
            app.idle_footer_text(chunks[3].width as usize)
        }
    };
    frame.render_widget(
        Paragraph::new(truncate_to_width(
            &sanitize::terminal_safe_text(&footer_text),
            chunks[3].width as usize,
        )),
        chunks[3],
    );

    if app.stop_chooser_open {
        draw_stop_chooser(frame, app);
    } else if app.retry_confirmation.is_some() {
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
                Line::from("PageUp/Down browse transcript rows · Ctrl-End follows the tail · Esc closes the current overlay."),
                Line::from("Ctrl-Up/Down focuses the previous/next tool row without editing the composer."),
                Line::from("F1 help · F2/Ctrl-P actions · F3 sessions: / filter · N new · R rename · W workspace · Enter reopen."),
                Line::from("F8 shows or hides the session rail at 105 columns and wider."),
                Line::from("F3 picker: S search · Ctrl-F searches titles and complete transcript source · Up/Down move through results."),
                Line::from("Search: Enter opens the match and matching tool details · Ctrl-U clears · Esc closes."),
                Line::from("F4 inspects the focused tool call; without a focused row, it toggles the latest turn inspector."),
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
    } else if app.palette.is_some() {
        draw_command_palette(frame, app);
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

fn draw_command_palette(frame: &mut Frame<'_>, app: &App) {
    let Some(palette) = app.palette.as_ref() else {
        return;
    };
    let area = centered_rect(98, 96, frame.area());
    let entries = app.filtered_palette_entries();
    let page_rows = area.height.saturating_sub(5).max(1) as usize;
    let start = palette
        .selected
        .saturating_sub(page_rows.saturating_sub(1))
        .min(entries.len().saturating_sub(page_rows));
    let end = (start + page_rows).min(entries.len());
    let width = area.width.saturating_sub(4) as usize;
    let mut lines = vec![Line::from(format!(
        "Filter: {}▏",
        sanitize::terminal_safe_text(&palette.query)
    ))];
    if entries.is_empty() {
        lines.push(Line::from(
            "No actions match. Type more or press Ctrl-U to clear.",
        ));
    } else {
        for (index, entry) in entries.iter().enumerate().take(end).skip(start) {
            let marker = if index == palette.selected {
                ">"
            } else if entry.disabled_reason.is_some() {
                "×"
            } else {
                " "
            };
            let disabled = if entry.disabled_reason.is_some() {
                " · disabled"
            } else {
                ""
            };
            let line = Line::from(truncate_to_width(
                &sanitize::terminal_safe_text(&format!(
                    "{marker} {}  [{}]{disabled}",
                    entry.label, entry.shortcut
                )),
                width,
            ));
            if index == palette.selected && app.use_color {
                lines.push(
                    line.style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                );
            } else if entry.disabled_reason.is_some() && app.use_color {
                lines.push(line.style(Style::default().fg(Color::DarkGray)));
            } else {
                lines.push(line);
            }
        }
    }
    let selected_reason = entries
        .get(palette.selected)
        .and_then(|entry| entry.disabled_reason.as_deref());
    let notice = palette.notice.as_deref().or(selected_reason);
    lines.push(Line::from(truncate_to_width(
        &sanitize::terminal_safe_text(
            notice.unwrap_or("Enter invokes enabled · ↑/↓ select · Ctrl-U clear · Esc close"),
        ),
        width,
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Command palette · F2/Ctrl-P"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
    let cursor_x = area
        .x
        .saturating_add(
            1 + UnicodeWidthStr::width("Filter: ") as u16
                + UnicodeWidthStr::width(palette.query.as_str()) as u16,
        )
        .min(area.right().saturating_sub(2));
    frame.set_cursor_position((cursor_x, area.y.saturating_add(1)));
}

fn draw_stop_chooser(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(78, 70, frame.area());
    let mut lines = vec![
        Line::from("Choose a background session to stop"),
        Line::from("The selected run is rechecked before Stop is sent."),
        Line::from(""),
    ];
    for (index, target) in app.stop_targets.iter().enumerate() {
        let state = app
            .views
            .get(&target.state_key)
            .map(|view| {
                if view.turn_status.is_active() {
                    view.turn_status.display()
                } else {
                    "No longer active".to_string()
                }
            })
            .unwrap_or_else(|| "No longer available".to_string());
        let marker = if index == app.stop_picker_index {
            ">"
        } else {
            " "
        };
        lines.push(Line::from(truncate_to_width(
            &format!(
                "{marker} {} · {} · {}",
                target.title, state, target.record_id
            ),
            area.width.saturating_sub(4) as usize,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "↑/↓ select · Enter stop only this run · Esc cancel",
    ));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Stop background run"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
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
    let after = if page_end < value.len() {
        format!(
            "\n[… {} bytes below; Shift-PageDown to continue …]",
            value.len() - page_end
        )
    } else {
        String::new()
    };
    let display = format!("{before}{}{after}", &value[start..page_end]);
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
    lines.extend(wrap_inspector_value(
        &display,
        width,
        label.ends_with(" · diff"),
        app.use_color,
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

fn wrap_inspector_value(
    value: &str,
    width: usize,
    is_edit_diff: bool,
    use_color: bool,
) -> Vec<Line<'static>> {
    let safe = sanitize::terminal_safe_text(value);
    let mut diff_started = false;
    let mut rows = Vec::new();
    for logical_line in safe.split('\n') {
        if is_edit_diff && logical_line.starts_with("--- ") {
            diff_started = true;
        }
        let style = if is_edit_diff && diff_started {
            edit_diff_line_style(logical_line, use_color)
        } else {
            Style::default()
        };
        for mut row in wrap_transcript(logical_line, width) {
            row.style = style;
            rows.push(row);
        }
    }
    rows
}

fn edit_diff_line_style(line: &str, use_color: bool) -> Style {
    let (color, modifier) = if line.starts_with("... [diff truncated:") {
        (Color::Yellow, Modifier::BOLD | Modifier::UNDERLINED)
    } else if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("@@") {
        (Color::Cyan, Modifier::BOLD)
    } else if line.starts_with('+') {
        (Color::Green, Modifier::BOLD)
    } else if line.starts_with('-') {
        (Color::Red, Modifier::UNDERLINED)
    } else if line.starts_with(' ') {
        (Color::Blue, Modifier::DIM)
    } else {
        (Color::Reset, Modifier::empty())
    };
    let style = if use_color {
        Style::default().fg(color)
    } else {
        Style::default()
    };
    style.add_modifier(modifier)
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

fn find_case_insensitive(text: &str, query: &str) -> Option<usize> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut lowered = String::with_capacity(text.len());
    let mut source_offsets = Vec::with_capacity(text.len());
    for (source_offset, character) in text.char_indices() {
        let lowered_character = character.to_lowercase().collect::<String>();
        source_offsets.extend(std::iter::repeat_n(source_offset, lowered_character.len()));
        lowered.push_str(&lowered_character);
    }
    lowered
        .find(&needle)
        .and_then(|offset| source_offsets.get(offset).copied())
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

fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let content_width = width - 1;
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > content_width {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result.push('…');
    result
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
mod responsiveness_tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use leg_ui_client::{StreamEvent, TurnOutcome};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::{
        App, ConversationState, MAX_TURN_MESSAGES_PER_CYCLE, MIN_RENDER_INTERVAL, RedrawScheduler,
        Screen, SessionCatalog, SessionCatalogConfig, TranscriptTurn, TurnMessage, TurnStatus,
        draw_terminal,
    };

    fn test_app(name: &str) -> (App, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let state_dir =
            std::env::temp_dir().join(format!("leg-tui-{name}-{}-{unique}", std::process::id()));
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state_dir.clone()),
            ..SessionCatalogConfig::default()
        })
        .expect("temporary test catalog opens");
        let mut app = App::new(catalog);
        app.screen = Screen::Conversation;
        app.use_color = false;
        app.set_terminal_size(80, 24);
        (app, state_dir)
    }

    fn text_delta(seq: u64, block_index: u64, text: String) -> StreamEvent {
        StreamEvent::TextDelta {
            seq,
            round_index: 0,
            block_index,
            text,
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn seeded_history_is_cached_and_stays_quiescent_for_thirty_seconds() {
        let (mut app, state_dir) = test_app("idle-cache");
        let base_reply = "r".repeat(4 * 1024);
        let mut turns = (0..1_000)
            .map(|index| {
                let mut turn = TranscriptTurn::new(&format!("prompt {index}"));
                assert!(turn.observe(&text_delta(0, 0, base_reply.clone())));
                turn
            })
            .collect::<Vec<_>>();
        let long_answer = (0..10_000)
            .map(|line| format!("answer line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(long_answer.lines().count(), 10_000);
        turns[998].observe(&text_delta(1, 1, long_answer));
        let tool_output = "t".repeat(1024 * 1024);
        assert_eq!(tool_output.len(), 1024 * 1024);
        turns[999].observe(&StreamEvent::ToolCall {
            seq: 0,
            round_index: 0,
            tool_use_id: "large-result".to_string(),
            tool_name: "bash".to_string(),
            input: json!({"command": "fixture"}),
        });
        turns[999].observe(&StreamEvent::ToolResult {
            seq: 1,
            round_index: 0,
            tool_use_id: "large-result".to_string(),
            tool_name: "bash".to_string(),
            status: "completed".to_string(),
            output: json!({"stdout": tool_output, "exit_code": 0}),
        });
        app.transcript = turns;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal opens");
        let mut scheduler = RedrawScheduler::default();
        scheduler.request();
        assert!(scheduler.is_due(Duration::ZERO));
        draw_terminal(&mut terminal, &mut app).expect("initial headless draw succeeds");
        scheduler.mark_drawn(Duration::ZERO);
        assert_eq!(app.render_counters.draws, 1);
        assert_eq!(app.render_counters.transcript_turn_rebuilds, 1_000);
        assert!(app.view.transcript_rows_cache.turns[998].rows.len() >= 10_000);

        let build_counts = app
            .view
            .transcript_rows_cache
            .turns
            .iter()
            .map(|cached| cached.build_count)
            .collect::<Vec<_>>();
        let width = app.view.transcript_rows_cache.turns[0].width;
        for milliseconds in 1..=30_000 {
            let now = Duration::from_millis(milliseconds);
            scheduler.update_clock(now, false);
            assert!(!scheduler.is_due(now));
        }
        assert_eq!(scheduler.draws, 1);
        assert_eq!(app.render_counters.draws, 1);
        let unchanged_refresh = {
            let view = &mut app.view;
            let transcript = &view.transcript;
            view.transcript_rows_cache.refresh(transcript, width, false)
        };
        assert_eq!(unchanged_refresh, (0, 0));

        app.transcript[500].observe(&text_delta(2, 1, "changed row".to_string()));
        let changed_rebuilt_turns = {
            let view = &mut app.view;
            let transcript = &view.transcript;
            view.transcript_rows_cache
                .refresh(transcript, width, false)
                .0
        };
        assert_eq!(changed_rebuilt_turns, 1);
        for (index, cached) in app.view.transcript_rows_cache.turns.iter().enumerate() {
            assert_eq!(
                cached.build_count,
                build_counts[index] + u64::from(index == 500),
                "unexpected rebuild for turn {index}"
            );
        }
        drop(terminal);
        drop(app);
        std::fs::remove_dir_all(state_dir).expect("temporary test catalog is removed");
    }

    #[test]
    fn dual_stream_is_bounded_and_final_outcome_appears_on_first_due_draw() {
        let (mut app, state_dir) = test_app("stream-cache");
        app.view.state_key = "session-one".to_string();
        app.view.turn_status = TurnStatus::Running;
        let history_turns = (0..32)
            .map(|index| {
                let mut turn = TranscriptTurn::new(&format!("history {index}"));
                assert!(turn.observe(&text_delta(0, 0, "history reply".to_string())));
                turn
            })
            .collect::<Vec<_>>();
        let history_turn_count = history_turns.len();
        app.view.transcript = history_turns;
        app.view
            .transcript
            .push(TranscriptTurn::new("first stream"));
        let mut second_view = ConversationState::empty();
        second_view.state_key = "session-two".to_string();
        second_view.turn_status = TurnStatus::Running;
        second_view
            .transcript
            .push(TranscriptTurn::new("second stream"));
        app.views.insert("session-two".to_string(), second_view);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal opens");
        draw_terminal(&mut terminal, &mut app).expect("initial headless draw succeeds");
        let mut scheduler = RedrawScheduler::default();
        scheduler.mark_drawn(Duration::ZERO);
        let history_build_counts = app.view.transcript_rows_cache.turns[..history_turn_count]
            .iter()
            .map(|cached| cached.build_count)
            .collect::<Vec<_>>();

        let total_chunks = 12_000_u64;
        let mut next_chunk = 1_u64;
        let mut emitted = 0;
        let mut received = 0;
        let mut completion_received_at = None;
        let mut first_completion_draw = None;
        for milliseconds in 1..=30_034 {
            let now = Duration::from_millis(milliseconds);
            let mut received_turn_messages = 0;
            while next_chunk <= total_chunks && Duration::from_micros(next_chunk * 2_500) <= now {
                let owner = if next_chunk % 2 == 0 {
                    "session-two"
                } else {
                    "session-one"
                };
                app.turn_tx
                    .send(TurnMessage::Event {
                        owner: owner.to_string(),
                        event: text_delta(next_chunk, 0, "x".repeat(32)),
                    })
                    .expect("stream receiver remains connected");
                emitted += 1;
                next_chunk += 1;
            }
            if milliseconds % 30 == 0 {
                if milliseconds == 30_000 {
                    app.turn_tx
                        .send(TurnMessage::Finished {
                            owner: "session-one".to_string(),
                            result: Ok(TurnOutcome::Succeeded {
                                response: json!({"body": "final response"}),
                                capped: false,
                            }),
                        })
                        .expect("stream receiver remains connected");
                    emitted += 1;
                }
                app.begin_turn_message_cycle();
                let batch = app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
                assert!(batch <= MAX_TURN_MESSAGES_PER_CYCLE);
                received_turn_messages = batch;
                received += batch;
                if batch > 0 {
                    scheduler.request();
                }
                if milliseconds == 30_000 {
                    completion_received_at = Some(now);
                }
            }
            scheduler.update_clock(now, app.turn_status.is_active());
            if scheduler.is_due_after_batch(now, received_turn_messages) {
                assert!(
                    now.saturating_sub(scheduler.last_draw.expect("initial frame was drawn"))
                        >= MIN_RENDER_INTERVAL
                );
                draw_terminal(&mut terminal, &mut app)
                    .expect("headless stream frame draws successfully");
                if completion_received_at.is_some() && first_completion_draw.is_none() {
                    assert!(buffer_text(&terminal).contains("Outcome: succeeded"));
                    first_completion_draw = Some(now);
                }
                scheduler.mark_drawn(now);
            }
        }

        assert_eq!(emitted, total_chunks as usize + 1);
        assert_eq!(received, emitted);
        assert_eq!(app.render_counters.turn_messages, emitted as u64);
        assert_eq!(app.transcript.last().unwrap().render_revision(), 6_001);
        assert_eq!(
            app.views["session-two"].transcript[0].render_revision(),
            6_000
        );
        assert_eq!(app.render_counters.draws, scheduler.draws);
        assert!(app.render_counters.draws <= 900);
        for (index, cached) in app.view.transcript_rows_cache.turns[..history_turn_count]
            .iter()
            .enumerate()
        {
            assert_eq!(
                cached.build_count, history_build_counts[index],
                "unchanged offscreen turn {index} was rebuilt"
            );
        }
        let completion_received_at = completion_received_at.expect("completion was received");
        let first_completion_draw = first_completion_draw.expect("completion was drawn");
        assert!(first_completion_draw >= completion_received_at);
        assert!(first_completion_draw - completion_received_at <= Duration::from_millis(34));
        drop(terminal);
        drop(app);
        std::fs::remove_dir_all(state_dir).expect("temporary test catalog is removed");
    }

    #[test]
    fn each_message_receive_cycle_has_a_hard_bound_without_dropping_events() {
        let (mut app, state_dir) = test_app("message-bound");
        app.view.state_key = "session-one".to_string();
        app.view
            .transcript
            .push(TranscriptTurn::new("bounded stream"));
        for seq in 0..(MAX_TURN_MESSAGES_PER_CYCLE as u64 + 1) {
            app.turn_tx
                .send(TurnMessage::Event {
                    owner: "session-one".to_string(),
                    event: text_delta(seq, 0, "chunk".to_string()),
                })
                .expect("stream receiver remains connected");
        }
        app.begin_turn_message_cycle();
        assert_eq!(
            app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE),
            MAX_TURN_MESSAGES_PER_CYCLE
        );
        assert_eq!(app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE), 0);
        app.begin_turn_message_cycle();
        assert_eq!(app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE), 1);
        assert_eq!(
            app.transcript[0].render_revision(),
            MAX_TURN_MESSAGES_PER_CYCLE as u64 + 1
        );
        assert_eq!(
            app.render_counters.turn_messages,
            MAX_TURN_MESSAGES_PER_CYCLE as u64 + 1
        );
        drop(app);
        std::fs::remove_dir_all(state_dir).expect("temporary test catalog is removed");
    }

    #[test]
    fn queued_outcome_is_rendered_after_a_full_message_batch() {
        let (mut app, state_dir) = test_app("outcome-message-bound");
        app.view.turn_status = TurnStatus::Running;
        app.transcript.push(TranscriptTurn::new("bounded stream"));
        for seq in 0..MAX_TURN_MESSAGES_PER_CYCLE as u64 {
            app.turn_tx
                .send(TurnMessage::Event {
                    owner: app.state_key.clone(),
                    event: text_delta(seq, 0, "chunk".to_string()),
                })
                .expect("stream receiver remains connected");
        }
        app.turn_tx
            .send(TurnMessage::Finished {
                owner: app.state_key.clone(),
                result: Ok(TurnOutcome::Succeeded {
                    response: json!({"body": "final response"}),
                    capped: false,
                }),
            })
            .expect("stream receiver remains connected");

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal opens");
        let mut scheduler = RedrawScheduler::default();
        draw_terminal(&mut terminal, &mut app).expect("initial headless draw succeeds");
        scheduler.mark_drawn(Duration::ZERO);

        let now = Duration::from_millis(34);
        app.begin_turn_message_cycle();
        let first_batch = app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE / 2);
        assert_eq!(first_batch, MAX_TURN_MESSAGES_PER_CYCLE / 2);
        let second_batch = app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
        assert_eq!(second_batch, MAX_TURN_MESSAGES_PER_CYCLE / 2);
        assert_eq!(app.turn_messages_this_cycle, MAX_TURN_MESSAGES_PER_CYCLE);
        scheduler.request();
        assert!(scheduler.is_due(now));
        assert!(!scheduler.is_due_after_batch(now, app.turn_messages_this_cycle));
        assert!(!buffer_text(&terminal).contains("Outcome: succeeded"));

        app.begin_turn_message_cycle();
        let completion_batch = app.receive_turn_messages(MAX_TURN_MESSAGES_PER_CYCLE);
        assert_eq!(completion_batch, 1);
        scheduler.request();
        assert!(scheduler.is_due_after_batch(now, app.turn_messages_this_cycle));
        draw_terminal(&mut terminal, &mut app)
            .expect("completed state draws on the headless backend");
        scheduler.mark_drawn(now);

        assert!(buffer_text(&terminal).contains("Outcome: succeeded"));
        assert_eq!(
            app.render_counters.turn_messages,
            MAX_TURN_MESSAGES_PER_CYCLE as u64 + 1
        );
        drop(terminal);
        drop(app);
        std::fs::remove_dir_all(state_dir).expect("temporary test catalog is removed");
    }

    #[test]
    fn repeated_full_message_batches_do_not_starve_due_draws() {
        let mut scheduler = RedrawScheduler::default();
        scheduler.mark_drawn(Duration::ZERO);
        scheduler.request();

        let first_due_frame = Duration::from_millis(34);
        assert!(!scheduler.is_due_after_batch(first_due_frame, MAX_TURN_MESSAGES_PER_CYCLE));
        assert!(scheduler.is_due_after_batch(first_due_frame, MAX_TURN_MESSAGES_PER_CYCLE));
        scheduler.mark_drawn(first_due_frame);

        scheduler.request();
        let second_due_frame = first_due_frame + MIN_RENDER_INTERVAL;
        assert!(!scheduler.is_due_after_batch(second_due_frame, MAX_TURN_MESSAGES_PER_CYCLE));
        assert!(scheduler.is_due_after_batch(second_due_frame, MAX_TURN_MESSAGES_PER_CYCLE));
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::transcript::{TranscriptSourceId, TranscriptTurn};
    use leg_ui_client::StreamEvent;
    use serde_json::json;

    use super::{
        App, PaletteAction, SessionCatalog, SessionCatalogConfig, TranscriptAnchor, TurnStatus,
        build_transcript_rows, build_transcript_rows_with_focus, find_case_insensitive,
        find_transcript_anchor_row, response_stop_reason, restored_draft, same_tool_anchor,
        should_use_color, successful_status, transcript_page_down_start, transcript_page_up_start,
        truncate_to_width,
    };
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn metadata_truncation_preserves_graphemes_and_display_width() {
        assert_eq!(truncate_to_width("Alpha", 3), "Al…");
        assert_eq!(truncate_to_width("中👩‍👩‍👧‍👦z", 4), "中…");
        assert_eq!(truncate_to_width("🙂", 2), "🙂");
        assert_eq!(truncate_to_width("anything", 0), "");
        assert!(UnicodeWidthStr::width(truncate_to_width("中👩‍👩‍👧‍👦z", 4).as_str()) <= 4);
    }

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
    fn palette_stop_with_no_active_tui_run_never_exits() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let state_dir = std::env::temp_dir().join(format!(
            "leg-tui-palette-stop-{}-{unique}",
            std::process::id()
        ));
        let catalog = SessionCatalog::open(SessionCatalogConfig {
            state_dir: Some(state_dir.clone()),
            ..SessionCatalogConfig::default()
        })
        .expect("temporary test catalog opens");
        let mut app = App::new(catalog);

        app.invoke_palette_action(PaletteAction::Stop);

        assert!(!app.quit);
        assert!(!app.stop_chooser_open);
        assert_eq!(app.status, "No active TUI session to stop");
        drop(app);
        std::fs::remove_dir_all(state_dir).expect("temporary test catalog is removed");
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
    fn transcript_markdown_keeps_code_literal_and_malformed_source_readable() {
        let source = "## Heading\n- item with `inline`\n```rust\n  let value = `literal`;\n```\nopen `inline";
        let mut turn = TranscriptTurn::new("user question");
        turn.observe(&StreamEvent::TextDelta {
            seq: 1,
            round_index: 0,
            block_index: 0,
            text: source.to_string(),
        });
        let colored = build_transcript_rows(&[turn.clone()], 80, true)
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let monochrome = build_transcript_rows(&[turn], 80, false)
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(colored.contains("You: user question"));
        assert!(colored.contains("Assistant"));
        assert!(colored.contains("## Heading"));
        assert!(colored.contains("• item with inline"));
        assert!(colored.contains("Code block (rust):\n  let value = `literal`;"));
        assert!(colored.contains("open `inline"));
        assert!(monochrome.contains("`inline`"));
        assert!(monochrome.contains("Code block (rust):"));
    }

    #[test]
    fn tool_calls_render_as_single_elided_rows_and_focus_has_a_text_marker() {
        let mut turn = TranscriptTurn::new("tool summary fixture");
        turn.observe(&StreamEvent::ToolRound {
            seq: 1,
            round_index: 0,
            content: json!([
                {"type":"tool_use","id":"hidden-read-id","name":"read","input":{"path":"src/非常长的路径.rs","offset":24,"limit":80}},
                {"type":"tool_use","id":"hidden-bash-id","name":"bash","input":{"description":"inspect fixture output","command":"echo ignored"}},
                {"type":"tool_use","id":"hidden-unknown-id","name":"lookup","input":{"query":"界".repeat(200)}}
            ]),
        });
        let rows = build_transcript_rows(&[turn.clone()], 78, false)
            .into_iter()
            .filter(|row| matches!(&row.anchor.source_id, TranscriptSourceId::Tool { .. }))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            let text = row.line.to_string();
            assert!(!text.contains('\n'));
            assert!(UnicodeWidthStr::width(text.as_str()) <= 78);
            assert!(!text.contains("hidden-"));
        }
        assert!(
            rows[0]
                .line
                .to_string()
                .contains("read · pending · src/非常长的路径.rs")
        );
        assert!(
            rows[1]
                .line
                .to_string()
                .contains("bash · pending · inspect fixture output")
        );
        assert!(rows[2].line.to_string().ends_with('…'));

        let focused = build_transcript_rows_with_focus(&[turn], 78, false, Some(&rows[1].anchor));
        let focused = focused
            .iter()
            .find(|row| same_tool_anchor(&row.anchor, &rows[1].anchor))
            .expect("focused tool row");
        let focused_text = focused.line.to_string();
        assert!(
            focused_text.starts_with("› bash · pending"),
            "{focused_text}"
        );
        assert!(UnicodeWidthStr::width(focused_text.as_str()) <= 78);
    }

    #[test]
    fn supplied_edit_diff_lines_keep_distinct_styles_without_color() {
        let diff = "Success\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n context\n... [diff truncated: 2 more lines]";
        let colored = super::wrap_inspector_value(diff, 80, true, true);
        let removed = colored
            .iter()
            .find(|line| line.to_string() == "-old")
            .unwrap();
        let added = colored
            .iter()
            .find(|line| line.to_string() == "+new")
            .unwrap();
        let context = colored
            .iter()
            .find(|line| line.to_string() == " context")
            .unwrap();
        let truncated = colored
            .iter()
            .find(|line| line.to_string().starts_with("... [diff truncated:"))
            .unwrap();
        assert_eq!(removed.style.fg, Some(ratatui::style::Color::Red));
        assert_eq!(added.style.fg, Some(ratatui::style::Color::Green));
        assert_eq!(context.style.fg, Some(ratatui::style::Color::Blue));
        assert!(
            truncated
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );

        let monochrome = super::wrap_inspector_value(diff, 80, true, false);
        let removed = monochrome
            .iter()
            .find(|line| line.to_string() == "-old")
            .unwrap();
        let added = monochrome
            .iter()
            .find(|line| line.to_string() == "+new")
            .unwrap();
        assert_ne!(removed.style.add_modifier, added.style.add_modifier);
    }

    #[test]
    fn ten_thousand_line_transcript_pages_and_searches_by_source_row() {
        let mut source = (1..=10_000)
            .map(|line| format!("{line:05}: payload {line} stays in its source block"))
            .collect::<Vec<_>>()
            .join("\n");
        let family = "👩‍👩‍👧‍👦";
        source.push('\n');
        source.push_str(&"中".repeat(36));
        source.push_str(&family.repeat(24));

        let mut turn = TranscriptTurn::new("long deterministic answer");
        turn.observe(&StreamEvent::TextDelta {
            seq: 1,
            round_index: 0,
            block_index: 0,
            text: source.clone(),
        });
        let rows = build_transcript_rows(&[turn.clone()], 48, false);
        let rendered = rows
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("00001: payload 1"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("10000: payload 10000"))
        );
        assert!(rendered.iter().any(|line| line.contains(family)));

        let match_offset = find_case_insensitive(&source, "05000: payload").unwrap();
        let anchor = TranscriptAnchor {
            turn_index: 0,
            source_id: TranscriptSourceId::Assistant {
                round_index: 0,
                block_index: 0,
            },
            source_order: 1,
            byte_offset: match_offset,
        };
        let middle_row = find_transcript_anchor_row(&rows, &anchor).unwrap();
        assert!(rendered[middle_row].contains("05000: payload"));

        let resized_rows = build_transcript_rows(&[turn], 24, false);
        let resized_middle_row = find_transcript_anchor_row(&resized_rows, &anchor).unwrap();
        assert_eq!(
            resized_rows[resized_middle_row].anchor.byte_offset,
            match_offset
        );
        let resized_line = resized_rows[resized_middle_row].line.to_string();
        assert!(resized_line.contains("05000: payload"), "{resized_line}");

        let page_rows = 18;
        let max_scroll = rows.len().saturating_sub(page_rows);
        let mut start = transcript_page_up_start(max_scroll, max_scroll, page_rows, true);
        assert_eq!(start, max_scroll.saturating_sub(page_rows));
        while let Some(next) = transcript_page_down_start(start, max_scroll, page_rows) {
            assert!(next > start);
            start = next;
        }
        start = max_scroll;
        assert!(
            rendered[start..start + page_rows]
                .iter()
                .any(|line| line.contains("10000: payload 10000"))
        );
        let previous_page = transcript_page_up_start(start, max_scroll, page_rows, true);
        assert!(previous_page < start);
        assert_eq!(transcript_page_up_start(0, max_scroll, page_rows, false), 0);
    }

    #[test]
    fn transcript_anchor_selects_the_row_containing_search_and_resize_offsets() {
        let source = (0..40)
            .map(|word| format!("w{word:02}xx"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut turn = TranscriptTurn::new("anchor test");
        turn.observe(&StreamEvent::TextDelta {
            seq: 1,
            round_index: 0,
            block_index: 0,
            text: source.clone(),
        });
        let source_id = TranscriptSourceId::Assistant {
            round_index: 0,
            block_index: 0,
        };

        let search_offset = source.find("w02xx").unwrap();
        let search_anchor = TranscriptAnchor {
            turn_index: 0,
            source_id: source_id.clone(),
            source_order: 1,
            byte_offset: search_offset,
        };
        let search_rows = build_transcript_rows(&[turn.clone()], 20, false);
        let search_row = find_transcript_anchor_row(&search_rows, &search_anchor).unwrap();
        assert!(search_rows[search_row].line.to_string().contains("w02xx"));

        let old_width_rows = build_transcript_rows(&[turn.clone()], 16, false);
        let old_row_anchor = old_width_rows
            .iter()
            .map(|row| &row.anchor)
            .find(|row| {
                row.turn_index == 0
                    && row.source_id == source_id
                    && row.source_order == 1
                    && row.byte_offset == 16
            })
            .unwrap()
            .clone();
        let resized_rows = build_transcript_rows(&[turn], 26, false);
        let resized_row = find_transcript_anchor_row(&resized_rows, &old_row_anchor).unwrap();
        assert!(resized_rows[resized_row].line.to_string().contains("w03xx"));
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
