//! A representation's decoder-ready packets: the async `DecoderStream` over a
//! `nonblocking::PacketStream`.

use super::adapter::Choice;
use super::{Adapter, DecoderInitialization, DecoderPacket, DecoderSeeked};
use crate::buffer::BufferHandle;
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::nonblocking::PacketStream;
use crate::schedule::TimeAccuracy;
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

/// A track's compressed packets in the form its codec's decoders read: a [`Stream`] of
/// [`DecoderPacket`]s in decode order, with the [`DecoderInitialization`] to configure a
/// decoder with first, and time seeking. The async counterpart of the blocking
/// `playdash::DecoderStream`.
///
/// Made by [`PacketStream::into_decoder_stream`], which chooses the elementary format of
/// the codec, [`PacketStream::into_decoder_stream_for`] or
/// [`PacketStream::into_decoder_stream_as`].
///
/// After an error, the stream yields it once and then ends, until a seek succeeds.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use futures_util::StreamExt;
/// use playdash::TimeAccuracy;
/// use playdash::nonblocking::{DashManifest, PacketStream};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
/// let packets = PacketStream::new(manifest.stream("video").await?).await?;
/// let mut input = packets.into_decoder_stream()?;
/// let initialization = input.initialization().clone();
/// // configure the decoder with initialization.data and initialization.parameters
/// # let _ = initialization;
///
/// let seeked = input.seek_time(Duration::from_secs(30), TimeAccuracy::Packet).await?;
/// # let _ = seeked;
/// while let Some(packet) = input.next().await {
///     let packet = packet?;
///     // decode packet.data
///     # let _ = packet;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct DecoderStream {
    packets: PacketStream,
    adapter: Adapter,
    /// Set once the stream has yielded an error, so it ends rather than carrying on past
    /// it.
    failed: bool,
}

impl DecoderStream {
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
    /// [`FragmentStream::buffer`](crate::nonblocking::FragmentStream::buffer).
    pub fn buffer(&self) -> Option<BufferHandle> {
        self.packets.buffer()
    }

    /// Seeks to `time` on the representation's media timeline, and returns the packets
    /// to decode before the stream's next one. Lands where [`PacketStream::seek_time`]
    /// does.
    ///
    /// Fails like [`PacketStream::seek_time`], and with [`Error::MalformedDecoderInput`]
    /// if a pre-roll packet cannot be converted, after which the stream yields nothing
    /// until a seek succeeds.
    pub async fn seek_time(
        &mut self,
        time: Duration,
        accuracy: TimeAccuracy,
    ) -> Result<DecoderSeeked, Error> {
        let seeked = self.packets.seek_time(time, accuracy).await?;
        let seeked = self.adapter.seeked(seeked);
        self.failed = seeked.is_err();
        seeked
    }
}

impl Stream for DecoderStream {
    type Item = Result<DecoderPacket, Error>;

    /// The next packet in decode order.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(None);
        }
        let Some(packet) = ready!(Pin::new(&mut this.packets).poll_next(cx)) else {
            return Poll::Ready(None);
        };
        let packet = packet.and_then(|packet| this.adapter.convert(packet));
        this.failed = packet.is_err();
        Poll::Ready(Some(packet))
    }
}
