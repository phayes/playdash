#![doc = include_str!("../README.md")]
#![warn(missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(feature = "encryption")]
mod cenc;
#[cfg(any(feature = "sync", feature = "async"))]
pub mod codec;
#[cfg(any(feature = "sync", feature = "async"))]
pub mod decoder;
mod error;
mod manifest;
mod mp4;
#[cfg(feature = "async")]
pub mod nonblocking;
#[cfg(any(feature = "sync", feature = "async"))]
pub mod packet;
#[cfg(feature = "rodio")]
mod rodio;
pub mod segment;
mod vendor;

#[cfg(feature = "encryption")]
#[doc(inline)]
pub use cenc::ContentKeys;
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use decoder::{
    DecoderFormat, DecoderInitialization, DecoderParameters, DecoderStyle, MediaPacket, Seeked,
};
#[cfg(feature = "sync")]
#[doc(inline)]
pub use decoder::{MediaStream, MediaStreamBuilder};
#[doc(inline)]
pub use error::Error;
#[doc(inline)]
pub use manifest::{ContentKind, DashManifest, FrameRate, LiveDelay, Representation};
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use mp4::codec::Codec;
#[cfg(feature = "rodio")]
#[doc(inline)]
pub use rodio::DashSource;
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use segment::buffer::{BufferAmount, BufferHandle, BufferStatus, BufferTotal};
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use segment::schedule::{Retention, TimeAccuracy};
#[cfg(feature = "sync")]
#[doc(inline)]
pub use segment::{SegmentCache, Transport};

/// The byte buffer used by lower-level segment transports.
///
/// This is re-exported so custom [`Transport`] implementations do not need a direct
/// dependency on the `bytes` crate.
#[cfg(any(feature = "sync", feature = "async"))]
pub use bytes::Bytes;
