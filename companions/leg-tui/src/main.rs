mod editor;
mod sanitize;

use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

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
    CatalogTurn, SessionCatalog, SessionCatalogConfig, SessionInterface, StreamEvent, TurnOutcome,
    TurnStopHandle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

const WARNING: &str = "Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.";
const HELP: &str = "Enter newline | Ctrl-S send | ←/→ Home/End Backspace/Delete edit | Ctrl-Z undo | Ctrl-Y redo | F1 help | F2 actions | Ctrl-C stop/exit";

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
        terminal.draw(|frame| draw(frame, &app))?;
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
    transcript: Vec<(String, String)>,
    status: String,
    active_tool: Option<String>,
    active: bool,
    stopping: bool,
    active_prompt: Option<String>,
    active_draft_edited: bool,
    show_help: bool,
    show_menu: bool,
    quit: bool,
    model: String,
    stop_handle: Option<TurnStopHandle>,
    turn_tx: Sender<TurnMessage>,
    turn_rx: Receiver<TurnMessage>,
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
            active: false,
            stopping: false,
            active_prompt: None,
            active_draft_edited: false,
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
        match key.code {
            KeyCode::Esc => self.status = "Composer focused".to_string(),
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.show_menu = true,
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
        if self.screen != Screen::Conversation || self.show_help || self.show_menu {
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
        if self.active && changed {
            self.active_draft_edited = true;
        }
    }

    fn handle_ctrl_c(&mut self) {
        if self.active {
            if self.stopping {
                return;
            }
            match &self.stop_handle {
                Some(handle) => match handle.stop() {
                    Ok(()) => {
                        self.stopping = true;
                        self.status = "Stopping active turn…".to_string();
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
        if self.active {
            self.status = "Busy: wait for the active turn".to_string();
            return;
        }
        if self.composer.text().trim().is_empty() {
            self.status = "Blank prompts are not sent".to_string();
            return;
        }
        let Some(draft_id) = self.draft_id.as_deref() else {
            self.status = "Choose a workspace before sending".to_string();
            return;
        };
        let prompt = self.composer.text().to_string();
        let session_id = self.session_id.clone();
        let record_id = session_id.as_deref().unwrap_or(draft_id);
        if let Err(error) =
            self.catalog
                .save_draft(record_id, SessionInterface::Tui, prompt.clone())
        {
            self.status = format!("Could not save the prompt draft: {error}");
            return;
        }
        let started = match session_id {
            Some(session_id) => {
                self.catalog
                    .start_existing(&session_id, SessionInterface::Tui, prompt.clone())
            }
            None => self
                .catalog
                .start_new(draft_id, SessionInterface::Tui, prompt.clone()),
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
                self.transcript
                    .push(("You".to_string(), sanitize::terminal_safe_text(&prompt)));
                self.stop_handle = Some(stop_handle);
                self.active = true;
                self.stopping = false;
                self.active_prompt = Some(prompt);
                self.active_draft_edited = false;
                self.composer.clear();
                self.status = "Starting turn".to_string();
                self.active_tool = None;
            }
            Err(error) => {
                let _ = stop_handle.stop();
                self.status = format!("Could not observe the turn: {error}");
            }
        }
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
                self.status = "Running".to_string();
            }
            StreamEvent::TextDelta { text, .. } => {
                let safe = sanitize::terminal_safe_text(&text);
                if let Some((role, body)) = self.transcript.last_mut()
                    && role == "Assistant"
                {
                    body.push_str(&safe);
                } else {
                    self.transcript.push(("Assistant".to_string(), safe));
                }
                self.status = "Running".to_string();
            }
            StreamEvent::ToolCall { tool_name, .. } => {
                let name = sanitize::terminal_safe_text(&tool_name);
                self.active_tool = Some(name.clone());
                self.transcript
                    .push(("Activity".to_string(), format!("Tool running: {name}")));
                self.status = format!("Tool running: {name}");
            }
            StreamEvent::ToolResult {
                tool_name, status, ..
            } => {
                let name = sanitize::terminal_safe_text(&tool_name);
                let status = sanitize::terminal_safe_text(&status);
                self.active_tool = None;
                self.transcript
                    .push(("Activity".to_string(), format!("Tool {status}: {name}")));
                self.status = format!("Tool {status}: {name}");
            }
            StreamEvent::ToolRound { .. } => self.status = "Tool activity".to_string(),
            StreamEvent::TurnEnd { capped, .. } => {
                self.status = if capped {
                    "Turn reached its tool-round cap".to_string()
                } else {
                    "Finishing turn".to_string()
                };
            }
            StreamEvent::Unknown { .. } => {}
        }
    }

    fn finish_turn(&mut self, result: Result<TurnOutcome, String>) {
        let submitted_prompt = self.active_prompt.take();
        let draft_edited = self.active_draft_edited;
        self.active_draft_edited = false;
        self.active = false;
        self.stopping = false;
        self.stop_handle = None;
        self.active_tool = None;
        match result {
            Ok(TurnOutcome::Succeeded { .. }) => {
                self.status = "Succeeded".to_string();
            }
            Ok(TurnOutcome::Stopped { .. }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.status = "Stopped".to_string();
            }
            Ok(TurnOutcome::Failed { message, .. }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.status = format!("Failed: {}", sanitize::terminal_safe_text(&message));
            }
            Ok(TurnOutcome::Incomplete { message, forced }) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.status = format!(
                    "Incomplete{}: {}",
                    if forced { " (forced stop)" } else { "" },
                    sanitize::terminal_safe_text(&message)
                );
            }
            Err(message) => {
                self.restore_unedited_prompt(submitted_prompt, draft_edited);
                self.status = format!("Turn error: {}", sanitize::terminal_safe_text(&message));
            }
        }
        let _ = self.persist_draft();
    }

    fn restore_unedited_prompt(&mut self, prompt: Option<String>, draft_edited: bool) {
        if !draft_edited && let Some(prompt) = prompt {
            self.composer.set_text(prompt);
        }
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

fn draw(frame: &mut Frame<'_>, app: &App) {
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

fn draw_conversation(frame: &mut Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(4),
            Constraint::Length(4),
            Constraint::Length(1),
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
    let header = format!(
        "leg-tui  |  model: {}  |  status: {}{}\nworkspace: {workspace}",
        app.model, app.status, activity
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
            .map(|(role, text)| format!("{role}: {text}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    frame.render_widget(
        Paragraph::new(sanitize::terminal_safe_text(&transcript))
            .block(Block::default().borders(Borders::ALL).title("Conversation"))
            .wrap(Wrap { trim: false }),
        body[1],
    );

    let composer_title = if app.active {
        "Composer (busy)"
    } else {
        "Composer (Ctrl-S send)"
    };
    let composer_width = chunks[2].width.saturating_sub(2).max(1) as usize;
    let composer_height = chunks[2].height.saturating_sub(2).max(1) as usize;
    let (cursor_row, cursor_column) = app.composer.cursor_position(composer_width);
    let scroll = cursor_row.saturating_sub(composer_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(sanitize::terminal_safe_text(app.composer.text()))
            .block(Block::default().borders(Borders::ALL).title(composer_title))
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(HELP).style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );

    if app.show_help {
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
