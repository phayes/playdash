//! Compressed media in the form a codec decoder expects.
//!
//! Start with [`MediaStream`]. It provides decoder setup data and a sequence of
//! [`MediaPacket`] values. The codec-specific adapters are lower-level tools for
//! applications that already use the lower-level [`PacketStream`](crate::packet::PacketStream) API.

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

/// The byte layout used for a decoder's compressed input.
///
/// You normally do not need to choose this yourself. [`MediaStream`] selects a useful
/// default, and [`DecoderStyle`] selects between the two common families of formats.
/// Use [`MediaStreamBuilder::format`](crate::MediaStreamBuilder::format) only when your
/// decoder requires a specific variant.
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

/// Selects a decoder-friendly packet style without naming a codec-specific format.
///
/// Start with [`Elementary`](Self::Elementary) unless your decoder API explicitly asks
/// for MP4 configuration records such as `avcC`, `hvcC`, or `av1C`. The choice only
/// changes H.264, HEVC, and AV1; other codecs have one format in both styles.
///
/// See [choosing decoder input](crate::MediaStreamBuilder#choosing-decoder-input)
/// on [`MediaStreamBuilder`] for a compatibility guide.
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

/// Audio or video properties needed to configure a decoder.
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

/// Everything needed to configure a decoder before reading packets.
///
/// Read this from [`MediaStream::initialization`](crate::MediaStream::initialization).
/// Configure the decoder before the first packet and configure it again after resetting
/// the decoder for a seek.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderInitialization {
    /// Byte layout used by both [`Self::data`] and every [`MediaPacket::data`] value.
    /// Pass the matching format identifier to decoders that require one.
    pub format: DecoderFormat,
    /// Codec setup bytes to give the decoder before the first packet.
    ///
    /// Their layout is described by [`Self::format`]. An empty value is valid when the
    /// codec needs no separate setup or carries its configuration in media packets.
    pub data: Bytes,
    /// Parsed audio or video properties, such as dimensions, sample rate, or channels.
    /// These complement [`Self::data`] for decoder APIs that take structured settings.
    pub parameters: DecoderParameters,
}

/// One compressed audio or video unit, ready to send to a decoder.
///
/// A unit is usually one encoded audio or video frame. Packets arrive in decode order.
/// Timestamps and durations use `timescale` ticks; divide a value by `timescale` to get
/// seconds. Drop `trim_start` and `trim_end` ticks from the decoded output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacket {
    /// Compressed decoder input in [`DecoderInitialization::format`].
    /// The bytes share storage where possible and remain valid as long as this packet.
    pub data: Bytes,
    /// When the decoded output should be presented, in [`Self::timescale`] ticks.
    /// This can be negative for codec priming before the representation's time zero.
    pub pts: i64,
    /// When this packet should be decoded, in [`Self::timescale`] ticks.
    /// Packets are yielded in this order; presentation order may differ for B-frames.
    pub dts: i64,
    /// Full decoded duration in [`Self::timescale`] ticks, before applying the trims.
    pub duration: u64,
    /// Number of timestamp ticks per second for every timing field in this packet.
    pub timescale: u32,
    /// Whether a decoder can begin at this packet without an earlier video frame.
    /// Audio packets are always marked as keyframes.
    pub keyframe: bool,
    /// Ticks of decoded output to remove from the start, usually encoder priming.
    pub trim_start: u64,
    /// Ticks of decoded output to remove from the end, usually encoder padding.
    pub trim_end: u64,
}

impl MediaPacket {
    /// Converts [`Self::pts`] to a duration from the representation's time zero.
    ///
    /// Returns `None` for negative priming timestamps or a zero timescale.
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

/// Instructions for resuming decoding after a [`MediaStream`] seek.
///
/// After seeking:
///
/// 1. Reset the decoder and configure it again with
///    [`MediaStream::initialization`](crate::MediaStream::initialization).
/// 2. Decode every packet in [`Self::preroll`] and discard its output.
/// 3. Continue with the stream. Drop video frames for which [`Self::discards`] is true.
///    For audio, remove [`Self::trim_start_of`] ticks from the start of each packet's
///    decoded output.
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
    /// Original wall-clock position passed to `seek_time` or `seek_keyframe`.
    pub requested: Duration,
    /// Presentation timestamp of the stream's next packet, in [`Self::timescale`] ticks.
    /// It may be earlier than [`Self::requested`] when seeking to a keyframe.
    pub landed: i64,
    /// Presentation timestamp before which decoded output must not be shown.
    ///
    /// In [`Self::timescale`] ticks. This is the requested time for an exact seek and the
    /// landing timestamp for a keyframe seek.
    pub discard_before: i64,
    /// Number of ticks per second for [`Self::landed`] and [`Self::discard_before`].
    pub timescale: u32,
    /// Decoder-ready packets that must be decoded before the stream's next packet.
    /// Discard all output produced from this list.
    pub preroll: Vec<MediaPacket>,
}

impl Seeked {
    /// Converts [`Self::landed`] to a duration from the representation's time zero.
    /// Returns `None` for a negative timestamp or zero timescale.
    pub fn landed_time(&self) -> Option<Duration> {
        ticks_to_duration(self.landed, self.timescale)
    }

    /// Converts [`Self::discard_before`] to a duration from time zero.
    /// Returns `None` for a negative timestamp or zero timescale.
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
