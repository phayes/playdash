#![doc = include_str!("../README.md")]
#![warn(missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(any(feature = "sync", feature = "async"))]
mod buffer;
#[cfg(feature = "encryption")]
mod cenc;
mod error;
mod fragment;
mod manifest;
#[cfg(feature = "async")]
pub mod nonblocking;
#[cfg(feature = "rodio")]
mod rodio;
#[cfg(any(feature = "sync", feature = "async"))]
mod schedule;
#[cfg(any(feature = "sync", feature = "async"))]
mod store;
#[cfg(feature = "sync")]
mod stream;
mod vendor;

#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use buffer::{BufferAmount, BufferHandle, BufferStatus, BufferTotal};
#[cfg(feature = "encryption")]
#[doc(inline)]
pub use cenc::ContentKeys;
#[doc(inline)]
pub use error::Error;
#[doc(inline)]
pub use fragment::{Fragment, Fragments, MediaTimeline};
#[doc(inline)]
pub use manifest::{ContentKind, DashManifest, FrameRate, Representation};
#[cfg(feature = "rodio")]
#[doc(inline)]
pub use rodio::DashSource;
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use schedule::{Position, Retention, TimeAccuracy, TimePosition};
#[cfg(feature = "sync")]
#[doc(inline)]
pub use stream::{FragmentCache, FragmentStream, Transport};

/// The byte buffer fragment bodies come in, re-exported for implementing a
/// `Transport` without depending on the `bytes` crate directly.
#[cfg(any(feature = "sync", feature = "async"))]
pub use bytes::Bytes;
