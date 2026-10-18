//! Lower-level access to compressed packets exactly as MP4 stores them.
//!
//! Most applications should use [`MediaStream`](crate::MediaStream), which converts
//! these packets into decoder-friendly formats. Use [`PacketStream`] when you need the
//! original MP4 samples or want to perform codec conversion yourself.

#[cfg(feature = "sync")]
mod stream;

#[doc(inline)]
pub use crate::mp4::packet::{Packet, PacketSeeked};
#[doc(inline)]
pub use crate::mp4::track::Track;
#[cfg(feature = "sync")]
pub use stream::PacketStream;
