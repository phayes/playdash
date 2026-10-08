#![doc = include_str!("../README.md")]

#[cfg(feature = "encryption")]
mod cenc;
pub mod error;
pub mod manifest;
mod manifest_segment_list;
mod manifest_template;
#[cfg(feature = "rodio")]
pub mod rodio;
pub mod stream;

#[cfg(feature = "encryption")]
pub use cenc::ContentKeys;
pub use dash_mpd_core;
pub use error::Error;
pub use manifest::DashManifest;
#[cfg(feature = "rodio")]
pub use rodio::DashSource;
pub use stream::{
    Fragment, FragmentCache, MediaTimeline, MpegStreamReader, Position, TimeAccuracy, TimePosition,
    Transport,
};
