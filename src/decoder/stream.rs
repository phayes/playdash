//! The blocking [`MediaStream`]: the main API for reading decoder-ready media.

use super::adapter::Choice;
use super::{
    Adapter, DecoderFormat, DecoderInitialization, DecoderStyle, MediaPacket, Seeked, TrackChoice,
    track_of_kind,
};
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use crate::manifest::{BehindWindow, ContentKind, DashManifest, LiveDelay};
use crate::mp4::codec::Codec;
use crate::packet::{PacketSeeked, PacketStream};
use crate::segment::SegmentCache;
use crate::segment::buffer::BufferHandle;
use crate::segment::schedule::Retention;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

/// Decoder-ready compressed audio or video from one DASH representation.
///
/// Create one with [`DashManifest::media_stream`]. Configure your decoder with
/// [`Self::initialization`], then iterate over [`MediaPacket`] values in decode order.
/// The stream downloads media as iteration advances and supports time-based seeking.
///
/// Most applications do not need the lower-level [`PacketStream`]. If you already have
/// one, [`PacketStream::into_media_stream`] and its variants can create this type. The
/// `async` feature provides the corresponding
/// [`nonblocking::MediaStream`](crate::nonblocking::MediaStream).
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use playdash::DashManifest;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
/// let mut input = manifest.media_stream("video").open()?;
/// let initialization = input.initialization().clone();
/// // configure the decoder with initialization.data and initialization.parameters
/// # let _ = initialization;
///
/// // After a seek, reset and reconfigure the decoder, then prime it with the pre-roll.
/// let seeked = input.seek_time(Duration::from_secs(30))?;
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
pub struct MediaStream {
    packets: PacketStream,
    adapter: Adapter,
    /// Set once iteration has yielded an error, so it ends rather than carrying on
    /// past it.
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
    /// if a pre-roll packet cannot be converted, after which iteration yields nothing
    /// until a seek succeeds.
    pub fn seek_time(&mut self, time: Duration) -> Result<Seeked, Error> {
        let seeked = self.packets.seek_time(time);
        self.converted(seeked)
    }

    /// Seeks to the last keyframe at or before `time`.
    ///
    /// Use this when the caller wants control over the landing point. For ordinary
    /// playback at an exact time, prefer [`Self::seek_time`].
    ///
    /// Fails like [`Self::seek_time`].
    pub fn seek_keyframe(&mut self, time: Duration) -> Result<Seeked, Error> {
        let seeked = self.packets.seek_keyframe(time);
        self.converted(seeked)
    }

    /// `seeked`, the result of a seek of the packets, with its pre-roll converted.
    fn converted(&mut self, seeked: Result<PacketSeeked, Error>) -> Result<Seeked, Error> {
        let seeked = self.adapter.seeked(seeked?);
        self.failed = seeked.is_err();
        seeked
    }
}

impl Iterator for MediaStream {
    type Item = Result<MediaPacket, Error>;

    /// The next packet in decode order. Blocks while its segment downloads.
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

/// Configures and opens a [`MediaStream`] for one DASH representation.
///
/// Get a builder from [`DashManifest::media_stream`]. Calling [`Self::open`] with no
/// other options selects the representation's only track, uses the codec's elementary
/// format, creates a private [`SegmentCache`], and keeps downloaded segments.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use playdash::{ContentKind, DashManifest, DecoderStyle, Retention};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
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
///     .open_at(Duration::from_secs(90))?;
/// // configure the decoder with input.initialization(), then decode seeked.preroll
/// # let _ = (input, seeked);
/// # Ok(())
/// # }
/// ```
///
/// # Choosing decoder input
///
/// Choose a [`DecoderStyle`] with [`MediaStreamBuilder::style`] before opening the
/// stream. "Either" means the decoder accepts both styles, or the codec has the same
/// format in both. The notes explain any extra setup. When a note names a
/// [`DecoderFormat`], select it with [`MediaStreamBuilder::format`].
///
/// | Decoder                                                                | Codec                              | DecoderStyle | Notes |
/// |------------------------------------------------------------------------|------------------------------------|--------------|-------|
/// | [`openh264`], [OpenH264]                                               | H.264                              | Elementary   |  |
/// | [`rusty_h264`]                                                         | H.264                              | Elementary   | Length-prefixed input is ignored without an error. |
/// | [`rff-codec-h264`]                                                     | H.264                              | Elementary   | Initialization as `extradata`. An `avcC` is dropped without an error. |
/// | [`rust_h264`], [`ps-rust_h264`], [`z264`]                              | H.264                              | Either       | ConfigRecord: initialization through `parse_avcc_config`. |
/// | [`veloster-h264`]                                                      | H.264                              | Either       | ConfigRecord: initialization through `Decoder::from_avcc`. |
/// | [`gpu-video`], formerly [`vk-video`] ([Vulkan Video])                  | H.264                              | Elementary   |  |
/// | [JM reference decoder]                                                 | H.264                              | Elementary   |  |
/// | [`rusty_h265`]                                                         | HEVC                               | Elementary   | Main, Main 10 and Main Still Picture only. |
/// | [`libde265-rs`], [`libde265`], [libde265]                              | HEVC                               | Elementary   |  |
/// | [HM reference decoder]                                                 | HEVC                               | Elementary   |  |
/// | [`dav1d`], [`rav1d`], [dav1d]                                          | AV1                                | Either       | ConfigRecord: skip the initialization. |
/// | [libaom], [libgav1]                                                    | AV1                                | Either       | ConfigRecord: skip the initialization. |
/// | [`vpx-rs`], [`env-libvpx-sys`], [libvpx]                               | VP9                                | Either       | Skip the initialization. |
/// | [`cros-codecs`] ([VA-API])                                             | H.264, HEVC                        | Elementary   |  |
/// | [`cros-codecs`] ([VA-API])                                             | AV1                                | Either       | ConfigRecord: skip the initialization. |
/// | [`cros-codecs`] ([VA-API])                                             | VP9                                | Either       | Skip the initialization. |
/// | [`cros-codecs`] ([V4L2])                                               | H.264, AV1, VP9                    | Elementary   | The V4L2 backend is incomplete. |
/// | [`ffmpeg-next`], [`ffmpeg-the-third`], [`rsmpeg`], FFmpeg [libavcodec] | H.264, HEVC, FLAC, Opus, VP9       | Either       | Initialization as `extradata`. |
/// | [`ffmpeg-next`], [`ffmpeg-the-third`], [`rsmpeg`], FFmpeg [libavcodec] | AV1                                | Either       | Initialization as `extradata`. The native `av1` decoder needs hardware acceleration. |
/// | [`ffmpeg-next`], [`ffmpeg-the-third`], [`rsmpeg`], FFmpeg [libavcodec] | AAC                                | Either       | Initialization as `extradata`. Without it, HE-AAC decodes wrongly. |
/// | [`ffmpeg-next`], [`ffmpeg-the-third`], [`rsmpeg`], FFmpeg [libavcodec] | MP3, AC-3, E-AC-3                  | Either       |  |
/// | [`ffmpeg-next`], [`ffmpeg-the-third`], [`rsmpeg`], FFmpeg [libavcodec] | PCM                                | Either       | Pick the `pcm_*` decoder from [`DecoderParameters::Pcm`](crate::DecoderParameters::Pcm), and set the sample rate and channels. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | H.264, HEVC                        | Elementary   | `-f h264` or `-f hevc`. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | AV1                                | Elementary   | `-f obu`. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | FLAC                               | Either       | `-f flac`. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | MP3, AC-3, E-AC-3                  | Either       | `-f mp3`, `-f ac3` or `-f eac3`. Skip the initialization. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | PCM                                | Either       | `-f s16le` and the like, with `-ar` and `-ac`. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | AAC                                | Either       | Requires [`DecoderFormat::AacAdts`], with `-f aac`. For streams ADTS can describe. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | Opus                               | Either       | Wrapped in Ogg pages, with `-f ogg`: the initialization and an `OpusTags` header first, then a page per packet. |
/// | [`ffmpeg-sidecar`], [FFmpeg raw demuxers]                              | VP9                                | Either       | Wrapped in IVF, with `-f ivf`: a file header, then a frame header before each frame. Skip the initialization. |
/// | [`gstreamer`], [GStreamer]                                             | H.264, HEVC                        | Either       | Elementary: `stream-format=byte-stream`, initialization in the stream, no `codec_data`. ConfigRecord: `stream-format=avc` or `hvc1`, initialization as `codec_data`. Both `alignment=au`. |
/// | [`gstreamer`], [GStreamer]                                             | AV1                                | Either       | Through `av1parse`, with `stream-format=obu-stream, alignment=tu`. ConfigRecord: initialization as `codec_data`. |
/// | [`gstreamer`], [GStreamer]                                             | AAC                                | Either       | `stream-format=raw`, initialization as `codec_data`. |
/// | [`gstreamer`], [GStreamer]                                             | FLAC                               | Either       | Through `flacparse`. |
/// | [`gstreamer`], [GStreamer]                                             | Opus                               | Either       | Initialization in the stream, through `opusparse`, or as `opusdec` caps fields. Not `codec_data`. |
/// | [`gstreamer`], [GStreamer]                                             | MP3, AC-3, E-AC-3, VP9, PCM        | Either       | Skip the initialization. |
/// | [`objc2-video-toolbox`], [VideoToolbox]                                | H.264, HEVC                        | ConfigRecord | Initialization as the `avcC` or `hvcC` sample description extension, or parameter sets from [`DecoderParameters::Video`](crate::DecoderParameters::Video), through [`objc2-core-media`]. |
/// | [`objc2-video-toolbox`], [VideoToolbox]                                | AV1                                | ConfigRecord | Initialization as the `av1C` sample description extension. Hardware only. |
/// | [`ndk`], [MediaCodec]                                                  | H.264, HEVC                        | Elementary   | Initialization as one `BUFFER_FLAG_CODEC_CONFIG` buffer for H.264, or `csd-0` for HEVC. |
/// | [`ndk`], [MediaCodec]                                                  | AV1                                | ConfigRecord | Initialization as `csd-0`. |
/// | [`web-sys`], [WebCodecs] `VideoDecoder`                                | H.264, HEVC                        | Either       | ConfigRecord: initialization as `description`. Elementary: no `description`. |
/// | [`web-sys`], [WebCodecs] `VideoDecoder`                                | AV1                                | Either       |  |
/// | [`windows`], [Media Foundation]                                        | H.264, HEVC                        | Elementary   |  |
/// | [NVIDIA NVDEC] (NVCUVID parser), [Intel oneVPL]                        | H.264, HEVC                        | Elementary   |  |
/// | [NVIDIA NVDEC] (NVCUVID parser), [Intel oneVPL]                        | AV1                                | Either       | ConfigRecord: skip the initialization. |
/// | [NVIDIA NVDEC] (NVCUVID parser), [Intel oneVPL]                        | VP9                                | Either       | Skip the initialization. |
/// | [AMD AMF]                                                              | H.264, HEVC                        | Either       | ConfigRecord: initialization as `AMF_VIDEO_DECODER_EXTRADATA`. |
/// | [AMD AMF]                                                              | AV1                                | Elementary   |  |
/// | [AMD AMF]                                                              | VP9                                | Either       | Skip the initialization. |
/// | [V4L2 stateful] (Raspberry Pi, Qualcomm, MediaTek)                     | H.264, HEVC, AV1                   | Elementary   |  |
/// | [V4L2 stateful] (Raspberry Pi, Qualcomm, MediaTek)                     | VP9                                | Either       | One frame per buffer: split superframes. |
/// | [VA-API], [VDPAU], [D3D11 Video], [D3D12 Video], [Vulkan Video], [V4L2 stateless] | H.264, HEVC             | Elementary   | Take parameter sets as parsed structures and slices as NAL units, so a parser goes in front. No length prefixes. |
/// | [VA-API], [VDPAU], [D3D11 Video], [D3D12 Video], [Vulkan Video], [V4L2 stateless] | AV1, VP9                | Either       | Take parsed headers and frame data, so a parser goes in front. VP9 superframes are split. |
/// | [`symphonia`]                                                          | AAC                                | Either       | Initialization as [`extra_data`]. AAC-LC, mono or stereo only. |
/// | [`symphonia`]                                                          | MP3                                | Either       |  |
/// | [`symphonia`]                                                          | FLAC                               | Either       | Skip the initialization: [`extra_data`] is `streaminfo` from [`DecoderParameters::Flac`](crate::DecoderParameters::Flac). |
/// | [`symphonia`]                                                          | PCM                                | Either       | Pick the `CODEC_ID_PCM_*` codec from [`DecoderParameters::Pcm`](crate::DecoderParameters::Pcm), and set the sample rate and channels. |
/// | [`fdk-aac`], [FDK AAC]                                                 | AAC                                | Either       | `Transport::Raw`, initialization through `config_raw`. |
/// | [FAAD2]                                                                | AAC                                | Either       | Initialization through `NeAACDecInit2`. |
/// | [`claxon`]                                                             | FLAC                               | Either       |  |
/// | [libFLAC]                                                              | FLAC                               | Either       |  |
/// | [`minimp3`], [`minimp3_fixed`], [`puremp3`]                            | MP3                                | Either       |  |
/// | [minimp3], [mpg123]                                                    | MP3                                | Either       |  |
/// | [`opus`], [libopus], [`ruopus`], [`opus-rs`], [`opus-decoder`]         | Opus                               | Either       | Skip the initialization: configure from [`DecoderParameters::Opus`](crate::DecoderParameters::Opus) at 48 kHz, as a multistream decoder past stereo. |
/// | [`audiopus`], [`magnum-opus`]                                          | Opus                               | Either       | As for `opus`, but mono or stereo only. |
/// | [liba52]                                                               | AC-3                               | Either       | Skip the initialization. |
/// | [AudioToolbox]                                                         | AAC                                | Either       | Initialization wrapped in an `esds` descriptor chain, as the magic cookie. |
/// | [AudioToolbox]                                                         | MP3, AC-3, E-AC-3                  | Either       | Skip the initialization. |
///
/// [`audiopus`]: https://crates.io/crates/audiopus
/// [`claxon`]: https://crates.io/crates/claxon
/// [`cros-codecs`]: https://crates.io/crates/cros-codecs
/// [`dav1d`]: https://crates.io/crates/dav1d
/// [`env-libvpx-sys`]: https://crates.io/crates/env-libvpx-sys
/// [`fdk-aac`]: https://crates.io/crates/fdk-aac
/// [`ffmpeg-next`]: https://crates.io/crates/ffmpeg-next
/// [`ffmpeg-sidecar`]: https://crates.io/crates/ffmpeg-sidecar
/// [`ffmpeg-the-third`]: https://crates.io/crates/ffmpeg-the-third
/// [`gpu-video`]: https://crates.io/crates/gpu-video
/// [`gstreamer`]: https://crates.io/crates/gstreamer
/// [`libde265-rs`]: https://crates.io/crates/libde265-rs
/// [`libde265`]: https://crates.io/crates/libde265
/// [`magnum-opus`]: https://crates.io/crates/magnum-opus
/// [`minimp3`]: https://crates.io/crates/minimp3
/// [`minimp3_fixed`]: https://crates.io/crates/minimp3_fixed
/// [`ndk`]: https://crates.io/crates/ndk
/// [`objc2-core-media`]: https://crates.io/crates/objc2-core-media
/// [`objc2-video-toolbox`]: https://crates.io/crates/objc2-video-toolbox
/// [`openh264`]: https://crates.io/crates/openh264
/// [`opus`]: https://crates.io/crates/opus
/// [`opus-decoder`]: https://crates.io/crates/opus-decoder
/// [`opus-rs`]: https://crates.io/crates/opus-rs
/// [`ps-rust_h264`]: https://crates.io/crates/ps-rust_h264
/// [`puremp3`]: https://crates.io/crates/puremp3
/// [`rav1d`]: https://crates.io/crates/rav1d
/// [`rff-codec-h264`]: https://crates.io/crates/rff-codec-h264
/// [`rsmpeg`]: https://crates.io/crates/rsmpeg
/// [`ruopus`]: https://crates.io/crates/ruopus
/// [`rust_h264`]: https://crates.io/crates/rust_h264
/// [`rusty_h264`]: https://crates.io/crates/rusty_h264
/// [`rusty_h265`]: https://crates.io/crates/rusty_h265
/// [`symphonia`]: https://crates.io/crates/symphonia
/// [`extra_data`]: https://docs.rs/symphonia-core/0.6/symphonia_core/codecs/audio/struct.AudioCodecParameters.html#structfield.extra_data
/// [`veloster-h264`]: https://crates.io/crates/veloster-h264
/// [`vk-video`]: https://crates.io/crates/vk-video
/// [`vpx-rs`]: https://crates.io/crates/vpx-rs
/// [`web-sys`]: https://crates.io/crates/web-sys
/// [`windows`]: https://crates.io/crates/windows
/// [`z264`]: https://crates.io/crates/z264
///
/// [AMD AMF]: https://github.com/GPUOpen-LibrariesAndSDKs/AMF
/// [AudioToolbox]: https://developer.apple.com/documentation/audiotoolbox
/// [D3D11 Video]: https://learn.microsoft.com/en-us/windows/win32/medfound/direct3d-11-video-apis
/// [D3D12 Video]: https://learn.microsoft.com/en-us/windows/win32/medfound/direct3d-12-video-overview
/// [dav1d]: https://code.videolan.org/videolan/dav1d
/// [FAAD2]: https://github.com/knik0/faad2
/// [FDK AAC]: https://github.com/mstorsjo/fdk-aac
/// [FFmpeg raw demuxers]: https://ffmpeg.org/ffmpeg-formats.html
/// [GStreamer]: https://gstreamer.freedesktop.org/
/// [HM reference decoder]: https://vcgit.hhi.fraunhofer.de/jvet/HM
/// [Intel oneVPL]: https://github.com/intel/libvpl
/// [JM reference decoder]: https://vcgit.hhi.fraunhofer.de/jvet/JM
/// [liba52]: https://sourceforge.net/projects/liba52/
/// [libaom]: https://aomedia.googlesource.com/aom/
/// [libavcodec]: https://ffmpeg.org/libavcodec.html
/// [libde265]: https://github.com/strukturag/libde265
/// [libFLAC]: https://github.com/xiph/flac
/// [libgav1]: https://chromium.googlesource.com/codecs/libgav1/
/// [libopus]: https://opus-codec.org/
/// [libvpx]: https://chromium.googlesource.com/webm/libvpx/
/// [Media Foundation]: https://learn.microsoft.com/en-us/windows/win32/medfound/microsoft-media-foundation-sdk
/// [MediaCodec]: https://developer.android.com/reference/android/media/MediaCodec
/// [minimp3]: https://github.com/lieff/minimp3
/// [mpg123]: https://www.mpg123.de/
/// [NVIDIA NVDEC]: https://developer.nvidia.com/video-codec-sdk
/// [OpenH264]: https://github.com/cisco/openh264
/// [V4L2]: https://docs.kernel.org/userspace-api/media/v4l/v4l2.html
/// [V4L2 stateful]: https://docs.kernel.org/userspace-api/media/v4l/dev-decoder.html
/// [V4L2 stateless]: https://docs.kernel.org/userspace-api/media/v4l/dev-stateless-decoder.html
/// [VA-API]: https://github.com/intel/libva
/// [VDPAU]: https://gitlab.freedesktop.org/vdpau/libvdpau
/// [VideoToolbox]: https://developer.apple.com/documentation/videotoolbox
/// [Vulkan Video]: https://docs.vulkan.org/spec/latest/chapters/videocoding.html
/// [WebCodecs]: https://developer.mozilla.org/en-US/docs/Web/API/VideoDecoder
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
    /// and a reader has already used it.
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
    /// [`SegmentStream::tracks`](crate::segment::SegmentStream::tracks). Most DASH
    /// representations contain one track and need no explicit selection. Calling this
    /// replaces a previous [`Self::track`] choice.
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
    /// [choosing decoder input](Self#choosing-decoder-input) for the compatibility table.
    /// Replaces any [`Self::format`].
    pub fn style(mut self, style: DecoderStyle) -> Self {
        self.choice = Choice::Style(style);
        self
    }

    /// Requests one exact decoder-input format instead of a codec-independent style.
    ///
    /// Use this when the decoder requires a specific format such as AAC with ADTS. See
    /// [choosing decoder input](Self#choosing-decoder-input) for the compatibility table.
    /// Replaces any [`Self::style`].
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
    pub fn open(mut self) -> Result<MediaStream, Error> {
        // The representation's segments.
        let cache = self.take_cache()?;
        let retention = self.retention.unwrap_or_default();
        let segments = self.manifest.segment_stream_retained(
            &self.representation_id,
            cache,
            retention,
            self.live_delay,
        )?
        .with_behind_window(self.behind_window);

        // The packets of the chosen track.
        let mut packets = match self.track {
            None => PacketStream::new(segments)?,
            Some(TrackChoice::Id(track_id)) => PacketStream::with_track(segments, track_id)?,
            Some(TrackChoice::Kind(kind)) => {
                let track_id = track_of_kind(&segments.tracks()?, kind)?;
                PacketStream::with_track(segments, track_id)?
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
    pub fn open_at(self, time: Duration) -> Result<(MediaStream, Seeked), Error> {
        let mut stream = self.open()?;
        let seeked = stream.seek_time(time)?;
        Ok((stream, seeked))
    }

    /// Opens the representation at the last keyframe at or before `time`.
    ///
    /// Prefer [`Self::open_at`] for normal playback at an exact requested time. This
    /// method is useful when the caller wants the earlier keyframe landing point.
    ///
    /// Fails like [`Self::open`] and [`MediaStream::seek_keyframe`].
    pub fn open_at_keyframe(self, time: Duration) -> Result<(MediaStream, Seeked), Error> {
        let mut stream = self.open()?;
        let seeked = stream.seek_keyframe(time)?;
        Ok((stream, seeked))
    }

    /// The cache to fetch through: the one given, or a new one, with any keys given added.
    fn take_cache(&mut self) -> Result<Arc<SegmentCache>, Error> {
        let cache = self.cache.take().unwrap_or_default();
        #[cfg(feature = "encryption")]
        if let Some(keys) = self.keys.take() {
            cache.add_keys(keys)?;
        }
        Ok(cache)
    }
}
