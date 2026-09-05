//! leg: a standalone, agent-friendly headless LLM client.
//!
//! This slice ports the local-LLM-agent half of `baton`'s core — the
//! infallible [`participant::Participant`] contract plus a single-turn `ask`
//! driver — without the harness (mailbox/service/registry/converse-ring).
//!
//! - [`config`] — environment-backed runtime configuration.
//! - [`model`] — typed prompt/reply structures.
//! - [`transport`] — the provider transport boundary.
//! - [`events`] — the exchange-record types nested in a peer message.
//! - [`message`] — the `baton.message/v1` peer-message envelope.
//! - [`participant`] — the envelope-in / envelope-out participant seam.
//! - [`error`] — shared error and result types.
//! - [`cli`] — the command-line entry surface.

pub mod cli;
pub mod config;
pub mod error;
pub mod events;
pub mod message;
pub mod model;
pub mod participant;
pub mod transport;
