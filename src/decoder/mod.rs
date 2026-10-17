//! Decoder-ready packets: a track's compressed access units rewritten into the form codec
//! decoders read, with the initialization a decoder needs before them.
//!
//! [`MediaStream`] is the usual way in. The per-codec adapters here do the same
//! conversion one packet at a time.

mod aac;
mod adapter;
mod av1;
mod flac;
mod h264;
mod hevc;
mod nal;
mod opus;
mod passthrough;
#[cfg(feature = "sync")]
mod stream;

pub use aac::AacAdapter;
pub(crate) use adapter::{Adapter, Choice};
pub use av1::Av1Adapter;
pub use h264::H264Adapter;
pub use hevc::HevcAdapter;
pub use passthrough::PassthroughAdapter;
#[cfg(feature = "sync")]
pub use stream::{MediaStream, MediaStreamBuilder};

use crate::error::Error;
use crate::manifest::ContentKind;
use crate::mp4::codec::Codec;
use crate::mp4::packet::{self, Packet, ticks_to_duration};
use crate::mp4::reader::ParseError;
use crate::mp4::track::Track;
use bytes::Bytes;
use std::fmt;
use std::time::Duration;

/// A byte representation of compressed packets that decoders read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecoderFormat {
    /// H.264 NAL units, each after a four-byte `00 00 00 01` start code. Initialization
    /// holds the SPS, PPS and any SPS extension units in the same framing.
    H264AnnexB,
    /// H.264 NAL units, each after a big-endian length, as MP4 stores them.
    /// Initialization is the AVCDecoderConfigurationRecord (the `avcC` payload), which
    /// gives the length's size and holds the parameter sets.
    H264LengthPrefixed,
    /// HEVC NAL units, each after a four-byte start code. Initialization holds the VPS,
    /// SPS, PPS and any SEI units in the same framing.
    HevcAnnexB,
    /// HEVC NAL units, each after a big-endian length, as MP4 stores them.
    /// Initialization is the HEVCDecoderConfigurationRecord (the `hvcC` payload), which
    /// gives the length's size and holds the parameter sets.
    HevcLengthPrefixed,
    /// Raw AAC access units, without ADTS headers. Initialization is the
    /// AudioSpecificConfig.
    AacRaw,
    /// AAC access units, each after a 7-byte ADTS header without a CRC. Initialization is
    /// empty. Only for streams whose core is AAC Main, LC, SSR or LTP, with a channel
    /// configuration from 1 to 7 and a sampling frequency from the standard table.
    AacAdts,
    /// MPEG-1/2 Audio Layer III frames. Initialization is empty.
    Mp3,
    /// AC-3 frames. Initialization is the `dac3` payload, or empty for a track without
    /// one.
    Ac3,
    /// Enhanced AC-3 frames. Initialization is the `dec3` payload, or empty for a track
    /// without one.
    Eac3,
    /// FLAC frames. Initialization is a FLAC stream header: the `fLaC` marker, then the
    /// metadata blocks, starting with STREAMINFO. Initialization followed by the frames is
    /// a FLAC stream.
    Flac,
    /// Raw Opus packets, without Ogg framing. Initialization is an Ogg Opus identification
    /// header (`OpusHead`, RFC 7845 §5.1).
    OpusPackets,
    /// AV1 temporal units in the low-overhead bitstream format: each starts with a
    /// temporal delimiter OBU, and every OBU has a size field. Initialization holds the
    /// configuration OBUs, such as the sequence header, in the same form, and may be
    /// empty. In a byte stream, the initialization goes after the first packet's temporal
    /// delimiter, not before it.
    Av1Obu,
    /// AV1 temporal units as MP4 stores them, without temporal delimiters, but with a size
    /// field on every OBU. Initialization is the AV1CodecConfigurationRecord (the
    /// `av1C` payload).
    Av1Samples,
    /// VP9 frames, superframes intact, without IVF framing. Initialization is the `vpcC`
    /// payload: its version and flags, then the VPCodecConfigurationRecord. Decoders that
    /// take VP9 frames alone, such as libvpx, can ignore it.
    Vp9Frames,
    /// Uncompressed PCM samples, laid out as [`DecoderParameters::Pcm`] describes.
    /// Initialization is empty.
    Pcm,
}

impl fmt::Display for DecoderFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DecoderFormat::H264AnnexB => "H.264 Annex B",
            DecoderFormat::H264LengthPrefixed => "length-prefixed H.264",
            DecoderFormat::HevcAnnexB => "HEVC Annex B",
            DecoderFormat::HevcLengthPrefixed => "length-prefixed HEVC",
            DecoderFormat::AacRaw => "raw AAC",
            DecoderFormat::AacAdts => "ADTS AAC",
            DecoderFormat::Mp3 => "MP3 frames",
            DecoderFormat::Ac3 => "AC-3 frames",
            DecoderFormat::Eac3 => "E-AC-3 frames",
            DecoderFormat::Flac => "FLAC frames",
            DecoderFormat::OpusPackets => "Opus packets",
            DecoderFormat::Av1Obu => "AV1 OBUs",
            DecoderFormat::Av1Samples => "AV1 samples",
            DecoderFormat::Vp9Frames => "VP9 frames",
            DecoderFormat::Pcm => "PCM",
        })
    }
}

/// The kind of decoder input to produce, for decoders of one kind whatever the codec.
///
/// Packets and initialization differ between the two only for H.264, HEVC and AV1;
/// every other codec has one format for both.
///
/// # Decoders
///
/// The style common decoders take. "Either" means the decoder reads both forms, or that
/// the codec has one format for both styles. Notes say what a caller must add. A note
/// that requires a [`DecoderFormat`] means asking for that format with
/// [`MediaStreamBuilder::format`], whichever style is set.
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
/// | [`ffmpeg-next`], [`ffmpeg-the-third`], [`rsmpeg`], FFmpeg [libavcodec] | PCM                                | Either       | Pick the `pcm_*` decoder from [`DecoderParameters::Pcm`], and set the sample rate and channels. |
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
/// | [`objc2-video-toolbox`], [VideoToolbox]                                | H.264, HEVC                        | ConfigRecord | Initialization as the `avcC` or `hvcC` sample description extension, or parameter sets from [`DecoderParameters::Video`], through [`objc2-core-media`]. |
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
/// | [`symphonia`]                                                          | FLAC                               | Either       | Skip the initialization: [`extra_data`] is `streaminfo` from [`DecoderParameters::Flac`]. |
/// | [`symphonia`]                                                          | PCM                                | Either       | Pick the `CODEC_ID_PCM_*` codec from [`DecoderParameters::Pcm`], and set the sample rate and channels. |
/// | [`fdk-aac`], [FDK AAC]                                                 | AAC                                | Either       | `Transport::Raw`, initialization through `config_raw`. |
/// | [FAAD2]                                                                | AAC                                | Either       | Initialization through `NeAACDecInit2`. |
/// | [`claxon`]                                                             | FLAC                               | Either       |  |
/// | [libFLAC]                                                              | FLAC                               | Either       |  |
/// | [`minimp3`], [`minimp3_fixed`], [`puremp3`]                            | MP3                                | Either       |  |
/// | [minimp3], [mpg123]                                                    | MP3                                | Either       |  |
/// | [`opus`], [libopus], [`ruopus`], [`opus-rs`], [`opus-decoder`]         | Opus                               | Either       | Skip the initialization: configure from [`DecoderParameters::Opus`] at 48 kHz, as a multistream decoder past stereo. |
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DecoderStyle {
    /// Elementary streams, with their configuration in the bitstream's own form:
    /// [`DecoderFormat::H264AnnexB`], [`DecoderFormat::HevcAnnexB`] and
    /// [`DecoderFormat::Av1Obu`]. For H.264, HEVC, AV1, FLAC, MP3 and PCM, initialization
    /// followed by the packets is a valid stream. For decoders that parse bitstreams, such
    /// as dav1d, openh264, libFLAC and ffmpeg's raw demuxers.
    #[default]
    Elementary,
    /// Packets as MP4 stores them, with the codec's configuration record:
    /// [`DecoderFormat::H264LengthPrefixed`], [`DecoderFormat::HevcLengthPrefixed`] and
    /// [`DecoderFormat::Av1Samples`]. For decoders configured the way MP4 is, such as
    /// VideoToolbox.
    ConfigRecord,
}

/// Stream parameters a decoder may need to be configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecoderParameters {
    /// Video dimensions, and for H.264 and HEVC the parameter sets.
    #[non_exhaustive]
    Video {
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
        /// HEVC video parameter sets, each a NAL unit without a start code or length.
        /// Empty for other codecs.
        vps: Vec<Bytes>,
        /// H.264 or HEVC sequence parameter sets, each a NAL unit without a start code or
        /// length. Empty for other codecs, and for streams that carry them in their packets.
        sps: Vec<Bytes>,
        /// H.264 or HEVC picture parameter sets, each a NAL unit without a start code or
        /// length. Empty for other codecs, and for streams that carry them in their packets.
        pps: Vec<Bytes>,
    },
    /// Audio output format.
    #[non_exhaustive]
    Audio {
        /// Output sample rate in Hz.
        sample_rate: u32,
        /// Output channel count.
        channels: u16,
    },
    /// Opus stream parameters, as a multistream decoder takes them. A stream with
    /// mapping family 0 still gives its one stream and its mapping.
    #[non_exhaustive]
    Opus {
        /// Output channel count.
        channels: u16,
        /// Samples at 48 kHz to discard from the start of the decoded output.
        pre_skip: u16,
        /// Gain to apply to the decoded output, in Q7.8 dB.
        output_gain: i16,
        /// The channel mapping family: 0 for mono or stereo, others as RFC 7845 §5.1.1
        /// lists.
        mapping_family: u8,
        /// Opus streams in each packet.
        stream_count: u8,
        /// Streams of `stream_count` that are stereo.
        coupled_count: u8,
        /// The decoded stream channel of each output channel, `channels` long; 255 is
        /// silence.
        channel_mapping: Vec<u8>,
    },
    /// FLAC stream parameters.
    #[non_exhaustive]
    Flac {
        /// Sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u16,
        /// Bits per decoded sample.
        bits_per_sample: u16,
        /// The 34-byte STREAMINFO block, without its metadata block header, for decoders
        /// that take it in place of the stream header, such as symphonia.
        streaminfo: Bytes,
    },
    /// PCM sample layout.
    #[non_exhaustive]
    Pcm {
        /// Sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u16,
        /// Bits per sample.
        bits_per_sample: u16,
        /// Whether samples are IEEE floats rather than signed integers.
        float: bool,
        /// Whether samples are little-endian.
        little_endian: bool,
    },
}

/// The initialization a decoder needs before a stream's first packet, and again after
/// every reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderInitialization {
    /// The format of `data` and of every packet.
    pub format: DecoderFormat,
    /// Codec initialization bytes in `format`, as described on each [`DecoderFormat`]
    /// variant. May be empty, such as for H.264 streams carrying their parameter sets in
    /// their packets.
    pub data: Bytes,
    /// Stream parameters.
    pub parameters: DecoderParameters,
}

/// One compressed access unit in a [`DecoderFormat`], with its timing.
///
/// The timing fields mean what they do on [`Packet`]: timestamps count `timescale` ticks,
/// packets come in decode order, and the first `trim_start` and last `trim_end` ticks of
/// the decoded output are dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacket {
    /// The access unit, in the stream's [`DecoderFormat`].
    pub data: Bytes,
    /// Presentation timestamp in `timescale` ticks.
    pub pts: i64,
    /// Decode timestamp in `timescale` ticks.
    pub dts: i64,
    /// Duration in `timescale` ticks, including trimmed ticks.
    pub duration: u64,
    /// Ticks per second of `pts`, `dts`, `duration` and the trims.
    pub timescale: u32,
    /// Whether decoding can start at this packet. Every audio packet is a keyframe.
    pub keyframe: bool,
    /// Ticks of decoded output to drop from the start.
    pub trim_start: u64,
    /// Ticks of decoded output to drop from the end.
    pub trim_end: u64,
}

impl MediaPacket {
    /// The presentation timestamp as a time, or `None` if it is negative.
    pub fn presentation_time(&self) -> Option<Duration> {
        ticks_to_duration(self.pts, self.timescale)
    }

    /// `packet`'s timing with `data` in place of its MP4 bytes. Every adapter builds its
    /// packets through this, so none drops a timing field.
    pub(crate) fn from_packet(packet: Packet, data: Bytes) -> Self {
        let Packet {
            data: _,
            pts,
            dts,
            duration,
            timescale,
            keyframe,
            trim_start,
            trim_end,
        } = packet;
        Self {
            data,
            pts,
            dts,
            duration,
            timescale,
            keyframe,
            trim_start,
            trim_end,
        }
    }
}

/// A [`MediaStream`] seek's landing point, and the packets to decode before
/// continuing.
///
/// Reset the decoder and configure it with the stream's initialization, feed it
/// `preroll` in order and discard its output, then decode the stream's packets as usual,
/// discarding any output presented before `discard_before`: drop output for which
/// [`Self::discards`] is true, and for audio, [`Self::trim_start_of`] ticks from the
/// start of each packet's output.
///
/// # Examples
///
/// ```
/// use playdash::{DecoderInitialization, MediaPacket, Seeked};
///
/// # struct Frame { pts: i64, duration: u64 }
/// # struct VideoDecoder;
/// # impl VideoDecoder {
/// #     fn reset(&mut self, _: &DecoderInitialization) {}
/// #     fn decode(&mut self, _: &MediaPacket) -> Vec<Frame> { Vec::new() }
/// # }
/// # struct AudioDecoder;
/// # impl AudioDecoder {
/// #     fn reset(&mut self, _: &DecoderInitialization) {}
/// #     fn decode(&mut self, _: &MediaPacket) -> Vec<f32> { Vec::new() }
/// # }
/// # fn show(_: Frame) {}
/// # fn play(_: &[f32]) {}
/// /// Decodes video from where `seeked` landed, given the stream's packets after the seek.
/// fn decode_video(
///     decoder: &mut VideoDecoder,
///     initialization: &DecoderInitialization,
///     seeked: &Seeked,
///     packets: impl IntoIterator<Item = MediaPacket>,
/// ) {
///     // Reset the decoder and configure it with the stream's initialization.
///     decoder.reset(initialization);
///
///     // Feed it the pre-roll in order, discarding its output.
///     for packet in &seeked.preroll {
///         decoder.decode(packet);
///     }
///
///     // Decode the stream's packets, discarding output presented before
///     // `discard_before`. Frames come out in presentation order, and B-frames can
///     // present before the landing point.
///     for packet in packets {
///         for frame in decoder.decode(&packet) {
///             if !seeked.discards(frame.pts, frame.duration) {
///                 show(frame);
///             }
///         }
///     }
/// }
///
/// /// Decodes mono audio at `sample_rate` from where `seeked` landed, given the stream's
/// /// packets after the seek.
/// fn decode_audio(
///     decoder: &mut AudioDecoder,
///     initialization: &DecoderInitialization,
///     seeked: &Seeked,
///     packets: impl IntoIterator<Item = MediaPacket>,
///     sample_rate: u64,
/// ) {
///     decoder.reset(initialization);
///     for packet in &seeked.preroll {
///         decoder.decode(packet);
///     }
///
///     // Each packet's output, without its encoder priming or what presents before
///     // `discard_before`.
///     for packet in packets {
///         let samples = decoder.decode(&packet);
///         let to_samples = |ticks: u64| {
///             let count = ticks * sample_rate / u64::from(packet.timescale);
///             usize::try_from(count).unwrap_or(usize::MAX).min(samples.len())
///         };
///         let start = to_samples(seeked.trim_start_of(&packet));
///         let end = samples.len() - to_samples(packet.trim_end);
///         play(&samples[start.min(end)..end]);
///     }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seeked {
    /// The time the seek asked for.
    pub requested: Duration,
    /// Presentation timestamp of the packet the stream yields next, in `timescale` ticks.
    pub landed: i64,
    /// Presentation timestamp before which decoded output is discarded, in `timescale`
    /// ticks: the requested time for `seek_time`, `landed` for `seek_keyframe`.
    pub discard_before: i64,
    /// Ticks per second of `landed` and `discard_before`: the stream's timescale.
    pub timescale: u32,
    /// Packets to decode, in order, before the stream's next packet. None of their output
    /// is presented.
    pub preroll: Vec<MediaPacket>,
}

impl Seeked {
    /// The landing point as a time, or `None` if it is negative.
    pub fn landed_time(&self) -> Option<Duration> {
        ticks_to_duration(self.landed, self.timescale)
    }

    /// The time before which decoded output is discarded, or `None` if it is negative.
    pub fn discard_before_time(&self) -> Option<Duration> {
        ticks_to_duration(self.discard_before, self.timescale)
    }

    /// Whether decoded output presented from `pts` for `duration` ticks is discarded
    /// whole: it starts before `discard_before` and ends at or before it. Output that
    /// spans `discard_before` is kept; for audio, trim it with [`Self::trim_start_of`].
    pub fn discards(&self, pts: i64, duration: u64) -> bool {
        packet::discards(self.discard_before, pts, duration)
    }

    /// Ticks to drop from the start of `packet`'s decoded output: its own `trim_start`,
    /// or more to discard what presents before `discard_before`, up to its whole
    /// `duration`. For audio, where output is decoded a packet at a time.
    pub fn trim_start_of(&self, packet: &MediaPacket) -> u64 {
        packet::trim_start_of(
            self.discard_before,
            packet.pts,
            packet.duration,
            packet.trim_start,
        )
    }
}

/// `error`, from parsing `codec`'s configuration for `format`, as a decoder-input error.
pub(crate) fn config_error(codec: &Codec, format: DecoderFormat, error: ParseError) -> Error {
    match error {
        ParseError::Malformed(reason) => Error::MalformedDecoderInput {
            codec: codec.name(),
            format,
            reason,
        },
        ParseError::Unsupported(reason) => Error::UnsupportedDecoderInput {
            codec: codec.name(),
            format: Some(format),
            reason,
        },
    }
}

/// The parameters of `width` by `height` video without parameter sets.
pub(crate) fn video(width: u16, height: u16) -> DecoderParameters {
    DecoderParameters::Video {
        width,
        height,
        vps: Vec::new(),
        sps: Vec::new(),
        pps: Vec::new(),
    }
}

/// The error for an adapter of `format` given `codec`, which it does not read.
pub(crate) fn wrong_codec(codec: &Codec, format: DecoderFormat) -> Error {
    Error::UnsupportedDecoderInput {
        codec: codec.name(),
        format: Some(format),
        reason: format!("{} cannot be {format}", codec.name()),
    }
}

/// The error for `codec` asked for as `format`, when it converts only to `supported`.
pub(crate) fn unsupported_format(
    codec: &Codec,
    format: DecoderFormat,
    supported: &[DecoderFormat],
) -> Error {
    let supported: Vec<String> = supported.iter().map(DecoderFormat::to_string).collect();
    Error::UnsupportedDecoderInput {
        codec: codec.name(),
        format: Some(format),
        reason: format!(
            "{} converts only to {}",
            codec.name(),
            supported.join(" or ")
        ),
    }
}

/// The track of a representation a media stream builder reads.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TrackChoice {
    /// The track with this MP4 track ID.
    Id(u32),
    /// The first track of this kind, in the initialization fragment's order.
    Kind(ContentKind),
}

/// The ID of the first of `tracks` of `kind`, or an [`Error::Initialization`] if none is.
pub(crate) fn track_of_kind(tracks: &[Track], kind: ContentKind) -> Result<u32, Error> {
    tracks
        .iter()
        .find(|track| track.kind == kind)
        .map(|track| track.id)
        .ok_or_else(|| Error::Initialization(format!("representation has no {kind:?} track")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_packet_keeps_every_timing_field() {
        let packet = Packet {
            data: Bytes::from_static(&[1, 2, 3]),
            pts: -7,
            dts: -9,
            duration: 11,
            timescale: 13,
            keyframe: true,
            trim_start: 5,
            trim_end: 3,
        };
        let converted = MediaPacket::from_packet(packet, Bytes::from_static(&[4]));
        assert_eq!(
            converted,
            MediaPacket {
                data: Bytes::from_static(&[4]),
                pts: -7,
                dts: -9,
                duration: 11,
                timescale: 13,
                keyframe: true,
                trim_start: 5,
                trim_end: 3,
            }
        );
    }
}
