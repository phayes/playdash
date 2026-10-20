//! The async [`MediaStream`]: the main async API for decoder-ready media.

use super::DashManifest;
use super::packet::PacketStream;
use super::segment::SegmentCache;
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::decoder::{
    Adapter, Choice, DecoderFormat, DecoderInitialization, DecoderStyle, MediaPacket, Seeked,
    TrackChoice, track_of_kind,
};
use crate::error::Error;
use crate::manifest::{BehindWindow, ContentKind, LiveDelay};
use crate::mp4::codec::Codec;
use crate::packet::PacketSeeked;
use crate::segment::buffer::BufferHandle;
use crate::segment::schedule::Retention;
use futures_util::Stream;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

/// Decoder-ready compressed audio or video from one DASH representation.
///
/// Create one with [`DashManifest::media_stream`]. Configure your decoder with
/// [`Self::initialization`], then poll [`MediaPacket`] values in decode order. The stream
/// downloads media as it advances and supports time-based seeking.
///
/// This is the async counterpart of the blocking [`MediaStream`](crate::MediaStream). Most
/// applications do not need the lower-level [`PacketStream`].
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use futures_util::StreamExt;
/// use playdash::nonblocking::DashManifest;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
/// let mut input = manifest.media_stream("video").open().await?;
/// let initialization = input.initialization().clone();
/// // configure the decoder with initialization.data and initialization.parameters
/// # let _ = initialization;
///
/// let seeked = input.seek_time(Duration::from_secs(30)).await?;
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
pub struct MediaStream {
    packets: PacketStream,
    adapter: Adapter,
    /// Set once the stream has yielded an error, so it ends rather than carrying on past
    /// it.
    failed: bool,
}

impl MediaStream {
    /// The packets of `packets` in the format `choice` asks for.
    pub(crate) fn new(packets: PacketStream, choice: Choice) -> Result<Self, Error> {
        let adapter = Adapter::new(packets.codec(), choice)?;
        Ok(Self {
            packets,
            adapter,
            failed: false,
        })
    }

    /// Returns the format, setup bytes, and parameters needed to configure the decoder.
    ///
    /// Configure the decoder before reading the first packet. After seeking, reset the
    /// decoder and configure it again with this same value.
    pub fn initialization(&self) -> &DecoderInitialization {
        self.adapter.initialization()
    }

    /// Returns the selected track's codec and original MP4 configuration.
    ///
    /// Use [`Self::initialization`] to configure a decoder; it reflects the requested
    /// output format, while this value describes the source track.
    pub fn codec(&self) -> &Codec {
        self.packets.codec()
    }

    /// Returns the number of timestamp ticks per second.
    /// Divide packet timestamps and durations by this value to obtain seconds.
    pub fn timescale(&self) -> u32 {
        self.packets.timescale()
    }

    /// Returns the selected MP4 track's numeric ID.
    pub fn track_id(&self) -> u32 {
        self.packets.track_id()
    }

    /// Returns how much earlier media may be needed to decode after a seek.
    /// This is the value used to build [`Seeked::preroll`].
    pub fn preroll(&self) -> Duration {
        self.packets.preroll()
    }

    /// Returns a handle for observing download and buffer progress.
    ///
    /// Returns `None` only when this [`MediaStream`] was built from a lower-level packet
    /// stream that has no DASH media timeline.
    pub fn buffer(&self) -> Option<BufferHandle> {
        self.packets.buffer()
    }

    /// Returns `true` if this stream is playing a live presentation.
    ///
    /// At the live edge, reads wait for newly produced media instead of
    /// reporting end of stream.
    pub fn is_live(&self) -> bool {
        self.packets.is_live()
    }

    /// Returns the currently available range in presentation time.
    ///
    /// For live media this is the current time-shift window; for static media
    /// it is the full presentation. Returns `None` if timing is unavailable
    /// or a live stream has no available segment yet.
    pub fn available(&self) -> Option<Range<Duration>> {
        self.packets.available()
    }

    /// Returns the end time of the newest available live segment.
    ///
    /// Returns `None` for static streams or before any live segment is available.
    pub fn live_edge(&self) -> Option<Duration> {
        self.packets.live_edge()
    }

    /// Seeks to the packet playing at `time`.
    ///
    /// The returned [`Seeked`] value explains how to prime the decoder and which decoded
    /// output to discard. Reset and reconfigure the decoder after this call.
    ///
    /// Fails like [`PacketStream::seek_time`], and with [`Error::MalformedDecoderInput`]
    /// if a pre-roll packet cannot be converted, after which the stream yields nothing
    /// until a seek succeeds.
    pub async fn seek_time(&mut self, time: Duration) -> Result<Seeked, Error> {
        let seeked = self.packets.seek_time(time).await;
        self.converted(seeked)
    }

    /// Seeks to the last keyframe at or before `time`.
    ///
    /// Use this when the caller wants control over the landing point. For ordinary
    /// playback at an exact time, prefer [`Self::seek_time`].
    ///
    /// Fails like [`Self::seek_time`].
    pub async fn seek_keyframe(&mut self, time: Duration) -> Result<Seeked, Error> {
        let seeked = self.packets.seek_keyframe(time).await;
        self.converted(seeked)
    }

    /// `seeked`, the result of a seek of the packets, with its pre-roll converted.
    fn converted(&mut self, seeked: Result<PacketSeeked, Error>) -> Result<Seeked, Error> {
        let seeked = self.adapter.seeked(seeked?);
        self.failed = seeked.is_err();
        seeked
    }
}

impl Stream for MediaStream {
    type Item = Result<MediaPacket, Error>;

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

/// A builder of a [`MediaStream`]: one representation of a [`DashManifest`], and the
/// options to open it with. The async counterpart of the blocking
/// [`MediaStreamBuilder`](crate::MediaStreamBuilder).
///
/// Made by [`DashManifest::media_stream`]. With no options set, [`Self::open`] reads the
/// representation's only track in its codec's elementary format, through a new
/// [`SegmentCache`] on the manifest's client, and keeps every segment.
///
/// See [choosing decoder input](crate::MediaStreamBuilder#choosing-decoder-input)
/// on the blocking builder for the decoder compatibility table. It applies to this
/// builder too.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use playdash::{ContentKind, DecoderStyle, Retention};
/// use playdash::nonblocking::DashManifest;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
/// let video = manifest
///     .highest_bandwidth(ContentKind::Video)
///     .ok_or("no video")?;
/// let (input, seeked) = manifest
///     .media_stream(video.id)
///     .retention(Retention {
///         behind: Some(Duration::from_secs(30)),
///         ahead: Some(Duration::from_secs(60)),
///     })
///     .style(DecoderStyle::ConfigRecord)
///     .open_at(Duration::from_secs(90))
///     .await?;
/// // configure the decoder with input.initialization(), then decode seeked.preroll
/// # let _ = (input, seeked);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct MediaStreamBuilder<'a> {
    manifest: &'a DashManifest,
    representation_id: String,
    cache: Option<Arc<SegmentCache>>,
    #[cfg(feature = "encryption")]
    keys: Option<ContentKeys>,
    retention: Option<Retention>,
    live_delay: Option<LiveDelay>,
    behind_window: BehindWindow,
    track: Option<TrackChoice>,
    preroll: Option<Duration>,
    choice: Choice,
}

impl<'a> MediaStreamBuilder<'a> {
    /// A builder for representation `representation_id` of `manifest`, with no options
    /// set.
    pub(crate) fn new(manifest: &'a DashManifest, representation_id: &str) -> Self {
        Self {
            manifest,
            representation_id: representation_id.to_owned(),
            cache: None,
            #[cfg(feature = "encryption")]
            keys: None,
            retention: None,
            live_delay: None,
            behind_window: BehindWindow::default(),
            track: None,
            preroll: None,
            choice: Choice::Style(DecoderStyle::Elementary),
        }
    }

    /// Uses `cache` for segment downloads instead of creating a private cache.
    ///
    /// Share one cache between related streams to reuse downloaded segments and HTTP
    /// connections. A custom cache can also provide a custom transport or content keys.
    pub fn cache(mut self, cache: Arc<SegmentCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Supplies content keys for `cenc` or `cbcs` encrypted media.
    ///
    /// With [`Self::cache`], the keys are added to that cache. Otherwise `open` creates
    /// a private cache containing these keys. Unencrypted media is unaffected.
    ///
    /// [`Self::open`] fails with [`Error::Initialization`] if that cache has no keys yet
    /// and a stream has already used it.
    #[cfg(feature = "encryption")]
    pub fn keys(mut self, keys: ContentKeys) -> Self {
        self.keys = Some(keys);
        self
    }

    /// Limits downloaded and retained media around the current playback position.
    ///
    /// Without this option, the stream downloads to the end and retains every segment.
    /// Set both limits for long video or other streams that should use bounded memory.
    /// See [`Retention`] for the meaning of its forward and backward windows.
    pub fn retention(mut self, retention: Retention) -> Self {
        self.retention = Some(retention);
        self
    }

    /// Sets how far behind the live edge playback starts.
    ///
    /// This overrides `MPD@suggestedPresentationDelay`. Without either value,
    /// playback starts four segments behind the edge. See [`LiveDelay`]. This
    /// setting has no effect on static manifests.
    pub fn live_delay(mut self, delay: LiveDelay) -> Self {
        self.live_delay = Some(delay);
        self
    }

    /// Sets how playback recovers after falling behind a live presentation.
    ///
    /// The default, [`BehindWindow::Fail`], returns
    /// [`Error::BehindLiveWindow`]. This setting has no effect on static
    /// presentations.
    pub fn behind_window(mut self, policy: BehindWindow) -> Self {
        self.behind_window = policy;
        self
    }

    /// Selects the first MP4 track whose media type is `kind`.
    ///
    /// This is useful for a muxed representation containing both audio and video. Most
    /// DASH representations contain one track and need no explicit selection. Calling
    /// this replaces a previous [`Self::track_id`] choice.
    ///
    /// [`Self::open`] fails with [`Error::Initialization`] if the representation has no
    /// track of `kind`.
    pub fn track(mut self, kind: ContentKind) -> Self {
        self.track = Some(TrackChoice::Kind(kind));
        self
    }

    /// Selects a track by its numeric MP4 track ID.
    ///
    /// Discover IDs with
    /// [`SegmentStream::tracks`](crate::nonblocking::segment::SegmentStream::tracks).
    /// Most representations contain one track. Calling this replaces a previous
    /// [`Self::track`] choice.
    ///
    /// [`Self::open`] fails with [`Error::Initialization`] if the representation has no
    /// such track.
    pub fn track_id(mut self, track_id: u32) -> Self {
        self.track = Some(TrackChoice::Id(track_id));
        self
    }

    /// Overrides how much earlier media is decoded to prime the codec after a seek.
    ///
    /// The default comes from the MP4 roll metadata or the codec. Override it only when
    /// the decoder or media requires a known amount. The returned [`Seeked::preroll`]
    /// contains the resulting decoder-ready packets.
    pub fn preroll(mut self, preroll: Duration) -> Self {
        self.preroll = Some(preroll);
        self
    }

    /// Chooses the decoder-input style for whichever codec the selected track uses.
    ///
    /// The default is [`DecoderStyle::Elementary`]. See
    /// [choosing decoder input](crate::MediaStreamBuilder#choosing-decoder-input) for the
    /// compatibility table. Replaces any [`Self::format`].
    pub fn style(mut self, style: DecoderStyle) -> Self {
        self.choice = Choice::Style(style);
        self
    }

    /// Requests one exact decoder-input format instead of a codec-independent style.
    ///
    /// Use this when the decoder requires a specific format such as AAC with ADTS. See
    /// [choosing decoder input](crate::MediaStreamBuilder#choosing-decoder-input) for the
    /// compatibility table. Replaces any [`Self::style`].
    ///
    /// [`Self::open`] fails with [`Error::UnsupportedDecoderInput`] if the track's codec
    /// cannot be converted to `format`.
    pub fn format(mut self, format: DecoderFormat) -> Self {
        self.choice = Choice::Format(format);
        self
    }

    /// Opens the selected representation as a decoder-ready [`MediaStream`].
    ///
    /// This fetches and parses the initialization segment before returning, so the
    /// stream's codec and [`MediaStream::initialization`] are immediately available.
    /// Media segments download as the stream advances.
    ///
    /// Fails like [`DashManifest::segment_stream`], [`PacketStream::new`] and
    /// [`PacketStream::into_media_stream`]. Fails with [`Error::Initialization`] if
    /// content keys cannot be added to the cache, or the representation has no track
    /// that [`Self::track`] or [`Self::track_id`] asks for.
    pub async fn open(mut self) -> Result<MediaStream, Error> {
        // The representation's segments.
        let cache = self.take_cache()?;
        let retention = self.retention.unwrap_or_default();
        let segments = self
            .manifest
            .segment_stream_retained(&self.representation_id, cache, retention, self.live_delay)
            .await?
            .with_behind_window(self.behind_window);

        // The packets of the chosen track.
        let mut packets = match self.track {
            None => PacketStream::new(segments).await?,
            Some(TrackChoice::Id(track_id)) => PacketStream::with_track(segments, track_id).await?,
            Some(TrackChoice::Kind(kind)) => {
                let track_id = track_of_kind(&segments.tracks()?, kind)?;
                PacketStream::with_track(segments, track_id).await?
            }
        };
        if let Some(preroll) = self.preroll {
            packets = packets.with_preroll(preroll);
        }

        MediaStream::new(packets, self.choice)
    }

    /// Opens the representation and seeks directly to the media playing at `time`.
    ///
    /// The returned [`Seeked`] must be handled before consuming the stream: reset and
    /// configure the decoder, then decode its pre-roll as described on that type. For
    /// live media, an unavailable `time` is clamped to the current window;
    /// [`Seeked::landed`] reports the actual starting point.
    ///
    /// Fails like [`Self::open`] and [`MediaStream::seek_time`].
    pub async fn open_at(self, time: Duration) -> Result<(MediaStream, Seeked), Error> {
        let mut stream = self.open().await?;
        let seeked = stream.seek_time(time).await?;
        Ok((stream, seeked))
    }

    /// Opens the representation at the last keyframe at or before `time`.
    ///
    /// Prefer [`Self::open_at`] for normal playback at an exact requested time. This
    /// method is useful when the caller wants the earlier keyframe landing point.
    ///
    /// Fails like [`Self::open`] and [`MediaStream::seek_keyframe`].
    pub async fn open_at_keyframe(self, time: Duration) -> Result<(MediaStream, Seeked), Error> {
        let mut stream = self.open().await?;
        let seeked = stream.seek_keyframe(time).await?;
        Ok((stream, seeked))
    }

    /// The cache to fetch through: the one given, or a new one on the manifest's client,
    /// with any keys given added.
    fn take_cache(&mut self) -> Result<Arc<SegmentCache>, Error> {
        let manifest = self.manifest;
        let cache = self
            .cache
            .take()
            .unwrap_or_else(|| Arc::new(manifest.new_cache()));
        #[cfg(feature = "encryption")]
        if let Some(keys) = self.keys.take() {
            cache.add_keys(keys)?;
        }
        Ok(cache)
    }
}
