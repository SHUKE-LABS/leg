//! The command-line entry surface.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::time::Instant;

use crate::config::LegConfig;
use crate::error::{LegError, Result};
use crate::events::{
    EventSink, Exchange, ExchangeEvent, ExchangeMeta, NoopSink, Outcome, WriterSink, now_ms,
};
use crate::message::{MessageEnvelope, MessageKind};
use crate::model::{AssistantReply, Conversation};
use crate::participant::{LocalParticipant, Participant};
use crate::transport::Transport;
use crate::transport::claude::ClaudeClient;

/// The one-line usage summary, shared by `--help` output and usage errors.
const USAGE: &str = "usage: leg [--version|-V] [--help|-h] | leg ask [--model <model>] <prompt> | leg session [--resume <file> [--session <id>]] | leg log show [--file <path>] | leg log replay [--file <path>] [--index <N>] | leg exchange [--in <path>] [--out <path>]";

/// Name of the environment variable naming the JSONL exchange trail to append
/// to. An unset or blank value disables recording for `ask`/fresh `session`
/// runs (a `--resume` run instead appends to the trail it read from).
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
    /// Re-runs one logged exchange's prompt against today's provider config.
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
    },
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
        Some(Command::Ask { prompt, model }) => {
            let stdout = std::io::stdout();
            execute_ask(&prompt, model, stdout.lock())
        }
        Some(Command::Session { resume }) => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            match resume {
                None => {
                    let config = LegConfig::from_env()?;
                    let meta = exchange_meta(&config);
                    let client = ClaudeClient::from_config(config);
                    let mut sink = open_event_sink();
                    let session_id = new_session_id();
                    execute_session(
                        &client,
                        sink.as_mut(),
                        &meta,
                        stdin.lock(),
                        stdout.lock(),
                        session_id,
                    )
                }
                Some(args) => {
                    let config = LegConfig::from_env()?;
                    let meta = exchange_meta(&config);
                    let client = ClaudeClient::from_config(config);
                    // Resume: load + select the prior session *before* opening
                    // any sink, so a bad selection (missing id, empty/
                    // ambiguous trail) exits non-zero having written nothing.
                    let resumed = load_resume(&args.file, args.session_id.as_deref())?;
                    let mut sink = open_append_sink(&args.file);
                    execute_session_resumed(
                        &client,
                        sink.as_mut(),
                        &meta,
                        stdin.lock(),
                        stdout.lock(),
                        resumed,
                    )
                }
            }
        }
        Some(Command::LogShow { file }) => {
            let exchanges = read_log(file.as_deref())?;
            let stdout = std::io::stdout();
            execute_log_show(&exchanges, stdout.lock())
        }
        Some(Command::LogReplay { file, index }) => {
            let exchanges = read_log(file.as_deref())?;
            let request = &select_exchange(&exchanges, index)?.request;

            // Replay targets the logged exchange's model + base_url, but uses
            // the *current* credential (and timeout / max_tokens / system
            // prompt) from the environment — so a replay re-runs with today's
            // auth, not a credential that was never recorded.
            let mut config = LegConfig::from_env()?;
            config.model = request.model.clone();
            config.base_url = request.base_url.clone();
            let prompt = request.prompt.clone();

            let stdout = std::io::stdout();
            execute_ask_with_config(config, &prompt, stdout.lock())
        }
        Some(Command::Exchange { in_path, out_path }) => {
            execute_exchange(in_path.as_deref(), out_path.as_deref())
        }
    }
}

/// The full `--help` body: the usage summary plus the env vars `ask` reads.
fn help_text() -> String {
    format!(
        "{USAGE}\n\n\
         Reads credentials from ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN /\n\
         CLAUDE_CODE_OAUTH_TOKEN). Also honours ANTHROPIC_BASE_URL, LEG_MODEL,\n\
         LEG_TIMEOUT_SECS, LEG_MAX_TOKENS, and LEG_SYSTEM_PROMPT.\n\n\
         LEG_EVENT_LOG names a JSONL file that `ask` and a fresh `session`\n\
         append an exchange trail to (unset/blank disables recording); `leg\n\
         log show`/`leg log replay` read it back (or `--file <path>`).\n\n\
         `leg exchange` reads a `baton.message/v1` envelope on --in/stdin and\n\
         writes the response envelope on --out/stdout; given plain text\n\
         instead, it writes just the reply body (or nothing, on failure) —\n\
         the shape `baton serve --agent-cmd <path> --agent-arg exchange`\n\
         expects."
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

/// Parses the arguments following `ask`: an optional `--model <value>` flag (in
/// any position) plus exactly one non-blank positional prompt.
fn parse_ask<'a>(iter: impl Iterator<Item = &'a String>) -> Result<Command> {
    let mut model = None;
    let mut prompt = None;

    let mut iter = iter.peekable();
    while let Some(arg) = iter.next() {
        if arg == "--model" {
            let value = iter
                .next()
                .ok_or_else(|| LegError::Usage("--model requires a value".to_string()))?;
            model = Some(value.clone());
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

    Ok(Command::Ask { prompt, model })
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
            other => {
                return Err(LegError::Usage(format!("unexpected argument {other:?}")));
            }
        }
    }

    Ok(Command::Exchange { in_path, out_path })
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
/// Config-load and `--in`/`--out` I/O failures propagate as `Err`; once a
/// [`LocalParticipant`] answers, [`execute_exchange_core`] is infallible —
/// see its doc for the per-mode output shape.
fn execute_exchange(in_path: Option<&str>, out_path: Option<&str>) -> Result<()> {
    let config = LegConfig::from_env()?;
    let meta = exchange_meta(&config);
    let client = ClaudeClient::from_config(config);
    let participant = LocalParticipant::new(client, meta);

    let mut raw = String::new();
    open_input(in_path)?
        .read_to_string(&mut raw)
        .map_err(io_err)?;

    let output = open_output(out_path)?;
    execute_exchange_core(&participant, &raw, output)
}

/// Testable core of [`execute_exchange`], parameterised over a [`Participant`]
/// so the per-mode output contract is exercisable without a network.
///
/// - [`ExchangeMode::Envelope`]: writes the full response envelope as one
///   JSON line, whatever `kind` it carries (mirrors `baton exchange`).
/// - [`ExchangeMode::PlainText`] + [`MessageKind::Response`]: writes just
///   `response.body`.
/// - [`ExchangeMode::PlainText`] + a delivered error kind: writes nothing to
///   `output` and prints the message to stderr. Baton's own
///   `ExternalAgentParticipant` treats exit-0-with-empty-stdout as a
///   machinery failure and synthesizes its own delivered `kind: "error"`
///   envelope — this is what makes AC3 observable for the external-agent
///   path, since a plain-text reply carries no `kind` field of its own.
///
/// Every branch returns `Ok(())`: a participant-delivered outcome — success
/// or error, either mode — never becomes a process `Err` here.
fn execute_exchange_core(
    participant: &impl Participant,
    raw: &str,
    mut output: impl Write,
) -> Result<()> {
    let (request, mode) = parse_exchange_request(raw);
    let response = participant.respond(&request);

    match (mode, response.kind) {
        (ExchangeMode::Envelope, _) => {
            let json = serde_json::to_string(&response).expect("MessageEnvelope always serializes");
            writeln!(output, "{json}").map_err(io_err)
        }
        (ExchangeMode::PlainText, MessageKind::Response) => {
            writeln!(output, "{}", response.body).map_err(io_err)
        }
        (ExchangeMode::PlainText, _) => {
            eprintln!("{}", response.body);
            Ok(())
        }
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
        None => Ok(Box::new(std::io::stdin())),
    }
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
/// propagate as `Err` — nothing has been sent to the provider yet. Once a
/// [`LocalParticipant`] answers, the result is infallible per the
/// [`Participant`] contract: a success prints the reply text; a provider or
/// delivery failure prints the response `MessageEnvelope` as JSON
/// (`"kind":"error"`) instead — both exit 0.
fn execute_ask(prompt: &str, model: Option<String>, output: impl Write) -> Result<()> {
    let mut config = LegConfig::from_env()?;
    apply_model_override(&mut config, model);
    execute_ask_with_config(config, prompt, output)
}

/// The testable core of [`execute_ask`], parameterised over an already-built
/// [`LegConfig`] so `leg log replay` (which overrides `model`/`base_url` from
/// the logged exchange) shares this path.
fn execute_ask_with_config(config: LegConfig, prompt: &str, output: impl Write) -> Result<()> {
    let meta = exchange_meta(&config);
    let client = ClaudeClient::from_config(config);
    let participant = LocalParticipant::new(client, meta.clone());
    let mut sink = open_event_sink();
    run_ask(&participant, &meta, prompt, output, sink.as_mut())
}

/// Testable core of [`execute_ask_with_config`], parameterised over a
/// [`Participant`] so the success/error stdout contract is exercisable
/// without a network.
///
/// The `request` event is recorded *before* the provider call — matching
/// [`ExchangeEvent::Request`]'s documented "emitted before the provider call"
/// contract, so a process killed mid-call still leaves a torn-but-present
/// request line (the trail's documented in-flight/torn-request behaviour;
/// see [`crate::log::parse_jsonl`]'s trailing-request handling). The
/// [`Participant`]'s own nested exchange still supplies the terminal outcome
/// (already timed against its own call), mirrored onto `sink` afterwards.
fn run_ask(
    participant: &impl Participant,
    meta: &ExchangeMeta,
    prompt: &str,
    mut output: impl Write,
    sink: &mut dyn EventSink,
) -> Result<()> {
    emit(sink, &ExchangeEvent::request(now_ms(), meta, prompt));

    let request = MessageEnvelope::new(
        "ask-1",
        "ask",
        "user",
        "assistant",
        MessageKind::Request,
        prompt,
        crate::events::now_ms(),
    );
    let response = participant.respond(&request);

    if let Some(wrapped) = &response.exchange {
        emit(
            sink,
            &ExchangeEvent::from_outcome(&wrapped.exchange.outcome),
        );
    }

    match response.kind {
        MessageKind::Response => writeln!(output, "{}", response.body).map_err(io_err),
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
    transport: &impl Transport,
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
    transport: &impl Transport,
    sink: &mut dyn EventSink,
    meta: &ExchangeMeta,
    input: impl BufRead,
    output: impl Write,
    resumed: ResumedSession,
) -> Result<()> {
    eprintln!(
        "leg session — resumed {} ({} prior turn(s)); type a message and press enter, Ctrl-D or {SESSION_EXIT_COMMAND} to quit",
        resumed.session_id,
        resumed.conversation.len() / 2,
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
    transport: &impl Transport,
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
/// so turn N carries all prior user and assistant turns. The assistant reply
/// is printed to `output` (and appended as the next turn). Blank lines are
/// ignored; EOF or a lone [`SESSION_EXIT_COMMAND`] line ends the loop cleanly.
///
/// A turn that fails at the transport layer is **not** fatal: the error is
/// reported on stderr and the loop continues. The failed user turn is rolled
/// back out of the history so it never produces two consecutive same-role
/// turns, which the Messages API rejects. Each turn still emits a `request`
/// plus one `response_ok`/`response_error` event, exactly like `ask`.
#[allow(clippy::too_many_arguments)]
fn run_session_repl_with_warning(
    transport: &impl Transport,
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
        let line = line.map_err(io_err)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == SESSION_EXIT_COMMAND {
            break;
        }

        conversation.push_user(line.as_str());
        let result =
            timed_session_exchange(sink, meta, &line, &session_id, turn_index, warning, || {
                transport.send_conversation(conversation.messages())
            });
        turn_index += 1;

        match result {
            Ok(reply) => {
                writeln!(output, "{}", reply.text).map_err(io_err)?;
                conversation.push_assistant(reply.text);
            }
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

/// Times one session turn's provider call, recording its `request` and
/// terminal outcome on `sink` before returning the call's result.
fn timed_session_exchange(
    sink: &mut dyn EventSink,
    meta: &ExchangeMeta,
    prompt: &str,
    session_id: &str,
    turn_index: u64,
    warning: &mut dyn Write,
    call: impl FnOnce() -> Result<AssistantReply>,
) -> Result<AssistantReply> {
    let request = ExchangeEvent::session_request(now_ms(), meta, prompt, session_id, turn_index);
    emit(sink, &request);

    let start = Instant::now();
    let result = call();
    let duration_ms = start.elapsed().as_millis() as u64;

    if let Ok(reply) = &result
        && let Some(stop_reason) = reply.stop_reason.as_deref()
    {
        let _ = writeln!(
            warning,
            "warning: reply truncated (stop_reason: {stop_reason})"
        );
    }

    let event = match &result {
        Ok(reply) => ExchangeEvent::session_response_ok(
            now_ms(),
            duration_ms,
            &reply.text,
            reply.usage.input_tokens,
            reply.usage.output_tokens,
            reply.stop_reason.as_deref(),
            session_id,
            turn_index,
        ),
        Err(err) => ExchangeEvent::session_response_error(
            now_ms(),
            duration_ms,
            err,
            session_id,
            turn_index,
        ),
    };
    emit(sink, &event);

    result
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
/// session's turns are replayed in order into a fresh [`Conversation`]: each
/// turn whose outcome is `Ok` contributes a user + an assistant turn. Turns
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
    for turn in &record.turns {
        if let Some(Outcome::Ok { reply, .. }) = &turn.outcome {
            conversation.push_user(turn.request.prompt.as_str());
            conversation.push_assistant(reply.as_str());
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
        next_turn_index,
    })
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

/// Resolves the log path and parses it into exchanges.
///
/// The path is `--file` when given, else [`EVENT_LOG_ENV`]; with neither set,
/// there is nothing to read, which is a usage error. Non-fatal warnings
/// collected by [`crate::log::parse_jsonl`] are surfaced on stderr here,
/// keeping `parse_jsonl` pure over its reader.
fn read_log(file: Option<&str>) -> Result<Vec<Exchange>> {
    let path = resolve_log_path(file)?;
    let handle = File::open(&path)
        .map_err(|err| LegError::Io(format!("failed to open log file {path:?}: {err}")))?;
    let report = crate::log::parse_jsonl(handle)?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(report.exchanges)
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

/// Writes each exchange as a human-readable block to `output`.
///
/// Parameterised over [`Write`] so the rendering is unit-testable with an
/// in-memory buffer. An empty log produces no output.
fn execute_log_show(exchanges: &[Exchange], mut output: impl Write) -> Result<()> {
    for (i, exchange) in exchanges.iter().enumerate() {
        write!(output, "{}", crate::log::format_exchange(i + 1, exchange)).map_err(io_err)?;
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
mod tests {
    use super::*;
    use crate::model::AssistantReply;
    use crate::transport::Transport;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    struct FakeTransport(std::result::Result<AssistantReply, LegError>);

    impl Transport for FakeTransport {
        fn send_conversation(&self, _messages: &[crate::model::Message]) -> Result<AssistantReply> {
            match &self.0 {
                Ok(reply) => Ok(reply.clone()),
                Err(LegError::Auth(msg)) => Err(LegError::Auth(msg.clone())),
                Err(other) => Err(LegError::Transport(other.to_string())),
            }
        }
    }

    /// A [`Transport`] that records every call's full message history and
    /// answers from a queue of canned replies, in order — lets a test assert
    /// exactly what history a later turn sent, not just its printed reply.
    struct CapturingTransport {
        replies: std::cell::RefCell<std::collections::VecDeque<AssistantReply>>,
        calls: std::cell::RefCell<Vec<Vec<crate::model::Message>>>,
    }

    impl CapturingTransport {
        fn new(replies: Vec<AssistantReply>) -> Self {
            Self {
                replies: std::cell::RefCell::new(replies.into()),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for CapturingTransport {
        fn send_conversation(&self, messages: &[crate::model::Message]) -> Result<AssistantReply> {
            self.calls.borrow_mut().push(messages.to_vec());
            Ok(self
                .replies
                .borrow_mut()
                .pop_front()
                .expect("test queued enough replies for every expected call"))
        }
    }

    fn meta() -> ExchangeMeta {
        ExchangeMeta {
            model: "claude-test-model".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    #[test]
    fn run_ask_prints_only_reply_text_on_success() {
        let participant =
            LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
        let mut buf = Vec::new();
        let mut sink = NoopSink;
        run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
            .expect("infallible per Participant contract");
        assert_eq!(String::from_utf8(buf).unwrap(), "hi there\n");
    }

    #[test]
    fn run_ask_prints_error_envelope_json_on_delivery_failure_and_does_not_err() {
        let participant = LocalParticipant::new(
            FakeTransport(Err(LegError::Auth("bad credentials".to_string()))),
            meta(),
        );
        let mut buf = Vec::new();
        let mut sink = NoopSink;
        run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
            .expect("infallible per Participant contract");
        let printed = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim()).expect("valid json");
        assert_eq!(value["kind"], "error");
        assert_eq!(value["body"], "authentication error: bad credentials");
    }

    #[test]
    fn run_ask_emits_request_and_response_ok_events_to_the_sink() {
        let participant =
            LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
        let mut buf = Vec::new();
        let mut trail = Vec::new();
        {
            let mut sink = WriterSink::new(&mut trail);
            run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
                .expect("infallible per Participant contract");
        }
        let text = String::from_utf8(trail).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "request");
        assert_eq!(first["prompt"], "hello");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "response_ok");
        assert_eq!(second["reply"], "hi there");
    }

    /// A [`Transport`] that panics if called — proves `run_ask` records the
    /// `request` event *before* invoking the provider (must be able to
    /// observe a request line even if the call that follows never returns),
    /// matching [`ExchangeEvent::Request`]'s documented ordering contract.
    struct PanicTransport;

    impl Transport for PanicTransport {
        fn send_conversation(&self, _messages: &[crate::model::Message]) -> Result<AssistantReply> {
            panic!("run_ask must not have called the transport yet");
        }
    }

    /// A sink that panics on its first `record` call whose event is not
    /// `request` — proves the request line is the *first* thing recorded,
    /// i.e. emitted before the provider call runs (which, for
    /// [`PanicTransport`], never returns at all).
    struct RequestFirstSink {
        seen_request: bool,
    }

    impl EventSink for RequestFirstSink {
        fn record(&mut self, event: &ExchangeEvent) -> std::io::Result<()> {
            match event {
                ExchangeEvent::Request { .. } if !self.seen_request => {
                    self.seen_request = true;
                    Ok(())
                }
                ExchangeEvent::Request { .. } => panic!("request recorded more than once"),
                _ => {
                    assert!(self.seen_request, "outcome recorded before request");
                    Ok(())
                }
            }
        }
    }

    #[test]
    fn run_ask_records_the_request_event_before_invoking_the_transport() {
        // PanicTransport aborts the test the moment `run_ask` reaches the
        // provider call, so simply completing this call proves the request
        // line was recorded first (and RequestFirstSink additionally asserts
        // ordering for any outcome recorded, on the success path elsewhere).
        let participant = LocalParticipant::new(PanicTransport, meta());
        let mut buf = Vec::new();
        let mut sink = RequestFirstSink {
            seen_request: false,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
        }));
        assert!(
            result.is_err(),
            "PanicTransport should have been reached and panicked"
        );
        assert!(sink.seen_request, "request event must be recorded first");
    }

    /// Exercises the actual `run_ask` path (not just the `ExchangeEvent`
    /// constructors) through a real [`WriterSink`], asserting the emitted
    /// lines' field names/order/omissions match baton's own
    /// `parses_valid_two_line_exchange` fixture (`baton/src/log.rs`) exactly
    /// — everything except the wall-clock `ts_ms`/`duration_ms` values, which
    /// neither `leg` nor baton makes deterministic in tests, so this
    /// compares against a template built from the *actual* values `run_ask`
    /// produced rather than fixed literals.
    #[test]
    fn run_ask_wire_lines_match_batons_field_order_and_omitted_fields() {
        let participant =
            LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
        let mut buf = Vec::new();
        let mut trail = Vec::new();
        {
            let mut sink = WriterSink::new(&mut trail);
            run_ask(&participant, &meta(), "hello", &mut buf, &mut sink)
                .expect("infallible per Participant contract");
        }
        let text = String::from_utf8(trail).unwrap();
        let mut lines = text.lines();
        let request_line = lines.next().expect("request line");
        let response_line = lines.next().expect("response line");
        assert!(lines.next().is_none(), "exactly two lines for a plain ask");

        let request_value: serde_json::Value = serde_json::from_str(request_line).unwrap();
        let ts_ms = request_value["ts_ms"].as_u64().expect("ts_ms");
        assert_eq!(
            request_line,
            format!(
                r#"{{"event":"request","schema":"baton.exchange/v1","ts_ms":{ts_ms},"model":"claude-test-model","base_url":"https://api.anthropic.com","prompt":"hello"}}"#
            ),
        );

        let response_value: serde_json::Value = serde_json::from_str(response_line).unwrap();
        let r_ts_ms = response_value["ts_ms"].as_u64().expect("ts_ms");
        let duration_ms = response_value["duration_ms"].as_u64().expect("duration_ms");
        assert_eq!(
            response_line,
            format!(
                r#"{{"event":"response_ok","schema":"baton.exchange/v1","ts_ms":{r_ts_ms},"duration_ms":{duration_ms},"reply":"hi there"}}"#
            ),
        );
    }

    #[test]
    fn no_arguments_is_a_no_op() {
        assert_eq!(parse_args(&argv(&[])).unwrap(), None);
    }

    #[test]
    fn version_flags_parse() {
        assert_eq!(
            parse_args(&argv(&["--version"])).unwrap(),
            Some(Command::Version)
        );
        assert_eq!(parse_args(&argv(&["-V"])).unwrap(), Some(Command::Version));
    }

    #[test]
    fn help_flags_parse() {
        assert_eq!(parse_args(&argv(&["--help"])).unwrap(), Some(Command::Help));
        assert_eq!(parse_args(&argv(&["-h"])).unwrap(), Some(Command::Help));
    }

    #[test]
    fn help_text_mentions_ask_usage_and_env_vars() {
        let text = help_text();
        assert!(text.contains("leg ask [--model <model>] <prompt>"));
        assert!(text.contains("ANTHROPIC_API_KEY"));
        assert!(text.contains("LEG_MODEL"));
        assert!(text.contains("LEG_EVENT_LOG"));
    }

    #[test]
    fn ask_parses_positional_prompt() {
        assert_eq!(
            parse_args(&argv(&["ask", "hello"])).unwrap(),
            Some(Command::Ask {
                prompt: "hello".to_string(),
                model: None,
            })
        );
    }

    #[test]
    fn ask_parses_model_override_before_or_after_prompt() {
        assert_eq!(
            parse_args(&argv(&["ask", "--model", "claude-opus-4-8", "hello"])).unwrap(),
            Some(Command::Ask {
                prompt: "hello".to_string(),
                model: Some("claude-opus-4-8".to_string()),
            })
        );
        assert_eq!(
            parse_args(&argv(&["ask", "hello", "--model", "claude-opus-4-8"])).unwrap(),
            Some(Command::Ask {
                prompt: "hello".to_string(),
                model: Some("claude-opus-4-8".to_string()),
            })
        );
    }

    #[test]
    fn ask_without_prompt_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["ask"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn ask_with_blank_prompt_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["ask", "   "])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn ask_with_extra_positional_argument_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["ask", "hello", "extra"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn model_flag_without_value_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["ask", "--model"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn unrecognised_argument_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["bogus"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn session_bare_parses_with_no_resume() {
        assert_eq!(
            parse_args(&argv(&["session"])).unwrap(),
            Some(Command::Session { resume: None })
        );
    }

    #[test]
    fn session_resume_parses_file_and_optional_session_id() {
        assert_eq!(
            parse_args(&argv(&["session", "--resume", "/tmp/x.jsonl"])).unwrap(),
            Some(Command::Session {
                resume: Some(ResumeArgs {
                    file: "/tmp/x.jsonl".to_string(),
                    session_id: None,
                })
            })
        );
        assert_eq!(
            parse_args(&argv(&[
                "session",
                "--resume",
                "/tmp/x.jsonl",
                "--session",
                "sess-1"
            ]))
            .unwrap(),
            Some(Command::Session {
                resume: Some(ResumeArgs {
                    file: "/tmp/x.jsonl".to_string(),
                    session_id: Some("sess-1".to_string()),
                })
            })
        );
    }

    #[test]
    fn session_without_resume_rejects_bare_session_flag() {
        assert!(matches!(
            parse_args(&argv(&["session", "--session", "sess-1"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn log_show_parses_optional_file() {
        assert_eq!(
            parse_args(&argv(&["log", "show"])).unwrap(),
            Some(Command::LogShow { file: None })
        );
        assert_eq!(
            parse_args(&argv(&["log", "show", "--file", "/tmp/x.jsonl"])).unwrap(),
            Some(Command::LogShow {
                file: Some("/tmp/x.jsonl".to_string())
            })
        );
    }

    #[test]
    fn log_replay_parses_optional_file_and_index() {
        assert_eq!(
            parse_args(&argv(&["log", "replay"])).unwrap(),
            Some(Command::LogReplay {
                file: None,
                index: None
            })
        );
        assert_eq!(
            parse_args(&argv(&[
                "log",
                "replay",
                "--index",
                "3",
                "--file",
                "/tmp/x.jsonl"
            ]))
            .unwrap(),
            Some(Command::LogReplay {
                file: Some("/tmp/x.jsonl".to_string()),
                index: Some(3),
            })
        );
    }

    #[test]
    fn log_replay_rejects_zero_index() {
        assert!(matches!(
            parse_args(&argv(&["log", "replay", "--index", "0"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn log_show_rejects_index_flag() {
        assert!(matches!(
            parse_args(&argv(&["log", "show", "--index", "1"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn log_without_subcommand_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["log"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn log_unknown_subcommand_is_usage_error() {
        assert!(matches!(
            parse_args(&argv(&["log", "bogus"])).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn execute_ask_propagates_config_error_without_calling_the_provider() {
        // SAFETY: this crate's tests run single-threaded w.r.t. process env
        // mutation is avoided entirely here — no ANTHROPIC_* var is read
        // through `execute_ask`'s `LegConfig::from_env`, which fails closed
        // when unset in this test process's environment. If some other test
        // or the outer environment happens to export a credential, this test
        // is skipped rather than flaking on shared state.
        if std::env::var("ANTHROPIC_API_KEY").is_ok()
            || std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok()
            || std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok()
        {
            return;
        }
        let mut buf = Vec::new();
        let err = execute_ask("hello", None, &mut buf).unwrap_err();
        assert!(matches!(err, LegError::Config(_)));
        assert!(buf.is_empty());
    }

    #[test]
    fn open_append_sink_on_an_unopenable_path_falls_back_to_noop_without_erroring() {
        // A directory can never be opened as a file for appending — a stand-in
        // for any real-world open failure (bad permissions, missing parent).
        // Recording is additive: this must not propagate an error, only warn.
        let sink = open_append_sink(std::env::temp_dir().to_str().expect("utf8 path"));
        let mut sink = sink;
        let event = ExchangeEvent::request(1, &meta(), "hello");
        sink.record(&event).expect("NoopSink fallback never fails");
    }

    /// A network-free [`crate::transport::http::HttpClient`] fake that
    /// captures the JSON body of the last request it served, via a shared
    /// handle a test retains after the fake is moved into a [`ClaudeClient`].
    struct RecordingHttp {
        captured_body: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    }

    impl crate::transport::http::HttpClient for RecordingHttp {
        fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            body: &str,
        ) -> Result<crate::transport::http::HttpResponse> {
            *self.captured_body.borrow_mut() = Some(body.to_string());
            Ok(crate::transport::http::HttpResponse {
                status: 200,
                body: r#"{"content":[{"type":"text","text":"hi"}]}"#.to_string(),
            })
        }
    }

    /// End-to-end (network-free): `--model` reaches `execute_ask`'s config
    /// override, which is stamped onto the outgoing Claude Messages request.
    #[test]
    fn model_override_reaches_the_outgoing_claude_request() {
        let mut config = LegConfig::from_lookup(|key| {
            (key == "ANTHROPIC_API_KEY").then(|| "secret".to_string())
        })
        .expect("config loads");
        apply_model_override(&mut config, Some("claude-opus-4-8".to_string()));
        let meta = ExchangeMeta {
            model: config.model.clone(),
            base_url: config.base_url.clone(),
        };

        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let http = RecordingHttp {
            captured_body: std::rc::Rc::clone(&captured),
        };
        let client = ClaudeClient::with_http(config, http);
        let participant = LocalParticipant::new(client, meta.clone());

        let mut buf = Vec::new();
        let mut sink = NoopSink;
        run_ask(&participant, &meta, "hello", &mut buf, &mut sink)
            .expect("infallible per Participant contract");

        let sent = captured.borrow().clone().expect("request body captured");
        let value: serde_json::Value = serde_json::from_str(&sent).expect("valid json");
        assert_eq!(value["model"], "claude-opus-4-8");
    }

    #[test]
    fn session_repl_accumulates_history_and_prints_replies() {
        let transport = FakeTransport(Ok(AssistantReply::new("reply-1")));
        let mut output = Vec::new();
        let mut warning = Vec::new();
        let mut sink = NoopSink;
        run_session_repl_with_warning(
            &transport,
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"hello\n".to_vec()),
            &mut output,
            "sess-1".to_string(),
            Conversation::new(),
            0,
            &mut warning,
        )
        .expect("session loop completes");
        assert_eq!(String::from_utf8(output).unwrap(), "reply-1\n");
    }

    #[test]
    fn session_repl_ignores_blank_lines_and_exits_on_exit_command() {
        let transport = FakeTransport(Ok(AssistantReply::new("reply-1")));
        let mut output = Vec::new();
        let mut warning = Vec::new();
        let mut sink = NoopSink;
        run_session_repl_with_warning(
            &transport,
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"\nhello\n\n/exit\nnever sent\n".to_vec()),
            &mut output,
            "sess-1".to_string(),
            Conversation::new(),
            0,
            &mut warning,
        )
        .expect("session loop completes");
        assert_eq!(String::from_utf8(output).unwrap(), "reply-1\n");
    }

    #[test]
    fn session_repl_rolls_back_failed_turn_and_continues() {
        let transport = FakeTransport(Err(LegError::Auth("bad credentials".to_string())));
        let mut output = Vec::new();
        let mut warning = Vec::new();
        let mut sink = NoopSink;
        run_session_repl_with_warning(
            &transport,
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"hello\n".to_vec()),
            &mut output,
            "sess-1".to_string(),
            Conversation::new(),
            0,
            &mut warning,
        )
        .expect("session loop reports the error and continues");
        assert!(String::from_utf8(output).unwrap().is_empty());
    }

    #[test]
    fn session_repl_emits_session_start_turn_and_end_events() {
        let transport = FakeTransport(Ok(AssistantReply::new("reply-1")));
        let mut output = Vec::new();
        let mut trail = Vec::new();
        {
            let mut sink = WriterSink::new(&mut trail);
            execute_session(
                &transport,
                &mut sink,
                &meta(),
                std::io::Cursor::new(b"hello\n".to_vec()),
                &mut output,
                "sess-1".to_string(),
            )
            .expect("session completes");
        }
        let text = String::from_utf8(trail).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        let events: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(events[0]["event"], "session_start");
        assert_eq!(events[1]["event"], "request");
        assert_eq!(events[1]["session_id"], "sess-1");
        assert_eq!(events[1]["turn_index"], 0);
        assert_eq!(events[2]["event"], "response_ok");
        assert_eq!(events[3]["event"], "session_end");
        assert_eq!(events[3]["turns"], 1);
    }

    #[test]
    fn session_repl_sends_full_prior_history_on_a_later_turn() {
        let transport = CapturingTransport::new(vec![
            AssistantReply::new("reply-1"),
            AssistantReply::new("reply-2"),
        ]);
        let mut output = Vec::new();
        let mut warning = Vec::new();
        let mut sink = NoopSink;
        run_session_repl_with_warning(
            &transport,
            &mut sink,
            &meta(),
            std::io::Cursor::new(b"turn one\nturn two\n".to_vec()),
            &mut output,
            "sess-1".to_string(),
            Conversation::new(),
            0,
            &mut warning,
        )
        .expect("session loop completes");

        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec![crate::model::Message::user("turn one")]);
        assert_eq!(
            calls[1],
            vec![
                crate::model::Message::user("turn one"),
                crate::model::Message::assistant("reply-1"),
                crate::model::Message::user("turn two"),
            ],
            "the second call must carry the full prior history, not just the new turn"
        );
        assert_eq!(String::from_utf8(output).unwrap(), "reply-1\nreply-2\n");
    }

    /// End-to-end resume cycle: a first session run writes a real
    /// [`WriterSink`] trail; that exact trail is re-parsed with
    /// [`crate::log::parse_sessions`] and rehydrated with
    /// [`select_and_rehydrate`] (no manually constructed [`SessionRecord`]);
    /// the resumed run's next provider call must then carry the rehydrated
    /// history plus the new turn, and its own trail must continue the same
    /// `session_id` at the next `turn_index` with no fresh `session_start`.
    #[test]
    fn session_resume_round_trips_history_and_turn_index_through_a_real_trail() {
        let first_transport = CapturingTransport::new(vec![AssistantReply::new("reply-1")]);
        let mut first_output = Vec::new();
        let mut trail = Vec::new();
        {
            let mut sink = WriterSink::new(&mut trail);
            execute_session(
                &first_transport,
                &mut sink,
                &meta(),
                std::io::Cursor::new(b"hello\n".to_vec()),
                &mut first_output,
                "sess-1".to_string(),
            )
            .expect("first session run completes");
        }

        let report =
            crate::log::parse_sessions(std::io::Cursor::new(trail.clone())).expect("parses");
        let resumed = select_and_rehydrate(report.sessions, None).expect("rehydrates");
        assert_eq!(resumed.session_id, "sess-1");
        assert_eq!(resumed.next_turn_index, 1);
        assert_eq!(
            resumed.conversation.messages(),
            &[
                crate::model::Message::user("hello"),
                crate::model::Message::assistant("reply-1"),
            ]
        );

        let second_transport = CapturingTransport::new(vec![AssistantReply::new("reply-2")]);
        let mut second_output = Vec::new();
        let mut resumed_trail = Vec::new();
        {
            let mut sink = WriterSink::new(&mut resumed_trail);
            execute_session_resumed(
                &second_transport,
                &mut sink,
                &meta(),
                std::io::Cursor::new(b"again\n".to_vec()),
                &mut second_output,
                resumed,
            )
            .expect("resumed session run completes");
        }

        let calls = second_transport.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec![
                crate::model::Message::user("hello"),
                crate::model::Message::assistant("reply-1"),
                crate::model::Message::user("again"),
            ],
            "the resumed call must carry the rehydrated history plus the new turn"
        );
        assert_eq!(String::from_utf8(second_output).unwrap(), "reply-2\n");

        let resumed_text = String::from_utf8(resumed_trail).unwrap();
        let resumed_lines: Vec<&str> = resumed_text.lines().collect();
        assert_eq!(
            resumed_lines.len(),
            3,
            "resuming emits no fresh session_start: {resumed_text}"
        );
        let request_event: serde_json::Value = serde_json::from_str(resumed_lines[0]).unwrap();
        assert_eq!(request_event["event"], "request");
        assert_eq!(request_event["session_id"], "sess-1");
        assert_eq!(request_event["turn_index"], 1);
        let end_event: serde_json::Value = serde_json::from_str(resumed_lines[2]).unwrap();
        assert_eq!(end_event["event"], "session_end");
        assert_eq!(end_event["session_id"], "sess-1");
        assert_eq!(end_event["turns"], 2);
    }

    #[test]
    fn select_and_rehydrate_restores_conversation_and_next_turn_index() {
        let sessions = vec![crate::log::SessionRecord {
            session_id: "sess-1".to_string(),
            started: true,
            ended: true,
            declared_turns: Some(2),
            turns: vec![
                crate::log::SessionTurn {
                    request: crate::events::RequestRecord {
                        ts_ms: 1,
                        model: "m".to_string(),
                        base_url: "u".to_string(),
                        prompt: "hi".to_string(),
                        session_id: Some("sess-1".to_string()),
                        turn_index: Some(0),
                    },
                    outcome: Some(Outcome::Ok {
                        ts_ms: 2,
                        duration_ms: 1,
                        reply: "hello".to_string(),
                        input_tokens: None,
                        output_tokens: None,
                        stop_reason: None,
                        session_id: Some("sess-1".to_string()),
                        turn_index: Some(0),
                    }),
                },
                crate::log::SessionTurn {
                    request: crate::events::RequestRecord {
                        ts_ms: 3,
                        model: "m".to_string(),
                        base_url: "u".to_string(),
                        prompt: "failed".to_string(),
                        session_id: Some("sess-1".to_string()),
                        turn_index: Some(1),
                    },
                    outcome: None,
                },
            ],
        }];

        let resumed = select_and_rehydrate(sessions, None).expect("rehydrates");
        assert_eq!(resumed.session_id, "sess-1");
        assert_eq!(resumed.next_turn_index, 2);
        assert_eq!(resumed.conversation.len(), 2);
    }

    #[test]
    fn select_and_rehydrate_requires_session_id_when_ambiguous() {
        let sessions = vec![
            crate::log::SessionRecord {
                session_id: "sess-1".to_string(),
                started: true,
                ended: true,
                declared_turns: Some(0),
                turns: vec![],
            },
            crate::log::SessionRecord {
                session_id: "sess-2".to_string(),
                started: true,
                ended: true,
                declared_turns: Some(0),
                turns: vec![],
            },
        ];
        assert!(matches!(
            select_and_rehydrate(sessions, None).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn select_and_rehydrate_errors_on_empty_trail() {
        assert!(matches!(
            select_and_rehydrate(vec![], None).unwrap_err(),
            LegError::Usage(_)
        ));
    }

    #[test]
    fn execute_log_show_writes_a_block_per_exchange() {
        let exchanges = vec![Exchange {
            request: crate::events::RequestRecord {
                ts_ms: 1_700_000_000_000,
                model: "m".to_string(),
                base_url: "u".to_string(),
                prompt: "hello".to_string(),
                session_id: None,
                turn_index: None,
            },
            outcome: Outcome::Ok {
                ts_ms: 1_700_000_000_420,
                duration_ms: 418,
                reply: "hi there".to_string(),
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                session_id: None,
                turn_index: None,
            },
        }];
        let mut buf = Vec::new();
        execute_log_show(&exchanges, &mut buf).expect("writes");
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("#1"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn execute_log_show_on_empty_log_writes_nothing() {
        let mut buf = Vec::new();
        execute_log_show(&[], &mut buf).expect("writes");
        assert!(buf.is_empty());
    }

    #[test]
    fn select_exchange_defaults_to_last_and_validates_range() {
        let exchanges = vec![
            Exchange {
                request: crate::events::RequestRecord {
                    ts_ms: 1,
                    model: "m".to_string(),
                    base_url: "u".to_string(),
                    prompt: "a".to_string(),
                    session_id: None,
                    turn_index: None,
                },
                outcome: Outcome::Ok {
                    ts_ms: 2,
                    duration_ms: 1,
                    reply: "ra".to_string(),
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    session_id: None,
                    turn_index: None,
                },
            },
            Exchange {
                request: crate::events::RequestRecord {
                    ts_ms: 3,
                    model: "m".to_string(),
                    base_url: "u".to_string(),
                    prompt: "b".to_string(),
                    session_id: None,
                    turn_index: None,
                },
                outcome: Outcome::Ok {
                    ts_ms: 4,
                    duration_ms: 1,
                    reply: "rb".to_string(),
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    session_id: None,
                    turn_index: None,
                },
            },
        ];
        assert_eq!(
            select_exchange(&exchanges, None).unwrap().request.prompt,
            "b"
        );
        assert_eq!(
            select_exchange(&exchanges, Some(1)).unwrap().request.prompt,
            "a"
        );
        assert!(select_exchange(&exchanges, Some(3)).is_err());
        assert!(select_exchange(&[], None).is_err());
    }

    // -- `leg exchange` --------------------------------------------------

    #[test]
    fn parse_args_exchange_bare_defaults_both_paths_to_none() {
        assert_eq!(
            parse_args(&argv(&["exchange"])).unwrap(),
            Some(Command::Exchange {
                in_path: None,
                out_path: None,
            })
        );
    }

    #[test]
    fn parse_args_exchange_accepts_in_and_out() {
        assert_eq!(
            parse_args(&argv(&["exchange", "--in", "/tmp/a", "--out", "/tmp/b"])).unwrap(),
            Some(Command::Exchange {
                in_path: Some("/tmp/a".to_string()),
                out_path: Some("/tmp/b".to_string()),
            })
        );
    }

    #[test]
    fn parse_args_exchange_missing_in_value_is_usage_error() {
        assert!(parse_args(&argv(&["exchange", "--in"])).is_err());
    }

    #[test]
    fn parse_args_exchange_unexpected_argument_is_usage_error() {
        assert!(parse_args(&argv(&["exchange", "--who"])).is_err());
    }

    #[test]
    fn parse_exchange_request_valid_envelope_passes_through_unchanged() {
        let envelope = MessageEnvelope::new(
            "m-1",
            "c-1",
            "user",
            "assistant",
            MessageKind::Request,
            "hello",
            1_700_000_000_000,
        );
        let raw = serde_json::to_string(&envelope).unwrap();
        let (request, mode) = parse_exchange_request(&raw);
        assert_eq!(mode, ExchangeMode::Envelope);
        assert_eq!(request, envelope);
    }

    #[test]
    fn parse_exchange_request_plain_text_synthesizes_a_request() {
        let (request, mode) = parse_exchange_request("hello there\n");
        assert_eq!(mode, ExchangeMode::PlainText);
        assert_eq!(request.body, "hello there");
        assert_eq!(request.kind, MessageKind::Request);
    }

    #[test]
    fn parse_exchange_request_trims_trailing_newline_only_preserves_trailing_spaces() {
        let (request, _) = parse_exchange_request("hello   \r\n");
        assert_eq!(request.body, "hello   ");
    }

    #[test]
    fn execute_exchange_core_envelope_mode_success_writes_full_response_envelope() {
        let participant =
            LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
        let envelope = MessageEnvelope::new(
            "m-1",
            "c-1",
            "user",
            "assistant",
            MessageKind::Request,
            "hello",
            1_700_000_000_000,
        );
        let raw = serde_json::to_string(&envelope).unwrap();
        let mut buf = Vec::new();
        execute_exchange_core(&participant, &raw, &mut buf).expect("infallible");
        let printed = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim()).expect("valid json");
        assert_eq!(value["kind"], "response");
        assert_eq!(value["body"], "hi there");
        assert_eq!(value["exchange"]["schema"], crate::events::SCHEMA);
    }

    #[test]
    fn execute_exchange_core_envelope_mode_error_still_writes_full_error_envelope() {
        let participant = LocalParticipant::new(
            FakeTransport(Err(LegError::Auth("bad credentials".to_string()))),
            meta(),
        );
        let envelope = MessageEnvelope::new(
            "m-1",
            "c-1",
            "user",
            "assistant",
            MessageKind::Request,
            "hello",
            1_700_000_000_000,
        );
        let raw = serde_json::to_string(&envelope).unwrap();
        let mut buf = Vec::new();
        execute_exchange_core(&participant, &raw, &mut buf).expect("infallible");
        let printed = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim()).expect("valid json");
        assert_eq!(value["kind"], "error");
        assert_eq!(value["body"], "authentication error: bad credentials");
    }

    #[test]
    fn execute_exchange_core_plain_text_success_writes_only_the_reply_body() {
        let participant =
            LocalParticipant::new(FakeTransport(Ok(AssistantReply::new("hi there"))), meta());
        let mut buf = Vec::new();
        execute_exchange_core(&participant, "hello", &mut buf).expect("infallible");
        assert_eq!(String::from_utf8(buf).unwrap(), "hi there\n");
    }

    #[test]
    fn execute_exchange_core_plain_text_failure_writes_nothing_and_still_returns_ok() {
        let participant = LocalParticipant::new(
            FakeTransport(Err(LegError::Auth("bad credentials".to_string()))),
            meta(),
        );
        let mut buf = Vec::new();
        execute_exchange_core(&participant, "hello", &mut buf)
            .expect("delivered errors never propagate as Err");
        assert!(
            buf.is_empty(),
            "plain-text failure must leave stdout empty so baton's machinery-error path fires"
        );
    }
}
