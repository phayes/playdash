//! Async, lower-level access to packets stored in fragmented MP4.

use super::MediaStream;
use super::segment::SegmentStream;
use crate::decoder::{Choice, DecoderFormat, DecoderStyle};
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::mp4::demux::{Demuxer, Landing, Loaded, Split};
use crate::packet::{Packet, PacketSeeked};
use crate::segment::buffer::BufferHandle;
use crate::segment::schedule::{Part, PartBody};
use futures_util::Stream;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

/// Demultiplexes one track from a [`SegmentStream`] into MP4 [`Packet`] values.
///
/// This is a lower-level API. Packets retain the byte layout used inside MP4, which is
/// not accepted directly by every decoder. Use [`MediaStream`] for
/// decoder-ready input.
///
/// Packets arrive in decode order and include timestamps, duration, and keyframe
/// information. The stream reads the representation's only track unless you choose one
/// with [`Self::with_track`].
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use futures_util::StreamExt;
/// use playdash::nonblocking::DashManifest;
/// use playdash::nonblocking::packet::PacketStream;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
/// let mut packets = PacketStream::new(manifest.segment_stream("audio").await?).await?;
///
/// let seeked = packets.seek_time(Duration::from_secs(30)).await?;
/// # let _ = seeked;
/// while let Some(packet) = packets.next().await {
///     let packet = packet?;
///     # let _ = packet;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct PacketStream {
    segments: SegmentStream,
    demuxer: Demuxer,
    /// Set once the stream has yielded an error, so it ends rather than
    /// yielding the same error forever.
    failed: bool,
}

impl PacketStream {
    /// Reads the packets of `segments`, a representation of one track, which must not
    /// have been read from yet. Reads its initialization segment before returning.
    ///
    /// Fails with [`Error::UnsupportedMedia`] if the representation has more or fewer
    /// than one track, for which see [`Self::with_track`], or its samples are still
    /// encrypted. Fails with [`Error::MalformedMedia`] for a damaged initialization
    /// segment, and with [`Error::Initialization`] if `segments` has been read from.
    pub async fn new(segments: SegmentStream) -> Result<Self, Error> {
        Self::open(segments, None).await
    }

    /// Reads the packets of track `track_id` of `segments`, one of its
    /// [`SegmentStream::tracks`]. Otherwise like [`Self::new`].
    ///
    /// Fails with [`Error::Initialization`] if the representation has no such track.
    pub async fn with_track(segments: SegmentStream, track_id: u32) -> Result<Self, Error> {
        Self::open(segments, Some(track_id)).await
    }

    /// A stream of `segments`' track `track_id`, or of its only track without one.
    async fn open(mut segments: SegmentStream, track_id: Option<u32>) -> Result<Self, Error> {
        if !segments.at_start() {
            return Err(Error::Initialization(
                "PacketStream needs a SegmentStream that has not been read from".to_owned(),
            ));
        }
        let init = match poll_fn(|cx| segments.poll_next_part(cx)).await {
            Some(Ok(init)) if init.part == Part::Init => init,
            Some(Err(error)) => return Err(error),
            _ => {
                return Err(Error::Initialization(
                    "MPEG stream ended before its initialization segment".to_owned(),
                ));
            }
        };
        let representation = segments.representation_index();
        let demuxer = Demuxer::new(init.body, &init.location, Some(representation), track_id)?;
        Ok(Self {
            segments,
            demuxer,
            failed: false,
        })
    }

    /// Decodes `preroll` of media before a seek target, instead of what the media or
    /// [`Codec::default_preroll`] says.
    pub fn with_preroll(mut self, preroll: Duration) -> Self {
        self.demuxer.set_preroll(preroll);
        self
    }

    /// The track's codec and decoder configuration.
    pub fn codec(&self) -> &Codec {
        self.demuxer.codec()
    }

    /// Ticks per second of packet timestamps.
    pub fn timescale(&self) -> u32 {
        self.demuxer.timescale()
    }

    /// The MP4 track ID of the track read.
    pub fn track_id(&self) -> u32 {
        self.demuxer.track_id()
    }

    /// Media decoded before a seek target: the one [`Self::with_preroll`] set,
    /// else the `roll` or `prol` sample group of the initialization segment,
    /// else [`Codec::default_preroll`]. A media segment's own roll group can
    /// change it for seeks landing there.
    pub fn preroll(&self) -> Duration {
        self.demuxer.preroll()
    }

    /// A handle reporting how much of the representation the cache holds; see
    /// [`SegmentStream::buffer`].
    pub fn buffer(&self) -> Option<BufferHandle> {
        self.segments.buffer()
    }

    /// Seeks to `time` on the representation's media timeline, and returns the
    /// packets to decode before the stream's next one.
    ///
    /// The stream's next packet is the first one in decode order that presents at or
    /// after `time`. The pre-roll runs from the keyframe decoding starts at, which may be
    /// in earlier segments; those are fetched without moving the underlying stream's
    /// retention window.
    ///
    /// Fails like [`SegmentStream::seek_time`], and with the errors of fetching
    /// and parsing the segments it reads.
    pub async fn seek_time(&mut self, time: Duration) -> Result<PacketSeeked, Error> {
        self.seek(time, Landing::Exact).await
    }

    /// Seeks to the last keyframe at or before `time` on the representation's media
    /// timeline, and returns the packets to decode before the stream's next one.
    ///
    /// The stream's next packet is that keyframe, or for a codec with pre-roll, the
    /// first packet past the pre-roll from it. The returned [`PacketSeeked::landed`] is at or
    /// before `time`, by up to the keyframe interval, or at the first packet for a
    /// `time` before it. The pre-roll holds only the
    /// codec's own, unless the keyframe's leading pictures would be shown, when decoding
    /// starts at the keyframe before.
    ///
    /// Fails like [`Self::seek_time`].
    pub async fn seek_keyframe(&mut self, time: Duration) -> Result<PacketSeeked, Error> {
        self.seek(time, Landing::Keyframe).await
    }

    /// Seeks to `time`, landing as `landing` says.
    async fn seek(&mut self, time: Duration, landing: Landing) -> Result<PacketSeeked, Error> {
        let range = self.demuxer.seek_range(time)?;
        let first = self.segments.first_number();

        // The segment containing `time`, where the stream then carries on.
        self.segments.seek_time(time).await?;
        let target = match poll_fn(|cx| self.segments.poll_next_part(cx)).await {
            Some(Ok(target)) => target,
            Some(Err(error)) => return Err(error),
            None => {
                return Err(Error::Initialization(
                    "MPEG stream ended before the requested media time".to_owned(),
                ));
            }
        };

        // Earlier segments, fetched without moving the stream.
        let mut loaded = Vec::new();
        let mut number = (*range.start()).max(first);
        while number < *range.end() {
            let earlier = self.segments.media_part_body(number).await?;
            loaded.push(self.load(earlier)?);
            number = number.next();
        }
        loaded.push(self.load(target)?);

        // Back further until decoding can start, and on until a packet presents at the time.
        let mut at_last = false;
        loop {
            let earliest = loaded[0].number();
            let at_first = earliest <= first;
            match self.demuxer.seek(time, landing, &loaded, at_first, at_last) {
                Split::Done(seeked) => {
                    self.failed = false;
                    return Ok(seeked);
                }
                Split::Earlier => {
                    let before = earliest.checked_prev().ok_or_else(|| {
                        Error::Initialization(format!(
                            "MPEG stream has no media segment before {earliest}"
                        ))
                    })?;
                    let earlier = self.segments.media_part_body(before).await?;
                    loaded.insert(0, self.load(earlier)?);
                }
                // The stream's next segment, where it then carries on.
                Split::Later => match poll_fn(|cx| self.segments.poll_next_part(cx)).await {
                    Some(Ok(later)) => loaded.push(self.load(later)?),
                    Some(Err(error)) => return Err(error),
                    None => at_last = true,
                },
            }
        }
    }

    /// Converts this stream to one of decoder-ready packets in its codec's elementary
    /// format, such as [`DecoderFormat::H264AnnexB`] for H.264. The same as
    /// [`Self::into_media_stream_for`] with [`DecoderStyle::Elementary`].
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if the codec has no decoder-ready
    /// form, and with [`Error::MalformedDecoderInput`] if its MP4 decoder configuration
    /// is damaged.
    pub fn into_media_stream(self) -> Result<MediaStream, Error> {
        self.into_media_stream_for(DecoderStyle::Elementary)
    }

    /// Converts this stream to one of decoder-ready packets in its codec's format of
    /// `style`, such as [`DecoderFormat::H264LengthPrefixed`] for H.264 in
    /// [`DecoderStyle::ConfigRecord`].
    ///
    /// Fails like [`Self::into_media_stream`].
    pub fn into_media_stream_for(self, style: DecoderStyle) -> Result<MediaStream, Error> {
        MediaStream::new(self, Choice::Style(style))
    }

    /// Converts this stream to one of decoder-ready packets in `format`.
    ///
    /// Fails like [`Self::into_media_stream`], and with
    /// [`Error::UnsupportedDecoderInput`] if the codec cannot be converted to `format`.
    pub fn into_media_stream_as(self, format: DecoderFormat) -> Result<MediaStream, Error> {
        MediaStream::new(self, Choice::Format(format))
    }

    /// The packets of a media segment `part`.
    fn load(&mut self, part: PartBody) -> Result<Loaded, Error> {
        let number = part.number.expect("a media segment");
        self.demuxer.load(number, &part.body, &part.location)
    }
}

impl Stream for PacketStream {
    type Item = Result<Packet, Error>;

    /// The next packet in decode order.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(None);
        }
        loop {
            if let Some(packet) = this.demuxer.pop() {
                return Poll::Ready(Some(Ok(packet)));
            }
            let loaded = match ready!(this.segments.poll_next_part(cx)) {
                Some(Ok(part)) => this.load(part),
                Some(Err(error)) => Err(error),
                None => return Poll::Ready(None),
            };
            match loaded {
                Ok(loaded) => this.demuxer.push(loaded),
                Err(error) => {
                    this.failed = true;
                    return Poll::Ready(Some(Err(error)));
                }
            }
        }
    }
}
