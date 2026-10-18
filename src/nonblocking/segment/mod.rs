//! Async, lower-level access to DASH segments, caching, and transport.
//!
//! Most applications should use [`MediaStream`](crate::nonblocking::MediaStream). Use
//! this module for complete segment bodies or a seekable async byte stream.

mod cache;
mod manifest;
mod stream;
mod transport;

pub use cache::SegmentCache;
pub use stream::SegmentStream;
pub use transport::Transport;
