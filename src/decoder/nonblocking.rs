//! A representation's decoder-ready packets: the async `MediaStream` over a
//! `nonblocking::PacketStream`.

use super::adapter::Choice;
use super::{
    Adapter, DecoderFormat, DecoderInitialization, DecoderStyle, MediaPacket, MediaSeeked,
    TrackChoice, track_of_kind,
};
use crate::buffer::BufferHandle;
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use crate::manifest::ContentKind;
use crate::mp4::codec::Codec;
use crate::nonblocking::{DashManifest, FragmentCache, PacketStream};
use crate::schedule::{Retention, TimeAccuracy};
use futures_util::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

/// A track's compressed packets in the form its codec's decoders read: a [`Stream`] of
/// [`MediaPacket`]s in decode order, with the [`DecoderInitialization`] to configure a
/// decoder with first, and time seeking. The async counterpart of the blocking
/// `playdash::MediaStream`.
///
/// Made by [`DashManifest::media_stream`], or from a [`PacketStream`] by
/// [`PacketStream::into_media_stream`], which chooses the elementary format of the codec,
/// [`PacketStream::into_media_stream_for`] or [`PacketStream::into_media_stream_as`].
///
/// After an error, the stream yields it once and then ends, until a seek succeeds.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use futures_util::StreamExt;
/// use playdash::TimeAccuracy;
/// use playdash::nonblocking::DashManifest;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
/// let mut input = manifest.media_stream("video").open().await?;
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
    ) -> Result<MediaSeeked, Error> {
        let seeked = self.packets.seek_time(time, accuracy).await?;
        let seeked = self.adapter.seeked(seeked);
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
/// `playdash::MediaStreamBuilder`.
///
/// Made by [`DashManifest::media_stream`]. With no options set, [`Self::open`] reads the
/// representation's only track in its codec's elementary format, through a new
/// [`FragmentCache`] on the manifest's client, and keeps every fragment.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use playdash::{ContentKind, DecoderStyle, Retention, TimeAccuracy};
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
///     .open_at(Duration::from_secs(90), TimeAccuracy::Packet)
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
    cache: Option<Arc<FragmentCache>>,
    #[cfg(feature = "encryption")]
    keys: Option<ContentKeys>,
    retention: Option<Retention>,
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
            track: None,
            preroll: None,
            choice: Choice::Style(DecoderStyle::Elementary),
        }
    }

    /// Fetches through `cache` instead of a new one.
    pub fn cache(mut self, cache: Arc<FragmentCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Decrypts `cenc` or `cbcs` Common Encryption with `keys`, through a new
    /// [`FragmentCache`] on the manifest's client.
    ///
    /// [`Self::open`] fails with [`Error::Initialization`] if [`Self::cache`] is also
    /// set; give that cache the keys with [`FragmentCache::with_keys`] instead.
    #[cfg(feature = "encryption")]
    pub fn keys(mut self, keys: ContentKeys) -> Self {
        self.keys = Some(keys);
        self
    }

    /// Keeps and downloads only as much of the representation around the position as
    /// `retention` says. See
    /// [`FragmentStream::with_retention`](crate::nonblocking::FragmentStream::with_retention).
    pub fn retention(mut self, retention: Retention) -> Self {
        self.retention = Some(retention);
        self
    }

    /// Reads the representation's first track of `kind`, such as the audio of a muxed
    /// representation. Replaces any [`Self::track_id`].
    ///
    /// [`Self::open`] fails with [`Error::Initialization`] if the representation has no
    /// track of `kind`.
    pub fn track(mut self, kind: ContentKind) -> Self {
        self.track = Some(TrackChoice::Kind(kind));
        self
    }

    /// Reads track `track_id`, one of
    /// [`FragmentStream::tracks`](crate::nonblocking::FragmentStream::tracks). Replaces
    /// any [`Self::track`].
    ///
    /// [`Self::open`] fails with [`Error::Initialization`] if the representation has no
    /// such track.
    pub fn track_id(mut self, track_id: u32) -> Self {
        self.track = Some(TrackChoice::Id(track_id));
        self
    }

    /// Decodes `preroll` of media before a seek target. See
    /// [`PacketStream::with_preroll`].
    pub fn preroll(mut self, preroll: Duration) -> Self {
        self.preroll = Some(preroll);
        self
    }

    /// Converts packets to the format of `style` for the track's codec. See
    /// [`PacketStream::into_media_stream_for`]. Replaces any [`Self::format`].
    pub fn style(mut self, style: DecoderStyle) -> Self {
        self.choice = Choice::Style(style);
        self
    }

    /// Converts packets to `format`. Replaces any [`Self::style`].
    ///
    /// [`Self::open`] fails with [`Error::UnsupportedDecoderInput`] if the track's codec
    /// cannot be converted to `format`.
    pub fn format(mut self, format: DecoderFormat) -> Self {
        self.choice = Choice::Format(format);
        self
    }

    /// Opens the representation as a [`MediaStream`]. Fetches its initialization fragment
    /// before returning.
    ///
    /// Fails like [`DashManifest::stream`], [`PacketStream::new`] and
    /// [`PacketStream::into_media_stream`]. Fails with [`Error::Initialization`] if both
    /// a cache and content keys are set, or the representation has no track that
    /// [`Self::track`] or [`Self::track_id`] asks for.
    pub async fn open(mut self) -> Result<MediaStream, Error> {
        // The representation's fragments.
        let cache = self.take_cache()?;
        let mut fragments = self
            .manifest
            .stream_with_cache(&self.representation_id, cache)
            .await?;
        if let Some(retention) = self.retention {
            fragments = fragments.with_retention(retention);
        }

        // The packets of the chosen track.
        let mut packets = match self.track {
            None => PacketStream::new(fragments).await?,
            Some(TrackChoice::Id(track_id)) => {
                PacketStream::with_track(fragments, track_id).await?
            }
            Some(TrackChoice::Kind(kind)) => {
                let track_id = track_of_kind(&fragments.tracks()?, kind)?;
                PacketStream::with_track(fragments, track_id).await?
            }
        };
        if let Some(preroll) = self.preroll {
            packets = packets.with_preroll(preroll);
        }

        MediaStream::new(packets, self.choice)
    }

    /// Opens the representation as a [`MediaStream`] seeked to `time`, and returns the
    /// packets to decode before its next one, as [`MediaStream::seek_time`] does.
    ///
    /// Fails like [`Self::open`] and [`MediaStream::seek_time`].
    pub async fn open_at(
        self,
        time: Duration,
        accuracy: TimeAccuracy,
    ) -> Result<(MediaStream, MediaSeeked), Error> {
        let mut stream = self.open().await?;
        let seeked = stream.seek_time(time, accuracy).await?;
        Ok((stream, seeked))
    }

    /// The cache to fetch through: the one given, or a new one on the manifest's client
    /// that decrypts with any keys given.
    fn take_cache(&mut self) -> Result<Arc<FragmentCache>, Error> {
        #[cfg(feature = "encryption")]
        if let Some(keys) = self.keys.take() {
            if self.cache.is_some() {
                return Err(Error::Initialization(
                    "a media stream takes either a cache or content keys; give the cache \
                     the keys with FragmentCache::with_keys"
                        .to_owned(),
                ));
            }
            return Ok(Arc::new(self.manifest.new_cache().with_keys(keys)));
        }
        let manifest = self.manifest;
        Ok(self
            .cache
            .take()
            .unwrap_or_else(|| Arc::new(manifest.new_cache())))
    }
}
