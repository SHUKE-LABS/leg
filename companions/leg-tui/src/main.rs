mod editor;
mod sanitize;
mod transcript;

use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

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
    CatalogTurn, RetryIntent, SessionCatalog, SessionCatalogConfig, SessionInterface, StreamEvent,
    TurnOutcome, TurnStopHandle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use transcript::TranscriptTurn;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const WARNING: &str = "Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.";
const HELP: &str = "Enter newline | Ctrl-S send | PageUp/PageDown scroll | Ctrl-End newest\n←/→ Home/End edit | Ctrl-Z undo | Ctrl-Y redo | F1 help | F2 actions | Ctrl-C stop/exit";

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
    let _terminal_guard = TerminalGuard::enter()?;
    let catalog = SessionCatalog::open(SessionCatalogConfig {
        leg_bin: args.leg_bin,
        supervisor_bin: args.supervisor_bin,
        ..SessionCatalogConfig::default()
    })?;
    let mut app = App::new(catalog);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    while !app.quit {
        terminal.draw(|frame| draw(frame, &mut app))?;
        app.receive_turn_messages();
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Paste(text) => app.handle_paste(&text),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    app.persist_draft()?;
    Ok(())
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
}

enum TurnMessage {
    Event(StreamEvent),
    Finished(Result<TurnOutcome, String>),
}

struct App {
    catalog: SessionCatalog,
    screen: Screen,
    workspace_input: String,
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
    show_help: bool,
    show_menu: bool,
    quit: bool,
    model: String,
    stop_handle: Option<TurnStopHandle>,
    turn_tx: Sender<TurnMessage>,
    turn_rx: Receiver<TurnMessage>,
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
            workspace_input: String::new(),
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
            show_help: false,
            show_menu: false,
            quit: false,
            model: env::var("LEG_MODEL").unwrap_or_else(|_| "provider default".to_string()),
            stop_handle: None,
            turn_tx,
            turn_rx,
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
        let result = match self.draft_id.as_deref() {
            Some(draft_id) => self
                .catalog
                .set_workspace(draft_id, canonical.as_path())
                .and_then(|()| self.catalog.get(draft_id)),
            None => {
                self.catalog
                    .create_draft(SessionInterface::Tui, None, Some(canonical.as_path()))
            }
        };
        match result {
            Ok(session) => {
                self.workspace = session.cwd.clone();
                self.draft_id = Some(session.id);
                self.screen = Screen::Warning;
                self.status = "Review and acknowledge the first-run warning".to_string();
            }
            Err(error) => self.status = format!("Could not create a session draft: {error}"),
        }
    }

    fn handle_warning_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.screen = Screen::Conversation;
                self.status = "Ready".to_string();
            }
            KeyCode::Esc => {
                self.screen = Screen::Workspace;
                self.status = "Choose a workspace".to_string();
            }
            _ => {}
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
            KeyCode::Esc => self.status = "Composer focused".to_string(),
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.show_menu = true,
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
        self.quit = true;
    }

    fn submit(&mut self) {
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
        let Some(draft_id) = self.draft_id.as_deref() else {
            self.status = "Choose a workspace before sending".to_string();
            return;
        };
        let session_id = self.session_id.clone();
        let record_id = session_id.as_deref().unwrap_or(draft_id);
        if let Err(error) =
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
        match spawn_turn_reader(turn, self.turn_tx.clone()) {
            Ok(()) => {
                self.transcript.push(TranscriptTurn::new(&prompt));
                self.note_transcript_change();
                self.stop_handle = Some(stop_handle);
                self.turn_status = TurnStatus::Starting;
                self.started_at = Some(Instant::now());
                self.last_elapsed = Duration::ZERO;
                self.terminal_detail = None;
                self.active_prompt = Some(prompt);
                self.active_draft_edited = false;
                self.composer.clear();
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
                Ok(TurnMessage::Event(event)) => self.handle_stream_event(event),
                Ok(TurnMessage::Finished(result)) => self.finish_turn(result),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
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
                ..
            } => {
                self.model = format!("{provider}/{model}");
                if session_id.is_some() {
                    self.session_id = session_id;
                }
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
            }
            Ok(TurnOutcome::Stopped { .. }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Interrupted;
                self.terminal_detail = Some("Turn interrupted gracefully.".to_string());
            }
            Ok(TurnOutcome::Failed { message, .. }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Failed;
                self.terminal_detail = Some(sanitize::terminal_safe_text(&message));
            }
            Ok(TurnOutcome::Incomplete { message, forced }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Incomplete { forced };
                self.terminal_detail = Some(sanitize::terminal_safe_text(&message));
            }
            Err(message) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.turn_status = TurnStatus::Incomplete { forced: false };
                self.terminal_detail = Some(sanitize::terminal_safe_text(&message));
            }
        }
        let _ = self.persist_draft();
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
}

fn spawn_turn_reader(mut turn: CatalogTurn, tx: Sender<TurnMessage>) -> io::Result<()> {
    thread::Builder::new()
        .name("leg-tui-turn-reader".to_string())
        .spawn(move || {
            let result = loop {
                match turn.observe() {
                    Ok(Some(event)) => {
                        if tx.send(TurnMessage::Event(event)).is_err() {
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
            let _ = tx.send(TurnMessage::Finished(result));
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

fn restored_draft(current: &str, submitted: Option<String>, draft_edited: bool) -> String {
    if draft_edited {
        current.to_string()
    } else {
        submitted.unwrap_or_else(|| current.to_string())
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    match app.screen {
        Screen::Workspace => draw_workspace(frame, app),
        Screen::Warning => draw_warning(frame),
        Screen::Conversation => draw_conversation(frame, app),
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

fn draw_warning(frame: &mut Frame<'_>) {
    let area = centered_rect(96, 46, frame.area());
    let lines = vec![
        Line::from(WARNING).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("Press Enter to acknowledge and open the composer."),
        Line::from("Press Esc to choose another workspace."),
        Line::from(""),
        Line::from(
            "Composer: Enter newline; Ctrl-S send; arrows, Home/End, Backspace/Delete edit.",
        ),
        Line::from("Ctrl-Z undo; Ctrl-Y redo. Ctrl-C stops a turn or exits when idle."),
        Line::from("F1 help; F2 keyboard actions; Esc closes overlays. ? is prompt text."),
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

    let workspace = app
        .workspace
        .as_deref()
        .map(Path::display)
        .map(|path| path.to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let activity = app
        .active_tool
        .as_deref()
        .map(|tool| format!("  |  tool: {tool}"))
        .unwrap_or_default();
    let elapsed = format_duration(app.elapsed());
    let detail = app
        .terminal_detail
        .as_deref()
        .or_else(|| (!app.status.is_empty()).then_some(app.status.as_str()))
        .unwrap_or_default();
    let header = format!(
        "leg-tui  |  model: {}  |  status: {}  |  elapsed: {elapsed}{activity}\nworkspace: {workspace}\n{detail}",
        app.model,
        app.turn_status.display(),
    );
    frame.render_widget(
        Paragraph::new(sanitize::terminal_safe_text(&header))
            .block(Block::default().borders(Borders::ALL))
            .style(Style::default().fg(Color::Cyan)),
        chunks[0],
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(1)])
        .split(chunks[1]);
    let session_label = app
        .session_id
        .as_deref()
        .or(app.draft_id.as_deref())
        .unwrap_or("new session");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Active"),
            Line::from(sanitize::terminal_safe_text(session_label)),
            Line::from(""),
            Line::from("No session browsing"),
        ])
        .block(Block::default().borders(Borders::ALL).title("Sessions"))
        .wrap(Wrap { trim: false }),
        body[0],
    );

    let transcript = if app.transcript.is_empty() {
        "Choose a workspace, acknowledge the warning, then send a prompt with Ctrl-S.".to_string()
    } else {
        app.transcript
            .iter()
            .map(|turn| turn.lines().join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let transcript_width = body[1].width.saturating_sub(2).max(1);
    let transcript_height = body[1].height.saturating_sub(2).max(1) as usize;
    let wrapped_transcript = wrap_transcript(
        &sanitize::terminal_safe_text(&transcript),
        transcript_width as usize,
    );
    let visual_lines = wrapped_transcript.len();
    app.transcript_max_scroll = visual_lines.saturating_sub(transcript_height);
    app.transcript_page_rows = transcript_height.max(1);
    if app.transcript_follow_tail {
        app.transcript_scroll = app.transcript_max_scroll;
    } else {
        app.transcript_scroll = app.transcript_scroll.min(app.transcript_max_scroll);
    }
    let transcript_title = if app.transcript_new_content {
        "Conversation · new content below (Ctrl-End)"
    } else {
        "Conversation"
    };
    let transcript_paragraph = Paragraph::new(wrapped_transcript)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(transcript_title),
        )
        .scroll((app.transcript_scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(transcript_paragraph, body[1]);

    let composer_title = if app.turn_status.is_active() {
        "Composer (busy)"
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
    let scroll = cursor_row.saturating_sub(composer_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(composer_text)
            .block(Block::default().borders(Borders::ALL).title(composer_title))
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(HELP).style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );

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
        let area = centered_rect(62, 40, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Keyboard help"),
                Line::from("Enter inserts a newline; Ctrl-S sends a nonblank prompt."),
                Line::from("Arrows, Home/End and Backspace/Delete edit by grapheme."),
                Line::from("Ctrl-Z undoes; Ctrl-Y redoes. F2 opens keyboard actions."),
                Line::from("Ctrl-C stops an active turn; when idle it exits and keeps the draft."),
                Line::from("Bracketed paste inserts Unicode and lines as one undoable edit."),
                Line::from("PageUp/PageDown scroll history; Ctrl-End returns to newest text."),
                Line::from("? is prompt text. Esc closes help and restores composer focus."),
            ])
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .wrap(Wrap { trim: false }),
            area,
        );
    } else if app.show_menu {
        let area = centered_rect(70, 52, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Keyboard actions"),
                Line::from("Enter     Insert a newline"),
                Line::from("Ctrl-S    Send the complete nonblank prompt"),
                Line::from("←/→      Move by grapheme; Home/End move within this line"),
                Line::from("Backspace/Delete  Remove one grapheme"),
                Line::from("Ctrl-Z / Ctrl-Y  Undo / redo"),
                Line::from("Ctrl-C    Stop the turn, or exit when idle"),
                Line::from("PageUp/PageDown  Scroll transcript; Ctrl-End  Newest text"),
                Line::from("F1 / F2   Help / keyboard actions"),
                Line::from("? is ordinary prompt text. Esc closes this menu."),
            ])
            .block(Block::default().borders(Borders::ALL).title("Actions"))
            .wrap(Wrap { trim: false }),
            area,
        );
    } else {
        let visible_row = cursor_row.saturating_sub(scroll);
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
    use std::time::Duration;

    use serde_json::json;

    use super::{TurnStatus, response_stop_reason, restored_draft, successful_status};

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
    fn elapsed_time_is_rendered_as_minutes_and_seconds() {
        assert_eq!(super::format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(super::format_duration(Duration::from_secs(83)), "1:23");
    }
}
