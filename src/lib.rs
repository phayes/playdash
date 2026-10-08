#![doc = include_str!("../README.md")]
#![warn(missing_docs, rustdoc::broken_intra_doc_links)]

#[cfg(feature = "encryption")]
mod cenc;
mod error;
mod manifest;
mod manifest_segment_base;
mod manifest_segment_list;
mod manifest_template;
#[cfg(feature = "rodio")]
mod rodio;
mod stream;

#[cfg(feature = "encryption")]
#[doc(inline)]
pub use cenc::ContentKeys;
#[doc(inline)]
pub use error::Error;
#[doc(inline)]
pub use manifest::{DashManifest, Representation};
#[cfg(feature = "rodio")]
#[doc(inline)]
pub use rodio::DashSource;
#[doc(inline)]
pub use stream::{
    Fragment, FragmentCache, MediaTimeline, Position, StreamReader, TimeAccuracy, TimePosition,
    Transport,
};
