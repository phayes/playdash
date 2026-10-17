//! A representation's packets as MP4 stores them: [`Packet`]s and the [`Track`]s they
//! belong to, and with the `sync` feature, the `PacketStream` that demuxes them from a
//! `FragmentStream`.

#[cfg(feature = "sync")]
mod stream;

#[doc(inline)]
pub use crate::mp4::packet::{Packet, PacketSeeked};
#[doc(inline)]
pub use crate::mp4::track::Track;
#[cfg(feature = "sync")]
pub use stream::PacketStream;
