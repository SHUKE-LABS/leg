//! Shared native `leg` process controller for human companion interfaces.

mod catalog;
mod client;
mod content;
mod draft;
mod process_owner;
mod protocol;
mod resolve;
pub mod supervisor;

pub use catalog::{
    CatalogError, CatalogRunState, CatalogSession, CatalogTurn, CatalogTurnError,
    DirectoryAddition, RetryIntent, SessionCatalog, SessionCatalogConfig, SessionInterface,
    TrailOutcome, TrailTool, TrailToolResult, TrailTurn,
};
pub use client::{
    Client, ClientConfig, ClientError, LegSession, StartError, TurnHandle, TurnOutcome,
    TurnRequest, TurnStopHandle,
};
pub use content::{
    AttachmentRef, ContentBlock, ImageSource, MAX_IMAGE_ENCODED_BYTES,
    MAX_TURN_IMAGE_ENCODED_BYTES, base64_encoded_len, detect_image_media_type, turn_fingerprint,
};
pub use draft::{
    AttachmentError, CLIPBOARD_IMAGE_LABEL, DraftBlock, DraftImage, DraftSubmission,
    SubmissionClaim, SubmissionResolution, attachment_label, draft_content, draft_fingerprint,
    draft_images, draft_prompt,
};
pub use protocol::{StreamEvent, StreamFailure};
pub use resolve::ResolveError;
