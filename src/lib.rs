//! Parse TIDAL MPEG-DASH manifests and prepare them for progressive playback.

pub mod error;
pub mod manifest;
#[cfg(feature = "rodio")]
pub mod rodio;
pub mod stream;

pub use error::Error;
pub use manifest::{DashManifest, DashRepresentation, DashSegment};
#[cfg(feature = "rodio")]
pub use rodio::DashSource;
pub use stream::{MediaTimeline, MpegStreamReader, Position, TimeAccuracy, TimePosition};
