//! ISO base media file format (MP4) parsing for fragmented media: initialization and
//! media segments, segment indexes, and the boxes Common Encryption adds.

// Each part is used by whichever features need it: `sidx` by the manifest, packets by
// `sync` and `async`, and protection boxes by `encryption`.
#![cfg_attr(not(any(feature = "sync", feature = "async")), allow(dead_code))]

#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod codec;
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod demux;
pub(crate) mod fragment;
pub(crate) mod init;
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod packet;
#[cfg(feature = "encryption")]
pub(crate) mod protection;
pub(crate) mod reader;
pub(crate) mod sidx;
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod track;
