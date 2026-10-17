//! A representation's decoder-ready packets: the blocking `DecoderPacketStream` over a
//! `PacketStream`.

use super::adapter::Choice;
use super::{Adapter, DecoderInitialization, DecoderPacket, DecoderSeeked};
use crate::buffer::BufferHandle;
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::packet_stream::PacketStream;
use crate::schedule::TimeAccuracy;
use std::time::Duration;

/// A track's compressed packets in the form its codec's decoders read: an [`Iterator`]
/// of [`DecoderPacket`]s in decode order, with the [`DecoderInitialization`] to
/// configure a decoder with first, and time seeking.
///
/// Made by [`PacketStream::into_decoder_packets`], which chooses the elementary format of
/// the codec, [`PacketStream::into_decoder_packets_for`] or
/// [`PacketStream::into_decoder_packets_as`].
///
/// After an error, iteration yields it once and then ends, until a seek succeeds.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use playdash::{DashManifest, PacketStream, TimeAccuracy};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
/// let mut input = PacketStream::new(manifest.stream("video")?)?.into_decoder_packets()?;
/// let initialization = input.initialization().clone();
/// // configure the decoder with initialization.data and initialization.parameters
/// # let _ = initialization;
///
/// // After a seek, reset and reconfigure the decoder, then prime it with the pre-roll.
/// let seeked = input.seek_time(Duration::from_secs(30), TimeAccuracy::Packet)?;
/// for packet in seeked.preroll {
///     # let _ = packet;
///     // decode packet.data, and discard the output
/// }
/// for packet in input {
///     let packet = packet?;
///     // decode packet.data, discarding output presented before seeked.discard_before
///     # let _ = packet;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct DecoderPacketStream {
    packets: PacketStream,
    adapter: Adapter,
    /// Set once iteration has yielded an error, so it ends rather than carrying on
    /// past it.
    failed: bool,
}

impl DecoderPacketStream {
    /// The packets of `packets` in the format `choice` asks for.
    pub(crate) fn new(packets: PacketStream, choice: Choice) -> Result<Self, Error> {
        let adapter = Adapter::new(packets.codec(), choice)?;
        Ok(Self {
            packets,
            adapter,
            failed: false,
        })
    }

    /// The initialization to configure a decoder with before the first packet, and again
    /// after every seek.
    pub fn initialization(&self) -> &DecoderInitialization {
        self.adapter.initialization()
    }

    /// The track's codec and its MP4 decoder configuration.
    pub fn codec(&self) -> &Codec {
        self.packets.codec()
    }

    /// Ticks per second of packet timestamps.
    pub fn timescale(&self) -> u32 {
        self.packets.timescale()
    }

    /// The MP4 track ID of the track read.
    pub fn track_id(&self) -> u32 {
        self.packets.track_id()
    }

    /// Media decoded before a seek target; see [`PacketStream::preroll`].
    pub fn preroll(&self) -> Duration {
        self.packets.preroll()
    }

    /// A handle reporting how much of the representation the cache holds; see
    /// [`FragmentStream::buffer`](crate::FragmentStream::buffer).
    pub fn buffer(&self) -> Option<BufferHandle> {
        self.packets.buffer()
    }

    /// Seeks to `time` on the representation's media timeline, and returns the packets
    /// to decode before the stream's next one. Lands where [`PacketStream::seek_time`]
    /// does.
    ///
    /// Fails like [`PacketStream::seek_time`], and with [`Error::MalformedDecoderInput`]
    /// if a pre-roll packet cannot be converted, after which iteration yields nothing
    /// until a seek succeeds.
    pub fn seek_time(
        &mut self,
        time: Duration,
        accuracy: TimeAccuracy,
    ) -> Result<DecoderSeeked, Error> {
        let seeked = self.packets.seek_time(time, accuracy)?;
        let seeked = self.adapter.seeked(seeked);
        self.failed = seeked.is_err();
        seeked
    }
}

impl Iterator for DecoderPacketStream {
    type Item = Result<DecoderPacket, Error>;

    /// The next packet in decode order. Blocks while its fragment downloads.
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let packet = self
            .packets
            .next()?
            .and_then(|packet| self.adapter.convert(packet));
        self.failed = packet.is_err();
        Some(packet)
    }
}
