//! The agent loop, message and content-block model, context compaction.

pub mod message;

pub use message::{CacheControl, CacheControlKind, ContentBlock, Message, Role};
