//! The command-line entry surface.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use crate::config::LegConfig;
use crate::error::{LegError, Result};
use crate::events::{
    EventSink, Exchange, ExchangeEvent, ExchangeMeta, NoopSink, Outcome, ToolStatus, WriterSink,
    now_ms, trail_content,
};
use crate::interrupt;
use crate::message::{MessageEnvelope, MessageKind};
use crate::model::{ContentBlock, Conversation, Message, Role, StopReason};
use crate::participant::{LocalParticipant, Participant, fresh_message_id};
use crate::tools::{
    BashTool, EditTool, ReadSet, ReadTool, ToolLoop, ToolObserver, ToolRegistry, TurnOutcome,
    WriteTool, tool_round_limit_warning,
};
use crate::transport::claude::ClaudeClient;
use crate::transport::http::UreqHttpClient;
use crate::transport::{RetryingHttpClient, Transport, TransportCall};

/// The one-line usage summary, shared by `--help` output and usage errors.
const USAGE: &str = "usage: leg [--version|-V] [--help|-h] | leg ask [--model <model>] [--image <path> ...] <prompt> | leg session [--resume <file> [--session <id>]] | leg log show [--file <path>] | leg log replay [--file <path>] [--index <N>] | leg exchange [--in <path>] [--out <path>] [--session <id>|--new-session] [--session-id-out <path>]";

/// Name of the environment variable naming the JSONL exchange trail to append
/// to. An unset or blank value disables recording for `ask`, cold `exchange`,
/// and fresh `session` runs. Named exchange sessions always append to their
/// own trail and also write here when configured; `--resume` appends to the
/// trail it read from.
const EVENT_LOG_ENV: &str = "LEG_EVENT_LOG";

/// The in-session command that ends the REPL cleanly (alongside EOF).
const SESSION_EXIT_COMMAND: &str = "/exit";

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Prints the crate version.
    Version,
    /// Prints usage help.
    Help,
    /// Runs one single-turn provider exchange.
    Ask {
        /// The user prompt text.
        prompt: String,
        /// `--model` override, replacing `LEG_MODEL`/the configured default.
        model: Option<String>,
        /// Repeatable image file paths to attach to the user turn.
        images: Vec<String>,
    },
    /// Runs an interactive multi-turn REPL, accumulating history on disk.
    Session {
        /// `--resume <file> [--session <id>]`; `None` starts a fresh session.
        resume: Option<ResumeArgs>,
    },
    /// Prints every complete exchange in a JSONL trail.
    LogShow {
        /// `--file <path>`; falls back to [`EVENT_LOG_ENV`] when absent.
        file: Option<String>,
    },
    /// Re-runs one logged exchange's user content against today's provider config.
    LogReplay {
        /// `--file <path>`; falls back to [`EVENT_LOG_ENV`] when absent.
        file: Option<String>,
        /// 1-based `--index`; the last exchange when absent.
        index: Option<usize>,
    },
    /// Runs one `baton.message/v1` request/response round-trip.
    Exchange {
        /// `--in <path>`; falls back to stdin when absent.
        in_path: Option<String>,
        /// `--out <path>`; falls back to stdout when absent.
        out_path: Option<String>,
        /// Existing named session, or a newly created session.
        session: Option<ExchangeSession>,
        /// Writes the selected session id after the turn.
        session_id_out: Option<String>,
    },
}

/// The session mode selected for one headless exchange.
#[derive(Debug, PartialEq, Eq)]
enum ExchangeSession {
    /// Continue the session stored under this id.
    Existing(String),
    /// Create a new session with a generated id.
    New,
}

/// Selects the session trail to rehydrate for `leg session --resume`.
#[derive(Debug, PartialEq, Eq)]
struct ResumeArgs {
    /// The JSONL session trail to read the prior turns from.
    file: String,
    /// The `session_id` to select; `None` selects the sole session in the
    /// file (an error when the file holds zero or more than one).
    session_id: Option<String>,
}

/// Process entry point: parse arguments and dispatch.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args)? {
        None => Ok(()),
        Some(Command::Version) => {
            println!("leg {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Command::Help) => {
            println!("{}", help_text());
            Ok(())
        }
        Some(Command::Ask {
            prompt,
            model,
            images,
        }) => {
            interrupt::install()?;
            let stdout = std::io::stdout();
            if images.is_empty() {
                execute_ask(&prompt, model, stdout.lock())
            } else {
                execute_ask_with_images(&prompt, model, &images, stdout.lock())
            }
        }
        Some(Command::Session { resume }) => {
            interrupt::install()?;
            #[cfg(unix)]
            let input = interrupt::SignalAwareStdin::stdin();
            #[cfg(not(unix))]
            let stdin = std::io::stdin();
            #[cfg(not(unix))]
            let input = stdin.lock();
            let stdout = std::io::stdout();
            match resume {
                None => {
                    let config = LegConfig::from_env()?;
                    let meta = exchange_meta(&config);
                    let client = build_transport(config);
                    let mut sink = open_event_sink();
                    let session_id = new_session_id();
                    execute_session(
                        &client,
                        sink.as_mut(),
                        &meta,
                        input,
                        stdout.lock(),
                        session_id,
                    )
                }
                Some(args) => {
                    let config = LegConfig::from_env()?;
                    let meta = exchange_meta(&config);
                    let client = build_transport(config);
                    // Resume: load + select the prior session *before* opening
                    // any sink, so a bad selection (missing id, empty/
                    // ambiguous trail) exits non-zero having written nothing.
                    let resumed = load_resume(&args.file, args.session_id.as_deref())?;
                    let mut sink = open_append_sink(&args.file);
                    execute_session_resumed(
                        &client,
                        sink.as_mut(),
                        &meta,
                        input,
                        stdout.lock(),
                        resumed,
                    )
                }
            }
        }
        Some(Command::LogShow { file }) => {
            let report = read_log(file.as_deref())?;
            let stdout = std::io::stdout();
            execute_log_show(&report, stdout.lock())
        }
        Some(Command::LogReplay { file, index }) => {
            let report = read_log(file.as_deref())?;
            let (config, prompt, content) = replay_target(&report, index, LegConfig::from_env)?;

            let stdout = std::io::stdout();
            execute_ask_with_content(config, &prompt, &content, stdout.lock())
        }
        Some(Command::Exchange {
            in_path,
            out_path,
            session,
            session_id_out,
        }) => {
            interrupt::install()?;
            execute_exchange(
                in_path.as_deref(),
                out_path.as_deref(),
                session,
                session_id_out.as_deref(),
            )
        }
    }
}

/// The full `--help` body: the provider environment variables and trail behavior.
fn help_text() -> String {
    format!(
        "{USAGE}\n\n\
         Reads credentials from ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN /\n\
         CLAUDE_CODE_OAUTH_TOKEN). ANTHROPIC_AUTH_TOKEN is for bearer keys of\n\
         Anthropic-compatible endpoints. Claude subscription OAuth tokens\n\
         (including `claude setup-token`) are unsupported outside Claude Code;\n\
         use an Anthropic Console API key with ANTHROPIC_API_KEY instead.\n\
         `leg ask --image <path>` accepts JPEG, PNG, GIF, and WebP; repeat the\n\
         flag to attach multiple images. Each base64 image is limited to 10 MB,\n\
         and image requests to 32 MB.\n\
         Also honours ANTHROPIC_BASE_URL, LEG_MODEL, LEG_TIMEOUT_SECS,\n\
         LEG_BASH_TIMEOUT_SECS, LEG_MAX_TOKENS, LEG_MAX_TOOL_ROUNDS,\n\
         LEG_MAX_RETRIES, LEG_RETRY_BASE_DELAY_MS,\n\
         LEG_PRETOOL_HOOK, and LEG_SYSTEM_PROMPT.\n\n\
         LEG_EVENT_LOG names an optional JSONL trail for `ask`, cold `exchange`,\n\
         and a fresh `session`; named exchange sessions always write their\n\
         session store and also append here when this variable is non-blank.\n\
         `leg log show`/`leg log replay` read it back (or `--file <path>`).\n\n\
         `leg exchange` is the headless entry point for adapters; it reads a\n\
         `baton.message/v1` envelope on --in/stdin and writes the response\n\
         envelope on --out/stdout; given plain text instead, it writes just\n\
         the reply body. Provider/delivery failures exit non-zero: `ask` and\n\
         plain-text `exchange` leave stdout empty and report an error on\n\
         stderr; envelope `exchange` writes its `kind:\"error\"` response\n\
         before reporting the failure. `--session <id>` continues a named\n\
         exchange session; `--new-session` creates one. These flags are\n\
         mutually exclusive, and `--session-id-out <path>` writes the id and a\n\
         newline after\n\
         the turn when either is used. The session store is\n\
         LEG_SESSION_DIR, else XDG_STATE_HOME/leg/sessions, else\n\
         ~/.local/state/leg/sessions. `baton serve --agent-cmd <path>\n\
         --agent-arg exchange` expects this protocol."
    )
}

/// Parses `args` into a [`Command`]. `None` means "do nothing" (no arguments),
/// matching leg#1's original no-op skeleton behaviour.
fn parse_args(args: &[String]) -> Result<Option<Command>> {
    let mut iter = args.iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };

    match first.as_str() {
        "--version" | "-V" => Ok(Some(Command::Version)),
        "--help" | "-h" => Ok(Some(Command::Help)),
        "ask" => parse_ask(iter).map(Some),
        "session" => parse_session(iter).map(Some),
        "log" => parse_log(iter).map(Some),
        "exchange" => parse_exchange(iter).map(Some),
        other => Err(LegError::Usage(format!(
            "unrecognised argument {other:?}; {USAGE}"
        ))),
    }
}

/// Parses the arguments following `ask`: optional `--model <value>` and
/// repeatable `--image <path>` flags in any position, plus one non-blank
/// positional prompt.
fn parse_ask<'a>(iter: impl Iterator<Item = &'a String>) -> Result<Command> {
    let mut model = None;
    let mut prompt = None;
    let mut images = Vec::new();

    let mut iter = iter.peekable();
    while let Some(arg) = iter.next() {
        if arg == "--model" {
            let value = iter
                .next()
                .ok_or_else(|| LegError::Usage("--model requires a value".to_string()))?;
            model = Some(value.clone());
        } else if arg == "--image" {
            let value = iter
                .next()
                .ok_or_else(|| LegError::Usage("--image requires a path".to_string()))?;
            if value.is_empty() {
                return Err(LegError::Usage(
                    "--image path must not be empty".to_string(),
                ));
            }
            images.push(value.clone());
        } else if prompt.is_some() {
            return Err(LegError::Usage(format!(
                "unexpected extra argument {arg:?}; ask takes exactly one prompt"
            )));
        } else {
            prompt = Some(arg.clone());
        }
    }

    let prompt = prompt.ok_or_else(|| LegError::Usage("ask requires a prompt".to_string()))?;
    if prompt.trim().is_empty() {
        return Err(LegError::Usage(
            "ask's prompt must not be blank".to_string(),
        ));
    }

    Ok(Command::Ask {
        prompt,
        model,
        images,
    })
}

/// Parses the arguments following `session`: optional `--resume <file>` and
/// `--session <id>`. `--session` without `--resume` is a usage error.
fn parse_session<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<Command> {
    let mut file: Option<String> = None;
    let mut session_id: Option<String> = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--resume" => {
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--resume requires a value".to_string()))?;
                file = Some(value.clone());
            }
            "--session" => {
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--session requires a value".to_string()))?;
                session_id = Some(value.clone());
            }
            other => {
                return Err(LegError::Usage(format!(
                    "unexpected argument {other:?}; {USAGE}"
                )));
            }
        }
    }

    match file {
        Some(file) => Ok(Command::Session {
            resume: Some(ResumeArgs { file, session_id }),
        }),
        None if session_id.is_some() => Err(LegError::Usage(
            "--session requires --resume <file>".to_string(),
        )),
        None => Ok(Command::Session { resume: None }),
    }
}

/// Parses the arguments following `log`: the `show`/`replay` subcommand plus
/// its options.
fn parse_log<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<Command> {
    let mode = iter
        .next()
        .ok_or_else(|| LegError::Usage("log requires a subcommand: show or replay".to_string()))?;
    match mode.as_str() {
        "show" => {
            let opts = parse_log_options(iter, false)?;
            Ok(Command::LogShow { file: opts.file })
        }
        "replay" => {
            let opts = parse_log_options(iter, true)?;
            Ok(Command::LogReplay {
                file: opts.file,
                index: opts.index,
            })
        }
        other => Err(LegError::Usage(format!("unknown log subcommand {other:?}"))),
    }
}

/// Parsed options shared by `log show` / `log replay`.
struct LogOptions {
    file: Option<String>,
    index: Option<usize>,
}

/// Parses `--file <path>` (both subcommands) and, when `allow_index` is set,
/// `--index <N>` (replay only). `--index` on `show`, an unknown flag, or a
/// non-positive-integer index are all usage errors.
fn parse_log_options<'a>(
    mut iter: impl Iterator<Item = &'a String>,
    allow_index: bool,
) -> Result<LogOptions> {
    let mut file: Option<String> = None;
    let mut index: Option<usize> = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--file" => {
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--file requires a value".to_string()))?;
                file = Some(value.clone());
            }
            "--index" if allow_index => {
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--index requires a value".to_string()))?;
                index = Some(parse_index(value)?);
            }
            other => {
                return Err(LegError::Usage(format!("unexpected argument {other:?}")));
            }
        }
    }

    Ok(LogOptions { file, index })
}

/// Parses a 1-based `--index` value: a positive integer. Zero and non-numeric
/// values are usage errors (the range itself is validated against the log
/// later).
fn parse_index(raw: &str) -> Result<usize> {
    let parsed = raw
        .parse::<usize>()
        .map_err(|_| LegError::Usage(format!("--index must be a positive integer, got {raw:?}")))?;
    if parsed == 0 {
        return Err(LegError::Usage(
            "--index is 1-based; 0 is not a valid exchange".to_string(),
        ));
    }
    Ok(parsed)
}

/// Parses the arguments following `exchange`: optional `--in <path>` and
/// `--out <path>`. Any other token is a usage error.
fn parse_exchange<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<Command> {
    let mut in_path: Option<String> = None;
    let mut out_path: Option<String> = None;
    let mut session: Option<ExchangeSession> = None;
    let mut session_id_out: Option<String> = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--in" => {
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--in requires a value".to_string()))?;
                in_path = Some(value.clone());
            }
            "--out" => {
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--out requires a value".to_string()))?;
                out_path = Some(value.clone());
            }
            "--session" => {
                if session.is_some() {
                    return Err(LegError::Usage(
                        "exchange accepts only one of --session and --new-session".to_string(),
                    ));
                }
                let value = iter
                    .next()
                    .ok_or_else(|| LegError::Usage("--session requires a value".to_string()))?;
                if matches!(
                    value.as_str(),
                    "--in" | "--out" | "--session" | "--new-session" | "--session-id-out"
                ) {
                    return Err(LegError::Usage("--session requires a value".to_string()));
                }
                session = Some(ExchangeSession::Existing(value.clone()));
            }
            "--new-session" => {
                if session.is_some() {
                    return Err(LegError::Usage(
                        "exchange accepts only one of --session and --new-session".to_string(),
                    ));
                }
                session = Some(ExchangeSession::New);
            }
            "--session-id-out" => {
                let value = iter.next().ok_or_else(|| {
                    LegError::Usage("--session-id-out requires a value".to_string())
                })?;
                session_id_out = Some(value.clone());
            }
            other => {
                return Err(LegError::Usage(format!("unexpected argument {other:?}")));
            }
        }
    }

    if session_id_out.is_some() && session.is_none() {
        return Err(LegError::Usage(
            "--session-id-out requires --session or --new-session".to_string(),
        ));
    }

    Ok(Command::Exchange {
        in_path,
        out_path,
        session,
        session_id_out,
    })
}

/// Which protocol `leg exchange`'s input was, and therefore what shape its
/// output takes.
#[derive(Debug, PartialEq, Eq)]
enum ExchangeMode {
    /// The input parsed as a whole `baton.message/v1` envelope — mirrors
    /// `baton exchange`'s own file/pipe contract. The output is always the
    /// full response envelope, whatever `kind` it carries.
    Envelope,
    /// The input was plain text — the shape `baton serve --agent-cmd` feeds
    /// on stdin. The output is just the reply body on success; on a
    /// delivered `kind: "error"` response, nothing is written to the
    /// response output (see [`execute_exchange`]), which is what lets
    /// baton's own `ExternalAgentParticipant` machinery-error path take
    /// over.
    PlainText,
}

/// Builds the request `leg exchange` will answer, and the [`ExchangeMode`]
/// that determines how the response is written.
///
/// Tries to parse the whole input as a `MessageEnvelope` first; a prompt
/// landing on this accidentally is not a realistic risk, since the struct
/// requires an exact set of fields (`schema`, `message_id`,
/// `conversation_id`, `from`, `to`, one of five fixed `kind` strings, `body`,
/// `ts_ms`). A parse failure falls back to treating the whole input as the
/// prompt body, trimmed only of a trailing `\r`/`\n` so intentional trailing
/// spaces/tabs survive.
fn parse_exchange_request(raw: &str) -> (MessageEnvelope, ExchangeMode) {
    if let Ok(envelope) = serde_json::from_str::<MessageEnvelope>(raw) {
        return (envelope, ExchangeMode::Envelope);
    }
    let body = raw.trim_end_matches(['\r', '\n']);
    let envelope = MessageEnvelope::new(
        "exchange-1",
        "exchange",
        "external",
        "leg",
        MessageKind::Request,
        body,
        now_ms(),
    );
    (envelope, ExchangeMode::PlainText)
}

/// Runs one `leg exchange` request/response round-trip: reads `in_path`
/// (stdin when absent), and writes to `out_path` (stdout when absent).
///
/// Config-load, `--in`/`--out` I/O, and delivered turn failures propagate as
/// `Err`; see [`execute_exchange_core`] for the per-mode failure output.
fn execute_exchange(
    in_path: Option<&str>,
    out_path: Option<&str>,
    session: Option<ExchangeSession>,
    session_id_out: Option<&str>,
) -> Result<()> {
    if let Some(session) = session {
        return execute_exchange_session(in_path, out_path, session, session_id_out);
    }

    let config = LegConfig::from_env()?;
    let meta = exchange_meta(&config);

    let raw = read_exchange_input(in_path)?;

    let output = open_output(out_path)?;
    let mut sink = Rc::new(RefCell::new(open_event_sink()));
    let transport = build_transport(config).with_observer(tool_trail_observer(sink.clone()));
    let participant = LocalParticipant::new(transport, meta.clone());
    execute_exchange_core(&participant, &meta, &raw, output, &mut sink)
}

/// Runs one exchange against a named session, restoring its conversation and
/// appending the turn to that session's JSONL trail.
fn execute_exchange_session(
    in_path: Option<&str>,
    out_path: Option<&str>,
    session: ExchangeSession,
    session_id_out: Option<&str>,
) -> Result<()> {
    let store_dir = session_store_dir()?;
    let (mut resumed, session_path, create_new) = match session {
        ExchangeSession::Existing(session_id) => {
            let path = exchange_session_path(&store_dir, &session_id)
                .ok_or_else(|| LegError::SessionNotFound(session_id.clone()))?;
            let resumed = load_exchange_session(&path, &session_id)?;
            (resumed, path, false)
        }
        ExchangeSession::New => {
            let session_id = new_session_id();
            let path = exchange_session_path(&store_dir, &session_id)
                .ok_or_else(|| LegError::Config("generated an invalid session id".to_string()))?;
            (
                ResumedSession {
                    session_id,
                    conversation: Conversation::new(),
                    prior_turns: 0,
                    next_turn_index: 0,
                },
                path,
                true,
            )
        }
    };

    let config = LegConfig::from_env()?;
    let meta = exchange_meta(&config);
    let raw = read_exchange_input(in_path)?;

    let mut output = open_output(out_path)?;
    if create_new {
        std::fs::create_dir_all(&store_dir).map_err(|err| {
            LegError::Io(format!(
                "failed to create session store {:?}: {err}",
                store_dir
            ))
        })?;
    }

    let SessionEventSink {
        sink: event_sink,
        session_write_error,
    } = open_session_event_sink(&session_path, create_new)?;
    let mut sink = Rc::new(RefCell::new(event_sink));
    let transport = build_transport(config);
    let mut response_output = Vec::new();
    let result = execute_exchange_session_core(
        &transport,
        &meta,
        &raw,
        &mut response_output,
        &mut sink,
        &mut resumed,
    );
    let session_write_error = session_write_error.borrow().clone();

    if let Some(path) = session_id_out {
        write_session_id_out(path, &resumed.session_id)?;
    }
    interrupt::check()?;
    output.write_all(&response_output).map_err(io_err)?;
    match (result, session_write_error) {
        (Ok(()), Some(error)) => Err(LegError::Io(format!(
            "failed to record session trail: {error}"
        ))),
        (result, _) => result,
    }
}

/// Testable core of [`execute_exchange`], parameterised over a [`Participant`]
/// so the per-mode output contract is exercisable without a network.
///
/// - [`ExchangeMode::Envelope`]: writes the full response envelope as one
///   JSON line; a `kind: "error"` response is then returned as a failure
///   (mirrors `baton exchange`'s output shape).
/// - [`ExchangeMode::PlainText`] + [`MessageKind::Response`]: writes just
///   `response.body`.
/// - [`ExchangeMode::PlainText`] + [`MessageKind::Error`]: writes nothing to
///   `output` and returns a failure for `main` to report on stderr.
///
/// Successful responses return `Ok(())`. A delivered error returns `Err`
/// after preserving the mode-specific output.
fn execute_exchange_core(
    participant: &impl Participant,
    meta: &ExchangeMeta,
    raw: &str,
    output: impl Write,
    sink: &mut dyn EventSink,
) -> Result<()> {
    let (request, mode) = parse_exchange_request(raw);
    let content = [ContentBlock::text(request.body.clone())];
    let response = respond_with_trail(meta, &request, &content, sink, |envelope| {
        participant.respond(envelope)
    })?;

    write_exchange_response(mode, &response, output)
}

/// Session-backed counterpart to [`execute_exchange_core`].
fn execute_exchange_session_core(
    transport: &ToolLoop<impl Transport>,
    meta: &ExchangeMeta,
    raw: &str,
    output: impl Write,
    sink: &mut dyn EventSink,
    resumed: &mut ResumedSession,
) -> Result<()> {
    let (request, mode) = parse_exchange_request(raw);
    let session_id = resumed.session_id.clone();
    let turn_index = resumed.next_turn_index;
    let request_ts_ms = now_ms();
    resumed.conversation.push_user(request.body.as_str());

    let stderr = std::io::stderr();
    let mut warning = stderr.lock();
    let call_start = Instant::now();
    let call = timed_session_exchange(
        sink,
        TimedSessionExchangeContext {
            meta,
            prompt: &request.body,
            session_id: &session_id,
            turn_index,
            max_tool_rounds: transport.max_tool_rounds(),
        },
        &mut warning,
        |sink| {
            transport.run_observed_with_attempts(resumed.conversation.messages(), &mut |event| {
                let turn = Some((session_id.as_str(), turn_index));
                emit(sink, &ExchangeEvent::from_tool_event(now_ms(), event, turn));
            })
        },
    );
    let attempts = Some(call.attempts);
    let duration_ms = call_start.elapsed().as_millis() as u64;
    resumed.next_turn_index += 1;

    let outcome_ts_ms = now_ms();
    let (kind, body, outcome) = match call.result {
        Ok(turn) => {
            for message in turn.transcript {
                resumed.conversation.push(message);
            }
            resumed.conversation.push(session_reply_message(
                turn.reply.content.clone(),
                turn.capped,
            ));
            let outcome = Outcome::Ok {
                ts_ms: outcome_ts_ms,
                duration_ms,
                reply: turn.reply.text.clone(),
                content: trail_content(&turn.reply.content),
                input_tokens: turn.reply.usage.input_tokens,
                output_tokens: turn.reply.usage.output_tokens,
                stop_reason: turn
                    .reply
                    .stop_reason
                    .as_ref()
                    .map(|reason| reason.as_str().to_string()),
                attempts,
                session_id: Some(session_id.clone()),
                turn_index: Some(turn_index),
            };
            (MessageKind::Response, turn.reply.text, outcome)
        }
        Err(err) => {
            resumed.conversation.pop();
            let body = err.to_string();
            let outcome = Outcome::Error {
                ts_ms: outcome_ts_ms,
                duration_ms,
                kind: err.kind().to_string(),
                message: body.clone(),
                attempts,
                session_id: Some(session_id.clone()),
                turn_index: Some(turn_index),
            };
            (MessageKind::Error, body, outcome)
        }
    };

    let request_record = crate::events::RequestRecord {
        ts_ms: request_ts_ms,
        model: meta.model.clone(),
        base_url: meta.base_url.clone(),
        prompt: request.body.clone(),
        content: None,
        session_id: Some(session_id),
        turn_index: Some(turn_index),
    };
    let mut response = MessageEnvelope::new(
        fresh_message_id(&request.conversation_id, outcome_ts_ms),
        request.conversation_id.clone(),
        request.to.clone(),
        request.from.clone(),
        kind,
        body,
        outcome_ts_ms,
    );
    response.in_reply_to = Some(request.message_id.clone());
    response.exchange = Some(crate::message::WrappedExchange::new(Exchange {
        request: request_record,
        outcome,
    }));

    write_exchange_response(mode, &response, output)
}

/// Writes an exchange response according to the input protocol.
fn write_exchange_response(
    mode: ExchangeMode,
    response: &MessageEnvelope,
    mut output: impl Write,
) -> Result<()> {
    interrupt::check()?;
    match (mode, response.kind) {
        (ExchangeMode::Envelope, _) => {
            let json = serde_json::to_string(&response).expect("MessageEnvelope always serializes");
            writeln!(output, "{json}").map_err(io_err)?;
            if response.kind == MessageKind::Error {
                Err(delivered_turn_failure(response))
            } else {
                Ok(())
            }
        }
        (ExchangeMode::PlainText, MessageKind::Response) => {
            writeln!(output, "{}", response.body).map_err(io_err)
        }
        (ExchangeMode::PlainText, MessageKind::Error) => Err(delivered_turn_failure(response)),
        (ExchangeMode::PlainText, _) => {
            eprintln!("{}", response.body);
            Ok(())
        }
    }
}

fn delivered_turn_failure(response: &MessageEnvelope) -> LegError {
    LegError::TurnFailure {
        message_kind: "error".to_string(),
        message: response.body.clone(),
    }
}

/// Opens `leg exchange`'s request source: `path` when given, else stdin.
fn open_input(path: Option<&str>) -> Result<Box<dyn Read>> {
    match path {
        Some(path) => {
            let file = File::open(path)
                .map_err(|err| LegError::Io(format!("failed to open --in file {path:?}: {err}")))?;
            Ok(Box::new(file))
        }
        None => {
            #[cfg(unix)]
            {
                Ok(Box::new(interrupt::SignalAwareStdin::stdin()))
            }
            #[cfg(not(unix))]
            {
                Ok(Box::new(std::io::stdin()))
            }
        }
    }
}

fn read_exchange_input(path: Option<&str>) -> Result<String> {
    let mut raw = String::new();
    if let Err(error) = open_input(path)?.read_to_string(&mut raw) {
        return Err(interrupt::error().unwrap_or_else(|| io_err(error)));
    }
    interrupt::check()?;
    Ok(raw)
}

/// Opens `leg exchange`'s response sink: `path` when given (created,
/// truncated), else stdout.
fn open_output(path: Option<&str>) -> Result<Box<dyn Write>> {
    match path {
        Some(path) => {
            let file = File::create(path).map_err(|err| {
                LegError::Io(format!("failed to create --out file {path:?}: {err}"))
            })?;
            Ok(Box::new(file))
        }
        None => Ok(Box::new(std::io::stdout())),
    }
}

/// Runs one single-turn exchange and writes its result to `output`.
///
/// Config-load failures (bad/missing credential, malformed env values)
/// propagate as `Err` — nothing has been sent to the provider yet. A
/// successful response prints the reply text; a delivered error leaves
/// stdout empty and propagates a failure for `main` to report on stderr.
fn execute_ask(prompt: &str, model: Option<String>, output: impl Write) -> Result<()> {
    let mut config = LegConfig::from_env()?;
    apply_model_override(&mut config, model);
    execute_ask_with_config(config, prompt, output)
}

fn execute_ask_with_images(
    prompt: &str,
    model: Option<String>,
    image_paths: &[String],
    output: impl Write,
) -> Result<()> {
    let mut content = crate::image_input::load_images(image_paths)?;
    content.push(ContentBlock::text(prompt));

    let mut config = LegConfig::from_env()?;
    apply_model_override(&mut config, model);
    execute_ask_with_content(config, prompt, &content, output)
}

/// The text-only core of [`execute_ask`].
fn execute_ask_with_config(config: LegConfig, prompt: &str, output: impl Write) -> Result<()> {
    execute_ask_with_content(config, prompt, &[ContentBlock::text(prompt)], output)
}

/// Runs an ask from an already-built config and user content blocks.
///
/// The one opened trail is shared between `run_ask` and the tool loop's
/// observer, so the turn's tool events land between its request and outcome.
fn execute_ask_with_content(
    config: LegConfig,
    prompt: &str,
    content: &[ContentBlock],
    output: impl Write,
) -> Result<()> {
    let meta = exchange_meta(&config);
    let mut sink = Rc::new(RefCell::new(open_event_sink()));
    let transport = build_transport(config).with_observer(tool_trail_observer(sink.clone()));
    let participant = LocalParticipant::new(transport, meta.clone());
    match content {
        [ContentBlock::Text { text }] if text == prompt => {
            run_ask(&participant, &meta, prompt, output, &mut sink)
        }
        _ => run_ask_with_content(&participant, &meta, prompt, content, output, &mut sink),
    }
}

/// A tool-loop observer recording each sessionless tool event on `sink`.
fn tool_trail_observer(mut sink: Rc<RefCell<Box<dyn EventSink>>>) -> ToolObserver {
    Box::new(move |event| {
        emit(
            &mut sink,
            &ExchangeEvent::from_tool_event(now_ms(), event, None),
        )
    })
}

/// Emits the sessionless request and outcome around one participant call.
fn respond_with_trail(
    meta: &ExchangeMeta,
    request: &MessageEnvelope,
    content: &[ContentBlock],
    sink: &mut dyn EventSink,
    respond: impl FnOnce(&MessageEnvelope) -> MessageEnvelope,
) -> Result<MessageEnvelope> {
    emit(
        sink,
        &ExchangeEvent::request(now_ms(), meta, &request.body).with_content(content),
    );

    let start = Instant::now();
    let response = respond(request);
    let duration_ms = start.elapsed().as_millis() as u64;
    if let Some(error) = interrupt::error() {
        let attempts = response
            .exchange
            .as_ref()
            .map(|wrapped| match &wrapped.exchange.outcome {
                Outcome::Ok { attempts, .. } | Outcome::Error { attempts, .. } => {
                    attempts.unwrap_or_default()
                }
            })
            .unwrap_or_default();
        emit(
            sink,
            &ExchangeEvent::response_error_with_attempts(now_ms(), duration_ms, &error, attempts),
        );
        return Err(error);
    }
    if let Some(wrapped) = &response.exchange {
        emit(
            sink,
            &ExchangeEvent::from_outcome(&wrapped.exchange.outcome),
        );
    }
    Ok(response)
}

/// Testable core of [`execute_ask_with_content`], parameterised over a
/// [`Participant`] so the success and delivered-error behavior is exercisable
/// without a network.
///
/// The `request` event is recorded *before* the provider call — matching
/// [`ExchangeEvent::Request`]'s documented "emitted before the provider call"
/// contract, so a process killed mid-call still leaves a torn-but-present
/// request line (the trail's documented in-flight/torn-request behaviour;
/// see [`crate::log::parse_jsonl`]'s trailing-request handling). The
/// [`Participant`]'s nested outcome is mirrored onto `sink` unless an
/// interrupt is pending, in which case the trail gets an `interrupted` error.
fn run_ask(
    participant: &impl Participant,
    meta: &ExchangeMeta,
    prompt: &str,
    output: impl Write,
    sink: &mut dyn EventSink,
) -> Result<()> {
    let content = [ContentBlock::text(prompt)];
    run_ask_inner(meta, prompt, &content, output, sink, |request| {
        participant.respond(request)
    })
}

fn run_ask_with_content<T: Transport>(
    participant: &LocalParticipant<T>,
    meta: &ExchangeMeta,
    prompt: &str,
    content: &[ContentBlock],
    output: impl Write,
    sink: &mut dyn EventSink,
) -> Result<()> {
    run_ask_inner(meta, prompt, content, output, sink, |request| {
        participant.respond_with_content(request, content)
    })
}

fn run_ask_inner(
    meta: &ExchangeMeta,
    prompt: &str,
    content: &[ContentBlock],
    mut output: impl Write,
    sink: &mut dyn EventSink,
    respond: impl FnOnce(&MessageEnvelope) -> MessageEnvelope,
) -> Result<()> {
    let request = MessageEnvelope::new(
        "ask-1",
        "ask",
        "user",
        "assistant",
        MessageKind::Request,
        prompt,
        crate::events::now_ms(),
    );
    let response = respond_with_trail(meta, &request, content, sink, respond)?;
    interrupt::check()?;

    match response.kind {
        MessageKind::Response => writeln!(output, "{}", response.body).map_err(io_err),
        MessageKind::Error => Err(delivered_turn_failure(&response)),
        _ => {
            let json = serde_json::to_string(&response).expect("MessageEnvelope always serializes");
            writeln!(output, "{json}").map_err(io_err)
        }
    }
}

/// Runs a fresh `leg session`: opens the session boundary on the trail (every
/// turn's `request` carries `session_id`; the matching `session_end` closes it
/// on a clean exit) and enters the shared REPL loop.
fn execute_session(
    transport: &ToolLoop<impl Transport>,
    sink: &mut dyn EventSink,
    meta: &ExchangeMeta,
    input: impl BufRead,
    output: impl Write,
    session_id: String,
) -> Result<()> {
    eprintln!(
        "leg session — type a message and press enter; Ctrl-D or {SESSION_EXIT_COMMAND} to quit"
    );
    emit(sink, &ExchangeEvent::session_start(now_ms(), &session_id));
    run_session_repl(
        transport,
        sink,
        meta,
        input,
        output,
        session_id,
        Conversation::new(),
        0,
    )
}

/// Resumes a prior session from its rehydrated state and re-enters the REPL.
///
/// Unlike [`execute_session`], no fresh `session_start` is emitted: the
/// original run already opened this session's frame on the trail, and
/// partitioning keys on `session_id` (see [`crate::log::parse_sessions`]), so
/// the resumed run reuses that id and continues its `turn_index`.
fn execute_session_resumed(
    transport: &ToolLoop<impl Transport>,
    sink: &mut dyn EventSink,
    meta: &ExchangeMeta,
    input: impl BufRead,
    output: impl Write,
    resumed: ResumedSession,
) -> Result<()> {
    eprintln!(
        "leg session — resumed {} ({} prior turn(s)); type a message and press enter, Ctrl-D or {SESSION_EXIT_COMMAND} to quit",
        resumed.session_id, resumed.prior_turns,
    );
    run_session_repl(
        transport,
        sink,
        meta,
        input,
        output,
        resumed.session_id,
        resumed.conversation,
        resumed.next_turn_index,
    )
}

/// The shared REPL loop behind [`execute_session`] and
/// [`execute_session_resumed`].
#[allow(clippy::too_many_arguments)]
fn run_session_repl(
    transport: &ToolLoop<impl Transport>,
    sink: &mut dyn EventSink,
    meta: &ExchangeMeta,
    input: impl BufRead,
    output: impl Write,
    session_id: String,
    conversation: Conversation,
    turn_index: u64,
) -> Result<()> {
    let stderr = std::io::stderr();
    let mut warning = stderr.lock();
    run_session_repl_with_warning(
        transport,
        sink,
        meta,
        input,
        output,
        session_id,
        conversation,
        turn_index,
        &mut warning,
    )
}

/// Testable form of [`run_session_repl`] with an injected warning sink.
///
/// Each line read from `input` becomes a user turn appended to
/// `conversation`; the full accumulated history is resent on every request,
/// so turn N carries all prior user and assistant turns. Each turn runs
/// through the [`ToolLoop`]; its tool rounds are appended to the history and
/// only the final reply is printed to `output` (and appended as the next
/// turn). Blank lines are
/// ignored; EOF or a lone [`SESSION_EXIT_COMMAND`] line ends the loop cleanly.
///
/// A turn that fails at the transport layer is **not** fatal: the error is
/// reported on stderr and the loop continues. The failed user turn is rolled
/// back out of the history so it never produces two consecutive same-role
/// turns, which the Messages API rejects. Each turn still emits a `request`
/// plus one `response_ok`/`response_error` event, exactly like `ask`.
#[allow(clippy::too_many_arguments)]
fn run_session_repl_with_warning(
    transport: &ToolLoop<impl Transport>,
    sink: &mut dyn EventSink,
    meta: &ExchangeMeta,
    input: impl BufRead,
    mut output: impl Write,
    session_id: String,
    mut conversation: Conversation,
    mut turn_index: u64,
    warning: &mut dyn Write,
) -> Result<()> {
    for line in input.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => return Err(interrupt::error().unwrap_or_else(|| io_err(error))),
        };
        interrupt::check()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == SESSION_EXIT_COMMAND {
            break;
        }

        conversation.push_user(line.as_str());
        let call = timed_session_exchange(
            sink,
            TimedSessionExchangeContext {
                meta,
                prompt: &line,
                session_id: &session_id,
                turn_index,
                max_tool_rounds: transport.max_tool_rounds(),
            },
            warning,
            |sink| {
                transport.run_observed_with_attempts(conversation.messages(), &mut |event| {
                    let turn = Some((session_id.as_str(), turn_index));
                    emit(sink, &ExchangeEvent::from_tool_event(now_ms(), event, turn));
                })
            },
        );
        turn_index += 1;

        match call.result {
            Ok(outcome) => {
                interrupt::check()?;
                writeln!(output, "{}", outcome.reply.text).map_err(io_err)?;
                // Keep the turn's tool rounds and the reply's full blocks so
                // the resent history matches what the provider returned.
                for message in outcome.transcript {
                    conversation.push(message);
                }
                conversation.push(session_reply_message(outcome.reply.content, outcome.capped));
            }
            Err(err @ LegError::Interrupted { .. }) => return Err(err),
            Err(err) => {
                // Roll the failed user turn back out so the next request does
                // not send two consecutive user turns. The loop continues —
                // a transient failure should not end an interactive session.
                conversation.pop();
                eprintln!("error: {err}");
            }
        }
    }

    // Clean exit (EOF / `/exit`): close the session boundary. A session
    // killed mid-run never reaches here, so its trail carries a
    // `session_start` and turns but no `session_end` — partitioning keys on
    // `session_id`, not on a matched pair (see `crate::log::parse_sessions`).
    emit(
        sink,
        &ExchangeEvent::session_end(now_ms(), &session_id, turn_index),
    );

    Ok(())
}

struct TimedSessionExchangeContext<'a> {
    meta: &'a ExchangeMeta,
    prompt: &'a str,
    session_id: &'a str,
    turn_index: u64,
    max_tool_rounds: Option<usize>,
}

/// Times one session turn's provider call, recording its `request` and
/// terminal outcome on `sink` before returning the call's result. `call`
/// gets the sink so the turn's tool events land between the two.
fn timed_session_exchange(
    sink: &mut dyn EventSink,
    context: TimedSessionExchangeContext<'_>,
    warning: &mut dyn Write,
    call: impl FnOnce(&mut dyn EventSink) -> TransportCall<TurnOutcome>,
) -> TransportCall<TurnOutcome> {
    let TimedSessionExchangeContext {
        meta,
        prompt,
        session_id,
        turn_index,
        max_tool_rounds,
    } = context;
    let request = ExchangeEvent::session_request(now_ms(), meta, prompt, session_id, turn_index);
    emit(sink, &request);

    let start = Instant::now();
    let call = call(sink);
    let duration_ms = start.elapsed().as_millis() as u64;
    let attempts = call.attempts;
    let result = match interrupt::error() {
        Some(error) => Err(error),
        None => call.result,
    };

    if let Ok(outcome) = &result {
        if outcome.reply.stop_reason == Some(StopReason::MaxTokens) {
            let _ = writeln!(
                warning,
                "warning: reply truncated (stop_reason: {})",
                StopReason::MaxTokens.as_str()
            );
        }
        if outcome.capped {
            let max_tool_rounds =
                max_tool_rounds.expect("a capped turn has a configured round limit");
            let _ = writeln!(warning, "{}", tool_round_limit_warning(max_tool_rounds));
        }
    }

    let event = match &result {
        Ok(TurnOutcome { reply, .. }) => ExchangeEvent::session_response_ok_with_attempts(
            now_ms(),
            duration_ms,
            &reply.text,
            reply.usage.input_tokens,
            reply.usage.output_tokens,
            reply.stop_reason.as_ref().map(StopReason::as_str),
            session_id,
            turn_index,
            attempts,
        )
        .with_content(&reply.content),
        Err(err) => ExchangeEvent::session_response_error_with_attempts(
            now_ms(),
            duration_ms,
            err,
            session_id,
            turn_index,
            attempts,
        ),
    };
    emit(sink, &event);

    TransportCall::completed(result, attempts)
}

/// The history turn recorded for a session reply.
///
/// A reply that stopped at the tool-round limit still carries `tool_use`
/// blocks that were never answered; the provider rejects a later request whose
/// `tool_use` has no matching `tool_result`, so those blocks are dropped while
/// text, images, and signed thinking blocks remain in history.
fn session_reply_message(content: Vec<ContentBlock>, capped: bool) -> Message {
    if !capped {
        return Message::new(Role::Assistant, content);
    }
    let preserved: Vec<ContentBlock> = content
        .into_iter()
        .filter(|block| {
            matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Thinking { .. }
            )
        })
        .collect();
    if preserved.is_empty() {
        Message::assistant(TOOL_ROUND_LIMIT_PLACEHOLDER)
    } else {
        Message::new(Role::Assistant, preserved)
    }
}

/// History text standing in for a tool-only reply stopped at the round limit.
const TOOL_ROUND_LIMIT_PLACEHOLDER: &str = "[stopped: tool-round limit reached]";

/// Builds the provider transport every command runs through: a
/// [`ClaudeClient`] advertising the registry's tools, wrapped in the tool loop.
fn build_transport(
    config: LegConfig,
) -> ToolLoop<ClaudeClient<RetryingHttpClient<UreqHttpClient>>> {
    let max_tool_rounds = config.max_tool_rounds;
    let registry = build_tool_registry(&config);
    let client = ClaudeClient::from_config(config).with_tools(registry.specs());
    ToolLoop::new(client, registry, max_tool_rounds)
}

/// Registers the synchronous tools. `read` and `write` share one read-set
/// that lives for this process: `read` records into it and `write` gates
/// overwrites on it.
fn build_tool_registry(config: &LegConfig) -> ToolRegistry {
    let reads = ReadSet::new();
    let mut registry = ToolRegistry::new();
    if let Some(path) = &config.pre_tool_hook {
        registry = registry.with_pre_tool_hook(path.clone());
    }
    registry.register(ReadTool::spec(), Box::new(ReadTool::new(reads.clone())));
    registry.register(WriteTool::spec(), Box::new(WriteTool::new(reads)));
    registry.register(EditTool::spec(), Box::new(EditTool::new()));
    registry.register(
        BashTool::spec_with_default_timeout_secs(config.bash_timeout_secs),
        Box::new(BashTool::with_default_timeout_secs(
            config.bash_timeout_secs,
        )),
    );
    registry
}

/// Records `event`, downgrading a persistence failure to a stderr warning.
///
/// The event trail is observability, not the user's result — a log write
/// that fails must not abort the command or pollute the stdout reply
/// contract.
fn emit(sink: &mut dyn EventSink, event: &ExchangeEvent) {
    if let Err(err) = sink.record(event) {
        eprintln!("warning: failed to record exchange event: {err}");
    }
}

/// Mints a session id unique to this `leg session` process.
///
/// Derived from the process id and the start timestamp — dependency-free.
/// One `session` process runs one session, so `(pid, start-ms)` cannot
/// collide with another live session on the same host.
fn new_session_id() -> String {
    format!("sess-{}-{}", std::process::id(), now_ms())
}

/// A prior session rehydrated from its trail, ready to re-enter the REPL.
#[derive(Debug)]
struct ResumedSession {
    /// The original session's id, reused for every resumed turn.
    session_id: String,
    /// History reconstructed from the trail's completed turns.
    conversation: Conversation,
    /// How many completed turns `conversation` holds.
    prior_turns: usize,
    /// The `turn_index` the first resumed turn will carry.
    next_turn_index: u64,
}

/// Reads a session trail and rehydrates the target session for `--resume`.
///
/// Opens `file`, partitions it with [`crate::log::parse_sessions`] (torn-tail
/// tolerant), surfaces any parse warnings on stderr, then hands off to
/// [`select_and_rehydrate`]. This runs *before* the caller opens the append
/// sink, so a parse or selection failure exits non-zero having written
/// nothing.
fn load_resume(file: &str, session_id: Option<&str>) -> Result<ResumedSession> {
    let handle = File::open(file)
        .map_err(|err| LegError::Io(format!("failed to open --resume file {file:?}: {err}")))?;
    let report = crate::log::parse_sessions(handle)?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    select_and_rehydrate(report.sessions, session_id)
}

/// Selects the target session and rehydrates it — the pure core of
/// `--resume`.
///
/// With `session_id`, selects that id (a miss is a usage error). Without it,
/// the file must hold exactly one session: zero is a usage error, more than
/// one names the available ids and requires `--session`. The selected
/// session's turns are replayed in order into a fresh [`Conversation`] by
/// [`rehydrate_turn`]: each turn whose outcome is `Ok` contributes its user
/// turn, its tool rounds (from `tool_round` + `tool_result` lines), and its
/// final reply, rebuilt from the trail's `content` blocks when recorded and
/// from the `prompt`/`reply` text otherwise (text-only and older trails). Turns
/// with an `Error` or a torn (`None`) outcome contributed no assistant reply
/// to the original in-memory history (the live loop rolls a failed user turn
/// back out), so they are skipped. The next `turn_index` continues past the
/// last recorded turn (torn or not).
fn select_and_rehydrate(
    sessions: Vec<crate::log::SessionRecord>,
    session_id: Option<&str>,
) -> Result<ResumedSession> {
    let record = match session_id {
        Some(wanted) => sessions
            .into_iter()
            .find(|s| s.session_id == wanted)
            .ok_or_else(|| {
                LegError::Usage(format!("no session {wanted:?} in the --resume trail"))
            })?,
        None => {
            let mut iter = sessions.into_iter();
            let first = iter.next().ok_or_else(|| {
                LegError::Usage("the --resume trail holds no sessions".to_string())
            })?;
            if let Some(second) = iter.next() {
                let mut ids = vec![first.session_id, second.session_id];
                ids.extend(iter.map(|s| s.session_id));
                return Err(LegError::Usage(format!(
                    "the --resume trail holds {} sessions; select one with --session <id>: {}",
                    ids.len(),
                    ids.join(", "),
                )));
            }
            first
        }
    };

    let mut conversation = Conversation::new();
    let mut prior_turns = 0;
    for turn in &record.turns {
        if let Some(messages) = rehydrate_turn(turn) {
            for message in messages {
                conversation.push(message);
            }
            prior_turns += 1;
        }
    }

    let next_turn_index = record
        .turns
        .last()
        .and_then(|t| t.request.turn_index)
        .map_or(record.turns.len() as u64, |i| i + 1);

    Ok(ResumedSession {
        session_id: record.session_id,
        conversation,
        prior_turns,
        next_turn_index,
    })
}

/// Rebuilds one trail turn as the history the live REPL appended for it: the
/// user turn, each tool round's full assistant reply and its `tool_result`
/// user turn, then the final reply (see [`run_session_repl_with_warning`]).
///
/// `None` for a turn that contributed nothing to the live history — an
/// `Error` or torn outcome — or whose tool results never all landed, since a
/// `tool_use` without its `tool_result` is a history the provider rejects.
fn rehydrate_turn(turn: &crate::log::SessionTurn) -> Option<Vec<Message>> {
    let Some(Outcome::Ok {
        reply,
        content,
        stop_reason,
        ..
    }) = &turn.outcome
    else {
        return None;
    };

    let mut messages = vec![rehydrate(
        Role::User,
        &turn.request.prompt,
        &turn.request.content,
    )];
    for round in &turn.rounds {
        let results = round
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(rehydrate_tool_result(turn, id)),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        messages.push(Message::new(Role::Assistant, round.clone()));
        messages.push(Message::new(Role::User, results));
    }
    // A reply still requesting tools stopped at the round limit; the live
    // loop kept it without its unanswered `tool_use` blocks.
    let capped = !turn.rounds.is_empty() && stop_reason.as_deref() == Some("tool_use");
    let reply = rehydrate(Role::Assistant, reply, content);
    messages.push(session_reply_message(reply.content, capped));
    Some(messages)
}

/// Rebuilds the `tool_result` block the loop sent for call `id`, from the
/// turn's recorded result; `None` when that result never landed.
fn rehydrate_tool_result(turn: &crate::log::SessionTurn, id: &str) -> Option<ContentBlock> {
    let result = turn
        .tools
        .iter()
        .find(|pair| pair.call.tool_use_id == id)?
        .result
        .as_ref()?;
    let (content, is_error) = match result.status {
        ToolStatus::Completed => (result.result.clone().unwrap_or_default(), None),
        ToolStatus::Failed | ToolStatus::Denied => {
            (result.error.clone().unwrap_or_default(), Some(true))
        }
    };
    Some(ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content,
        is_error,
    })
}

/// Rebuilds one trail turn as a [`Message`]: its recorded `content` blocks when
/// present, else a single text block from `text`.
fn rehydrate(role: Role, text: &str, content: &Option<Vec<ContentBlock>>) -> Message {
    match content {
        Some(blocks) => Message::new(role, blocks.clone()),
        None => Message::new(role, vec![ContentBlock::text(text)]),
    }
}

/// Opens the event sink described by [`EVENT_LOG_ENV`].
///
/// A non-blank path is opened for appending (created if absent), so
/// successive runs accumulate one exchange trail. An unset or blank value
/// disables recording. Recording is additive, never load-bearing for the
/// command's actual result (see [`emit`]) — so a failure to open the sink
/// falls back to [`NoopSink`] with a stderr warning rather than aborting the
/// command, exactly like a failure to *write* to an already-open sink.
fn open_event_sink() -> Box<dyn EventSink> {
    match std::env::var(EVENT_LOG_ENV) {
        Ok(path) if !path.trim().is_empty() => open_append_sink(&path),
        _ => Box::new(NoopSink),
    }
}

/// Opens an append-mode event sink on an explicit trail file, for `--resume`.
///
/// Resuming writes new turns back to the trail it read from (not
/// [`EVENT_LOG_ENV`]), so the resumed run extends the same session file. A
/// failure to (re)open it — e.g. a permission change between the earlier
/// read and this open — falls back to [`NoopSink`] with a stderr warning:
/// the session still runs, it just stops accumulating a trail, which is
/// preferable to refusing an otherwise-healthy interactive session.
fn open_append_sink(path: &str) -> Box<dyn EventSink> {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Box::new(WriterSink::new(file)),
        Err(err) => {
            eprintln!("warning: failed to open {path:?} for recording: {err}");
            Box::new(NoopSink)
        }
    }
}

/// Resolves the named-session directory using the documented environment
/// precedence.
fn session_store_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LEG_SESSION_DIR") {
        if path.is_empty() {
            return Err(LegError::Config(
                "LEG_SESSION_DIR must not be blank".to_string(),
            ));
        }
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = std::env::var_os("XDG_STATE_HOME")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path).join("leg").join("sessions"));
    }

    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|path| !path.is_empty()))
        .ok_or_else(|| {
            LegError::Config("could not determine home directory for session store".to_string())
        })?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("leg")
        .join("sessions"))
}

/// Returns the JSONL path for a safe session id. IDs emitted by
/// `new_session_id` use only these filename-safe characters.
fn exchange_session_path(store_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return None;
    }
    Some(store_dir.join(format!("{session_id}.jsonl")))
}

/// Loads and rehydrates one id-addressed session trail.
fn load_exchange_session(path: &Path, session_id: &str) -> Result<ResumedSession> {
    let handle = File::open(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            LegError::SessionNotFound(session_id.to_string())
        } else {
            LegError::Io(format!("failed to open session trail {:?}: {err}", path))
        }
    })?;
    let report = crate::log::parse_sessions(handle)?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    if !report
        .sessions
        .iter()
        .any(|record| record.session_id == session_id)
    {
        return Err(LegError::SessionNotFound(session_id.to_string()));
    }
    select_and_rehydrate(report.sessions, Some(session_id))
}

struct SessionEventSink {
    sink: Box<dyn EventSink>,
    session_write_error: Rc<RefCell<Option<String>>>,
}

/// Opens the required session trail and the optional `LEG_EVENT_LOG` sink.
fn open_session_event_sink(path: &Path, create_new: bool) -> Result<SessionEventSink> {
    let mut options = OpenOptions::new();
    options.write(true).append(true);
    if create_new {
        options.create_new(true);
    }
    let file = options.open(path).map_err(|err| {
        LegError::Io(format!(
            "failed to open session trail {:?} for recording: {err}",
            path
        ))
    })?;
    let event_log = open_event_sink_excluding(path);
    let session_write_error = Rc::new(RefCell::new(None));
    Ok(SessionEventSink {
        sink: Box::new(CompositeEventSink {
            session: Box::new(WriterSink::new(file)),
            event_log,
            session_write_error: Rc::clone(&session_write_error),
        }),
        session_write_error,
    })
}

/// Opens `LEG_EVENT_LOG` unless it points at the session store file itself.
fn open_event_sink_excluding(session_path: &Path) -> Box<dyn EventSink> {
    match std::env::var(EVENT_LOG_ENV) {
        Ok(path) if !path.trim().is_empty() => {
            let event_path = Path::new(&path);
            if same_file_path(event_path, session_path) {
                Box::new(NoopSink)
            } else {
                open_append_sink(&path)
            }
        }
        _ => Box::new(NoopSink),
    }
}

fn same_file_path(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (std::fs::canonicalize(left), std::fs::canonicalize(right)),
            (Ok(left), Ok(right)) if left == right
        )
}

/// Writes each session event to its durable trail and the optional legacy
/// event log.
struct CompositeEventSink {
    session: Box<dyn EventSink>,
    event_log: Box<dyn EventSink>,
    session_write_error: Rc<RefCell<Option<String>>>,
}

impl EventSink for CompositeEventSink {
    fn record(&mut self, event: &ExchangeEvent) -> std::io::Result<()> {
        let session_result = self.session.record(event);
        if let Err(err) = &session_result {
            let mut write_error = self.session_write_error.borrow_mut();
            if write_error.is_none() {
                *write_error = Some(err.to_string());
            }
        }
        let event_log_result = self.event_log.record(event);
        session_result.and(event_log_result)
    }
}

fn write_session_id_out(path: &str, session_id: &str) -> Result<()> {
    let mut file = File::create(path).map_err(|err| {
        LegError::Io(format!(
            "failed to create --session-id-out file {path:?}: {err}"
        ))
    })?;
    writeln!(file, "{session_id}").map_err(|err| {
        LegError::Io(format!(
            "failed to write --session-id-out file {path:?}: {err}"
        ))
    })?;
    file.flush().map_err(|err| {
        LegError::Io(format!(
            "failed to flush --session-id-out file {path:?}: {err}"
        ))
    })
}

/// Resolves the log path and parses it into exchanges.
///
/// The path is `--file` when given, else [`EVENT_LOG_ENV`]; with neither set,
/// there is nothing to read, which is a usage error. Non-fatal warnings
/// collected by [`crate::log::parse_jsonl`] are surfaced on stderr here,
/// keeping `parse_jsonl` pure over its reader.
fn read_log(file: Option<&str>) -> Result<crate::log::ParseReport> {
    let path = resolve_log_path(file)?;
    let handle = File::open(&path)
        .map_err(|err| LegError::Io(format!("failed to open log file {path:?}: {err}")))?;
    let report = crate::log::parse_jsonl(handle)?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(report)
}

/// Resolves the log file path: `--file` takes precedence, then
/// [`EVENT_LOG_ENV`]. A blank value (in either source) is treated as absent.
fn resolve_log_path(file: Option<&str>) -> Result<String> {
    if let Some(path) = file.filter(|p| !p.trim().is_empty()) {
        return Ok(path.to_string());
    }
    match std::env::var(EVENT_LOG_ENV) {
        Ok(path) if !path.trim().is_empty() => Ok(path),
        _ => Err(LegError::Usage(format!(
            "no log file: pass --file <path> or set {EVENT_LOG_ENV}"
        ))),
    }
}

/// Resolves what `leg log replay` reruns: the selected exchange's user content
/// against the config from `load_config` retargeted at that exchange's model +
/// base_url. The exchange is selected first, so a bad `--index` reports its
/// usage error even when the environment's config would not load.
///
/// The rest of the config — the credential, timeouts, max_tokens, system
/// prompt — is the *current* environment's, so a replay re-runs with today's
/// auth, not a credential that was never recorded. A tool-bearing exchange
/// reruns its recorded request content: the current tool loop executes its
/// tools afresh, and the stored tool results are never fed back.
fn replay_target(
    report: &crate::log::ParseReport,
    index: Option<usize>,
    load_config: impl FnOnce() -> Result<LegConfig>,
) -> Result<(LegConfig, String, Vec<ContentBlock>)> {
    let request = &select_exchange(&report.exchanges, index)?.request;
    let mut config = load_config()?;
    config.model = request.model.clone();
    config.base_url = request.base_url.clone();
    let content = request
        .content
        .clone()
        .unwrap_or_else(|| vec![ContentBlock::text(request.prompt.clone())]);
    Ok((config, request.prompt.clone(), content))
}

/// Selects the exchange to replay: 1-based `index`, or the last when `None`.
///
/// An empty log, or an index outside `1..=len`, is an error naming the valid
/// range so the user can correct it.
fn select_exchange(exchanges: &[Exchange], index: Option<usize>) -> Result<&Exchange> {
    if exchanges.is_empty() {
        return Err(LegError::Usage(
            "log contains no complete exchanges to replay".to_string(),
        ));
    }
    let position = match index {
        None => exchanges.len() - 1,
        Some(n) if (1..=exchanges.len()).contains(&n) => n - 1,
        Some(n) => {
            return Err(LegError::Usage(format!(
                "--index {n} is out of range; valid range is 1..={}",
                exchanges.len()
            )));
        }
    };
    Ok(&exchanges[position])
}

/// Writes each exchange, with its tool-round summaries and calls, as a
/// human-readable block to `output`.
///
/// Parameterised over [`Write`] so the rendering is unit-testable with an
/// in-memory buffer. An empty log produces no output.
fn execute_log_show(report: &crate::log::ParseReport, mut output: impl Write) -> Result<()> {
    for (i, exchange) in report.exchanges.iter().enumerate() {
        let tools = report.tools.get(i).map_or(&[][..], Vec::as_slice);
        let rounds = report.rounds.get(i).map_or(&[][..], Vec::as_slice);
        write!(
            output,
            "{}",
            crate::log::format_exchange_with_rounds(i + 1, exchange, tools, rounds)
        )
        .map_err(io_err)?;
    }
    Ok(())
}

/// Builds the replay-relevant [`ExchangeMeta`] shared by every exchange in a
/// command run.
fn exchange_meta(config: &LegConfig) -> ExchangeMeta {
    ExchangeMeta {
        model: config.model.clone(),
        base_url: config.base_url.clone(),
    }
}

fn io_err(err: std::io::Error) -> LegError {
    LegError::Io(err.to_string())
}

/// Applies the `--model` override (if any) onto a loaded config, in place.
fn apply_model_override(config: &mut LegConfig, model: Option<String>) {
    if let Some(model) = model {
        config.model = model;
    }
}

#[cfg(test)]
mod tests;
