//! The command-line entry surface.

use std::io::Write;

use crate::config::LegConfig;
use crate::error::{LegError, Result};
use crate::events::ExchangeMeta;
use crate::message::{MessageEnvelope, MessageKind};
use crate::participant::{LocalParticipant, Participant};
use crate::transport::claude::ClaudeClient;

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Prints the crate version.
    Version,
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
        Some(Command::Ask { prompt, model }) => {
            let stdout = std::io::stdout();
            execute_ask(&prompt, model, stdout.lock())
        }
    }
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
        "ask" => parse_ask(iter).map(Some),
        other => Err(LegError::Usage(format!(
            "unrecognised argument {other:?}; expected \"ask\", \"--version\", or no arguments"
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
    if let Some(model) = model {
        config.model = model;
    }
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
}
