//! Decoder-ready packets: a track's compressed access units rewritten into the form codec
//! decoders read, with the initialization a decoder needs before them.
//!
//! [`DecoderStream`] is the usual way in. The per-codec adapters here do the same
//! conversion one packet at a time.

mod aac;
mod adapter;
mod av1;
mod flac;
mod h264;
mod hevc;
mod nal;
#[cfg(feature = "async")]
pub(crate) mod nonblocking;
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
pub use stream::DecoderStream;

use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::mp4::packet::{Packet, ticks_to_duration};
use crate::mp4::reader::ParseError;
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
    /// AV1 temporal units as MP4 stores them, without temporal delimiters; the last OBU
    /// may lack a size field. Initialization is the AV1CodecConfigurationRecord (the
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecoderStyle {
    /// Elementary streams, with their configuration in the bitstream's own form:
    /// [`DecoderFormat::H264AnnexB`], [`DecoderFormat::HevcAnnexB`] and
    /// [`DecoderFormat::Av1Obu`]. Initialization followed by the packets is a valid
    /// stream. For decoders that parse bitstreams, such as dav1d, openh264, libFLAC and
    /// ffmpeg's raw demuxers.
    #[default]
    Elementary,
    /// Packets as MP4 stores them, with the codec's configuration record:
    /// [`DecoderFormat::H264LengthPrefixed`], [`DecoderFormat::HevcLengthPrefixed`] and
    /// [`DecoderFormat::Av1Samples`]. For decoders configured the way MP4 is, such as
    /// VideoToolbox, MediaCodec and WebCodecs.
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
pub struct DecoderPacket {
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

impl DecoderPacket {
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

/// A [`DecoderStream`] seek's landing point, and the packets to decode before
/// continuing.
///
/// Reset the decoder and configure it with the stream's initialization, feed it
/// `preroll` in order and discard its output, then decode the stream's packets as usual,
/// discarding any output presented before `discard_before`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderSeeked {
    /// The time the seek asked for.
    pub requested: Duration,
    /// Presentation timestamp of the packet the stream yields next, in its timescale.
    pub landed: i64,
    /// Presentation timestamp before which decoded output is discarded, in the stream's
    /// timescale.
    pub discard_before: i64,
    /// Packets to decode, in order, before the stream's next packet. None of their output
    /// is presented.
    pub preroll: Vec<DecoderPacket>,
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
        let converted = DecoderPacket::from_packet(packet, Bytes::from_static(&[4]));
        assert_eq!(
            converted,
            DecoderPacket {
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
