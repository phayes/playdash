#![doc = include_str!("../README.md")]
#![warn(missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(feature = "encryption")]
mod cenc;
#[cfg(any(feature = "sync", feature = "async"))]
pub mod codec;
#[cfg(any(feature = "sync", feature = "async"))]
pub mod decoder;
mod error;
pub mod fragment;
mod manifest;
mod mp4;
#[cfg(feature = "async")]
pub mod nonblocking;
#[cfg(any(feature = "sync", feature = "async"))]
pub mod packet;
#[cfg(feature = "rodio")]
mod rodio;
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
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use fragment::buffer::{BufferAmount, BufferHandle, BufferStatus, BufferTotal};
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use fragment::schedule::{Retention, TimeAccuracy};
#[cfg(feature = "sync")]
#[doc(inline)]
pub use fragment::{FragmentCache, Transport};
#[doc(inline)]
pub use manifest::{ContentKind, DashManifest, FrameRate, Representation};
#[cfg(any(feature = "sync", feature = "async"))]
#[doc(inline)]
pub use mp4::codec::Codec;
#[cfg(feature = "rodio")]
#[doc(inline)]
pub use rodio::DashSource;

/// The byte buffer fragment bodies come in, re-exported for implementing a
/// `Transport` without depending on the `bytes` crate directly.
#[cfg(any(feature = "sync", feature = "async"))]
pub use bytes::Bytes;
