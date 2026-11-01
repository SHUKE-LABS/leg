//! The capability bridge: the versioned, host-mediated contract a mobile
//! guest uses to ask the host for a native capability.
//!
//! The bridge is deliberately narrow:
//!
//! - a request is one line of JSON with a version, a request id, a capability
//!   name, and a structured input; the answer is one line of JSON with exactly
//!   one of six statuses ([`protocol::BridgeStatus`]);
//! - every request is validated before it reaches the host, so an unknown
//!   version, an unknown capability, an unknown field, a malformed document,
//!   an oversized request, a duplicate request id, and a disallowed path form
//!   are all refused without a dispatch ([`protocol`]);
//! - the host owns the transport, the permission prompt, the dispatch, the
//!   cancellation, and the user confirmation; the bridge only owns the
//!   protocol, the approval binding, and the audit summary ([`adapter`],
//!   [`service`]);
//! - the bridge logs an audit summary that carries request metadata and input
//!   *shape* — never provider credentials, photo contents, file contents, or
//!   shortcut inputs ([`state::AuditLog`]).
//!
//! The slice ships no platform adapter: [`adapter::FileHostAdapter`] is the
//! transport seam a host fills in, and the capability table names the
//! capabilities a later mobile slice implements.

pub mod adapter;
pub mod protocol;
pub mod service;
pub mod state;

#[cfg(test)]
mod tests;

use crate::error::Result;

/// Installs the interrupt handler the bridge uses to cancel a request.
///
/// The handler lives in the crate-private interrupt module; the bridge
/// exposes it so the `leg-bridge` binary can arm it before it waits for the
/// host.
pub fn install_interrupt_handler() -> Result<()> {
    crate::interrupt::install()
}
