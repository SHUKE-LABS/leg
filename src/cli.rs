//! The command-line entry surface.

use std::io::Write;

use crate::config::LegConfig;
use crate::error::{LegError, Result};
use crate::events::ExchangeMeta;
use crate::message::{MessageEnvelope, MessageKind};
use crate::participant::{LocalParticipant, Participant};
use crate::transport::claude::ClaudeClient;

/// The one-line usage summary, shared by `--help` output and usage errors.
const USAGE: &str = "usage: leg [--version|-V] [--help|-h] | leg ask [--model <model>] <prompt>";

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
    }
}

/// The full `--help` body: the usage summary plus the env vars `ask` reads.
fn help_text() -> String {
    format!(
        "{USAGE}\n\n\
         Reads credentials from ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN /\n\
         CLAUDE_CODE_OAUTH_TOKEN). Also honours ANTHROPIC_BASE_URL, LEG_MODEL,\n\
         LEG_TIMEOUT_SECS, LEG_MAX_TOKENS, and LEG_SYSTEM_PROMPT."
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
    let meta = ExchangeMeta {
        model: config.model.clone(),
        base_url: config.base_url.clone(),
    };
    let client = ClaudeClient::from_config(config);
    let participant = LocalParticipant::new(client, meta);
    run_ask(&participant, prompt, output)
}

/// Testable core of [`execute_ask`], parameterised over a [`Participant`] so
/// the success/error stdout contract is exercisable without a network.
fn run_ask(participant: &impl Participant, prompt: &str, mut output: impl Write) -> Result<()> {
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

    match response.kind {
        MessageKind::Response => writeln!(output, "{}", response.body).map_err(io_err),
        _ => {
            let json = serde_json::to_string(&response).expect("MessageEnvelope always serializes");
            writeln!(output, "{json}").map_err(io_err)
        }
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
        run_ask(&participant, "hello", &mut buf).expect("infallible per Participant contract");
        assert_eq!(String::from_utf8(buf).unwrap(), "hi there\n");
    }

    #[test]
    fn run_ask_prints_error_envelope_json_on_delivery_failure_and_does_not_err() {
        let participant = LocalParticipant::new(
            FakeTransport(Err(LegError::Auth("bad credentials".to_string()))),
            meta(),
        );
        let mut buf = Vec::new();
        run_ask(&participant, "hello", &mut buf).expect("infallible per Participant contract");
        let printed = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim()).expect("valid json");
        assert_eq!(value["kind"], "error");
        assert_eq!(value["body"], "authentication error: bad credentials");
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
        let participant = LocalParticipant::new(client, meta);

        let mut buf = Vec::new();
        run_ask(&participant, "hello", &mut buf).expect("infallible per Participant contract");

        let sent = captured.borrow().clone().expect("request body captured");
        let value: serde_json::Value = serde_json::from_str(&sent).expect("valid json");
        assert_eq!(value["model"], "claude-opus-4-8");
    }
}
