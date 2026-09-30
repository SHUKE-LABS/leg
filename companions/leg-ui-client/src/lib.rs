//! Shared native `leg` process controller for human companion interfaces.

mod catalog;
mod client;
mod protocol;
mod resolve;
pub mod supervisor;

pub use catalog::{
    CatalogError, CatalogRunState, CatalogSession, CatalogTurn, CatalogTurnError, RetryIntent,
    SessionCatalog, SessionCatalogConfig, SessionInterface, TrailOutcome, TrailTool,
    TrailToolResult, TrailTurn,
};
pub use client::{
    Client, ClientConfig, ClientError, LegSession, StartError, TurnHandle, TurnOutcome, TurnRequest,
};
pub use protocol::{StreamEvent, StreamFailure};
pub use resolve::ResolveError;
