//! Codecs: what a track's sample entry says its samples are, and how to configure a
//! decoder for them.

use super::init::{SampleEntry, Track};
use super::reader::{Atom, Fields, ParseError, fourcc, malformed, unsupported};
use bytes::Bytes;
use std::ops::Range;
use std::time::Duration;

/// A track's codec and decoder configuration, as its MP4 sample entry describes it.
///
/// # Examples
///
/// ```
/// use playdash::Codec;
///
/// fn describe(codec: &Codec) -> String {
///     match codec {
///         Codec::Aac(aac) => format!("AAC, {} Hz", aac.audio.sample_rate),
///         Codec::H264(video) => format!("H.264, {}x{}", video.width, video.height),
///         Codec::Unknown(unknown) => format!("{}", String::from_utf8_lossy(&unknown.fourcc)),
///         _ => "other".to_owned(),
///     }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Codec {
    /// AAC (MPEG-4 or MPEG-2 Audio), including HE-AAC. `config` is the
    /// AudioSpecificConfig.
    Aac(AacConfig),
    /// MPEG-1/2 Audio Layer III. `config` is empty.
    Mp3(AudioConfig),
    /// AC-3. `config` is the `dac3` payload, or empty when carried in `mp4a`.
    Ac3(AudioConfig),
    /// Enhanced AC-3. `config` is the `dec3` payload, or empty when carried in `mp4a`.
    Eac3(AudioConfig),
    /// FLAC. `config` is the `dfLa` payload: its version and flags, then the FLAC
    /// metadata blocks, starting with STREAMINFO.
    Flac(AudioConfig),
    /// Opus. `config` is the `dOps` payload, whose fields are big-endian, unlike an
    /// Ogg `OpusHead`.
    Opus(OpusConfig),
    /// Uncompressed PCM. `config` is empty.
    Pcm(PcmConfig),
    /// H.264 / AVC. `config` is the `avcC` payload (AVCDecoderConfigurationRecord).
    H264(VideoConfig),
    /// H.265 / HEVC. `config` is the `hvcC` payload (HEVCDecoderConfigurationRecord).
    Hevc(VideoConfig),
    /// AV1. `config` is the `av1C` payload (AV1CodecConfigurationRecord).
    Av1(VideoConfig),
    /// VP9. `config` is the `vpcC` payload: its version and flags, then the
    /// VPCodecConfigurationRecord.
    Vp9(VideoConfig),
    /// A sample entry type this crate does not interpret.
    Unknown(UnknownCodec),
}

/// Audio stream parameters and decoder configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    /// Output sample rate in Hz.
    pub sample_rate: u32,
    /// Output channel count.
    pub channels: u16,
    /// Decoder configuration bytes, as described on each [`Codec`] variant.
    pub config: Bytes,
}

/// AAC stream parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// Sample rate and channels after any SBR, and the AudioSpecificConfig.
    pub audio: AudioConfig,
    /// MPEG-4 Audio Object Type of the core codec, such as `2` for AAC-LC.
    pub object_type: u8,
    /// Whether the stream is HE-AAC: SBR, with or without parametric stereo, signalled
    /// explicitly in the AudioSpecificConfig. Streams that leave SBR for the decoder
    /// to discover read as plain AAC at the core's rate.
    pub sbr: bool,
}

/// Opus stream parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpusConfig {
    /// Always 48 kHz, Opus's decoding rate, and the `dOps` payload.
    pub audio: AudioConfig,
    /// Samples at 48 kHz the encoder added at the start, which a decoder discards.
    pub pre_skip: u16,
}

/// PCM stream parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcmConfig {
    /// Sample rate and channels, with empty `config`.
    pub audio: AudioConfig,
    /// Bits per sample.
    pub bits_per_sample: u16,
    /// Whether samples are IEEE floats rather than signed integers.
    pub float: bool,
    /// Whether samples are little-endian.
    pub little_endian: bool,
}

/// Video stream parameters and decoder configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoConfig {
    /// Width in pixels, from the sample entry.
    pub width: u16,
    /// Height in pixels, from the sample entry.
    pub height: u16,
    /// Decoder configuration bytes, as described on each [`Codec`] variant.
    pub config: Bytes,
}

/// A sample entry this crate does not interpret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownCodec {
    /// The sample entry type, such as `mp4v`.
    pub fourcc: [u8; 4],
    /// The whole sample entry box, header included.
    pub sample_entry: Bytes,
}

impl Codec {
    /// Media time a decoder needs to decode before a seek target for its output there to
    /// be correct: one frame for AAC-LC, AC-3 and E-AC-3, two for HE-AAC, three for MP3,
    /// 80 ms for Opus, and zero for FLAC, PCM, video and unknown codecs. Video instead
    /// starts decoding at a keyframe.
    pub fn default_preroll(&self) -> Duration {
        let frames = |count: u32, samples: u32, rate: u32| match rate {
            0 => Duration::ZERO,
            rate => Duration::from_secs(u64::from(count * samples)) / rate,
        };
        match self {
            Codec::Aac(aac) if aac.sbr => frames(2, 2048, aac.audio.sample_rate),
            Codec::Aac(aac) => frames(1, 1024, aac.audio.sample_rate),
            // MPEG-2 and 2.5 Layer III frames at low rates hold half as many samples.
            Codec::Mp3(audio) if audio.sample_rate < 32_000 => frames(3, 576, audio.sample_rate),
            Codec::Mp3(audio) => frames(3, 1152, audio.sample_rate),
            Codec::Ac3(audio) | Codec::Eac3(audio) => frames(1, 1536, audio.sample_rate),
            Codec::Opus(_) => Duration::from_millis(80),
            _ => Duration::ZERO,
        }
    }

    /// The codec's name for messages, such as `H.264`, or an unknown codec's sample
    /// entry type.
    pub(crate) fn name(&self) -> String {
        match self {
            Codec::Aac(_) => "AAC".to_owned(),
            Codec::Mp3(_) => "MP3".to_owned(),
            Codec::Ac3(_) => "AC-3".to_owned(),
            Codec::Eac3(_) => "E-AC-3".to_owned(),
            Codec::Flac(_) => "FLAC".to_owned(),
            Codec::Opus(_) => "Opus".to_owned(),
            Codec::Pcm(_) => "PCM".to_owned(),
            Codec::H264(_) => "H.264".to_owned(),
            Codec::Hevc(_) => "HEVC".to_owned(),
            Codec::Av1(_) => "AV1".to_owned(),
            Codec::Vp9(_) => "VP9".to_owned(),
            Codec::Unknown(unknown) => format!("unknown codec {}", fourcc(&unknown.fourcc)),
        }
    }
}

/// The codec of `entry`, a sample entry of `track` in `init`.
pub(crate) fn codec(init: &Bytes, track: &Track, entry: &SampleEntry) -> Result<Codec, ParseError> {
    let format = entry.format(init)?;
    let config = |kind: &[u8; 4]| -> Result<Option<Bytes>, ParseError> {
        Ok(entry
            .child(init, kind)?
            .map(|atom| init.slice(atom.payload..atom.end)))
    };
    let required = |kind: &[u8; 4]| -> Result<Bytes, ParseError> {
        config(kind)?.ok_or_else(|| {
            malformed(format!(
                "{} sample entry has no {} box",
                fourcc(&format),
                fourcc(kind)
            ))
        })
    };
    let unknown = || {
        Codec::Unknown(UnknownCodec {
            fourcc: format,
            sample_entry: init.slice(entry.atom.start..entry.atom.end),
        })
    };

    // Audio.
    if &track.handler == b"soun" {
        let fields = audio_fields(init, entry.atom)?;
        let audio = |config: Bytes| AudioConfig {
            sample_rate: fields.sample_rate,
            channels: fields.channels,
            config,
        };
        return Ok(match &format {
            b"mp4a" => {
                let Some(esds) = entry.child(init, b"esds")? else {
                    return Ok(unknown());
                };
                let (object_type, specific) = es_descriptor(init, esds)?;
                match object_type {
                    // MPEG-4 Audio, and MPEG-2 AAC Main, LC and SSR.
                    0x40 | 0x66..=0x68 => Codec::Aac(aac(audio(init.slice(specific)))?),
                    0x69 | 0x6b => Codec::Mp3(audio(Bytes::new())),
                    0xa5 => Codec::Ac3(audio(Bytes::new())),
                    0xa6 => Codec::Eac3(audio(Bytes::new())),
                    _ => unknown(),
                }
            }
            b".mp3" => Codec::Mp3(audio(Bytes::new())),
            b"ac-3" => Codec::Ac3(audio(config(b"dac3")?.unwrap_or_default())),
            b"ec-3" => Codec::Eac3(audio(config(b"dec3")?.unwrap_or_default())),
            b"fLaC" => Codec::Flac(flac(audio(required(b"dfLa")?))?),
            b"Opus" => Codec::Opus(opus(audio(required(b"dOps")?))?),
            b"ipcm" | b"fpcm" => {
                let pcmc = required(b"pcmC")?;
                let &[_, _, _, _, format_flags, sample_size, ..] = &pcmc[..] else {
                    return Err(malformed("truncated pcmC box"));
                };
                Codec::Pcm(PcmConfig {
                    audio: audio(Bytes::new()),
                    bits_per_sample: u16::from(sample_size),
                    float: &format == b"fpcm",
                    little_endian: format_flags & 0x01 != 0,
                })
            }
            b"sowt" | b"twos" => Codec::Pcm(PcmConfig {
                audio: audio(Bytes::new()),
                bits_per_sample: fields.sample_size,
                float: false,
                little_endian: &format == b"sowt",
            }),
            _ => unknown(),
        });
    }

    // Video.
    if &track.handler == b"vide" && entry.children.is_some() {
        let mut fields = entry.atom.fields(init);
        fields.skip(8 + 16)?; // SampleEntry header, pre_defined and reserved
        let (width, height) = (fields.u16()?, fields.u16()?);
        let video = |config: Bytes| VideoConfig {
            width,
            height,
            config,
        };
        return Ok(match &format {
            b"avc1" | b"avc3" => Codec::H264(video(required(b"avcC")?)),
            b"hvc1" | b"hev1" => Codec::Hevc(video(required(b"hvcC")?)),
            b"av01" => Codec::Av1(video(required(b"av1C")?)),
            b"vp09" => Codec::Vp9(video(required(b"vpcC")?)),
            _ => unknown(),
        });
    }
    Ok(unknown())
}

/// The fixed fields of an AudioSampleEntry that a codec may not repeat in its own
/// configuration.
struct AudioFields {
    channels: u16,
    sample_size: u16,
    sample_rate: u32,
}

fn audio_fields(buf: &[u8], entry: Atom) -> Result<AudioFields, ParseError> {
    let mut fields = entry.fields(buf);
    fields.skip(8 + 8)?; // SampleEntry header, version and reserved
    let channels = fields.u16()?;
    let sample_size = fields.u16()?;
    fields.skip(4)?; // pre_defined, reserved
    // 16.16 fixed point.
    let sample_rate = fields.u32()? >> 16;
    Ok(AudioFields {
        channels,
        sample_size,
        sample_rate,
    })
}

/// The object type indication and DecoderSpecificInfo range of an `esds` box's
/// ES_Descriptor (ISO/IEC 14496-1 §7.2.6.5). The range is empty without one.
fn es_descriptor(buf: &[u8], esds: Atom) -> Result<(u8, Range<usize>), ParseError> {
    let mut fields = esds.fields(buf);
    fields.version_flags()?;

    // ES_Descriptor
    descriptor(&mut fields, 0x03)?;
    fields.skip(2)?; // ES_ID
    let flags = fields.u8()?;
    if flags & 0x80 != 0 {
        fields.skip(2)?; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let len = fields.u8()?;
        fields.skip(usize::from(len))?; // URLstring
    }
    if flags & 0x20 != 0 {
        fields.skip(2)?; // OCR_ES_Id
    }

    // DecoderConfigDescriptor
    descriptor(&mut fields, 0x04)?;
    let object_type = fields.u8()?;
    fields.skip(12)?; // streamType, bufferSizeDB, maxBitrate, avgBitrate

    // DecoderSpecificInfo, when present.
    let start = fields.pos;
    let specific = match descriptor(&mut fields, 0x05) {
        Ok(len) => {
            let start = fields.pos;
            fields.skip(len)?;
            start..start + len
        }
        Err(_) => start..start,
    };
    Ok((object_type, specific))
}

/// Reads an MPEG-4 descriptor header with tag `expected`, and returns its length.
fn descriptor(fields: &mut Fields<'_>, expected: u8) -> Result<usize, ParseError> {
    let tag = fields.u8()?;
    if tag != expected {
        return Err(malformed(format!(
            "esds has descriptor tag {tag:#04x} where {expected:#04x} belongs"
        )));
    }
    // The length, in up to four 7-bit groups.
    let mut len = 0usize;
    for _ in 0..4 {
        let byte = fields.u8()?;
        len = len << 7 | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            break;
        }
    }
    Ok(len)
}

/// AAC parameters from `audio.config`, an AudioSpecificConfig (ISO/IEC 14496-3
/// §1.6.2.1). Without one, the sample entry's values stand.
fn aac(mut audio: AudioConfig) -> Result<AacConfig, ParseError> {
    let config = audio.config.clone();
    if config.is_empty() {
        return Ok(AacConfig {
            audio,
            object_type: 2,
            sbr: false,
        });
    }
    let mut bits = Bits::new(&config);
    let truncated = || malformed("truncated AudioSpecificConfig");

    let mut object_type = object_type(&mut bits).ok_or_else(truncated)?;
    let mut sample_rate = sampling_frequency(&mut bits).ok_or_else(truncated)?;
    let channels = bits.read(4).ok_or_else(truncated)? as u16;

    // Explicit SBR signalling: the output rate follows, then the core object type.
    let mut sbr = matches!(object_type, 5 | 29);
    if sbr {
        sample_rate = sampling_frequency(&mut bits).ok_or_else(truncated)?;
        object_type = self::object_type(&mut bits).ok_or_else(truncated)?;
    } else if let Some(output_rate) = sbr_extension(&mut bits, object_type, channels) {
        // Backward-compatible signalling, after the core's configuration.
        sbr = true;
        sample_rate = output_rate;
    }
    if sample_rate != 0 {
        audio.sample_rate = sample_rate;
    }
    // Channel configuration 0 defers to a program config element; keep the entry's count.
    if channels != 0 {
        audio.channels = match channels {
            7 => 8,
            11 => 7,
            12 | 14 => 8,
            channels => channels,
        };
    }
    Ok(AacConfig {
        audio,
        object_type,
        sbr,
    })
}

/// The output sample rate a backward-compatible SBR extension signals after the
/// GASpecificConfig of an `object_type` core with `channels`, if one does.
fn sbr_extension(bits: &mut Bits<'_>, object_type: u8, channels: u16) -> Option<u32> {
    // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder (and its delay), and
    // extensionFlag. A program config element (channels 0) is not read past.
    if !matches!(object_type, 1..=4 | 6 | 7 | 17 | 19..=23) || channels == 0 {
        return None;
    }
    bits.read(1)?;
    if bits.read(1)? == 1 {
        bits.read(14)?;
    }
    if bits.read(1)? == 1 && matches!(object_type, 6 | 20) {
        bits.read(3)?; // layerNr
    }

    // syncExtensionType 0x2b7, then the SBR object type and its present flag.
    if bits.read(11)? != 0x2b7 || bits.read(5)? != 5 || bits.read(1)? != 1 {
        return None;
    }
    sampling_frequency(bits)
}

/// An AudioSpecificConfig's next audio object type, with its escape for types past 30.
pub(crate) fn object_type(bits: &mut Bits<'_>) -> Option<u8> {
    match bits.read(5)? {
        31 => Some(32 + bits.read(6)? as u8),
        object_type => Some(object_type as u8),
    }
}

fn sampling_frequency(bits: &mut Bits<'_>) -> Option<u32> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    match bits.read(4)? {
        15 => bits.read(24),
        index => Some(RATES.get(index as usize).copied().unwrap_or(0)),
    }
}

/// FLAC parameters from `audio.config`, a `dfLa` payload.
fn flac(mut audio: AudioConfig) -> Result<AudioConfig, ParseError> {
    let setup = parse_dfla(&audio.config)?;
    audio.sample_rate = setup.sample_rate;
    audio.channels = setup.channels;
    Ok(audio)
}

/// What a `dfLa` box (FLAC in ISOBMFF §3.3.2) says about a FLAC stream, from its
/// STREAMINFO. Its metadata blocks follow its first four bytes, its version and flags.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FlacSetup {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) bits_per_sample: u16,
}

/// Bytes of a STREAMINFO block's body.
pub(crate) const STREAMINFO_LEN: usize = 34;

/// The FLAC setup in `dfla`, a `dfLa` payload whose metadata blocks must run from a
/// STREAMINFO to one flagged last, which ends the box.
pub(crate) fn parse_dfla(dfla: &[u8]) -> Result<FlacSetup, ParseError> {
    let mut fields = Fields::of_payload(*b"dfLa", dfla);
    let (version, _) = fields.version_flags()?;
    if version != 0 {
        return Err(unsupported(format!("dfLa has version {version}")));
    }

    // STREAMINFO: block and frame sizes, then the sample rate, channels and bit depth
    // packed from bit 80.
    let header = fields.u32()?;
    let (last, kind, len) = (header >> 31 == 1, header >> 24 & 0x7f, header & 0xff_ffff);
    if kind != 0 {
        return Err(malformed("dfLa does not start with STREAMINFO"));
    }
    if len as usize != STREAMINFO_LEN {
        return Err(malformed(format!("dfLa has a {len}-byte STREAMINFO")));
    }
    fields.skip(10)?;
    let packed = fields.u32()?;
    fields.skip(STREAMINFO_LEN - 14)?;

    // The other blocks, to the one flagged last.
    let mut last = last;
    while !last {
        let header = fields.u32()?;
        last = header >> 31 == 1;
        fields.skip((header & 0xff_ffff) as usize)?;
    }
    if fields.remaining() > 0 {
        return Err(malformed("dfLa has bytes after its last metadata block"));
    }
    Ok(FlacSetup {
        sample_rate: packed >> 12,
        channels: (packed >> 9 & 0x07) as u16 + 1,
        bits_per_sample: (packed >> 4 & 0x1f) as u16 + 1,
    })
}

/// Opus parameters from `audio.config`, a `dOps` payload.
fn opus(mut audio: AudioConfig) -> Result<OpusConfig, ParseError> {
    let setup = parse_dops(&audio.config)?;
    audio.sample_rate = 48_000;
    audio.channels = u16::from(setup.channels);
    Ok(OpusConfig {
        audio,
        pre_skip: setup.pre_skip,
    })
}

/// What a `dOps` box (Opus in ISOBMFF §4.3.2) says about an Opus stream, with mapping
/// family 0's implied stream counts and mapping filled in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OpusSetup {
    pub(crate) channels: u8,
    pub(crate) pre_skip: u16,
    pub(crate) input_sample_rate: u32,
    /// Q7.8 dB.
    pub(crate) output_gain: i16,
    pub(crate) mapping_family: u8,
    pub(crate) stream_count: u8,
    pub(crate) coupled_count: u8,
    /// One stream index per output channel.
    pub(crate) channel_mapping: Vec<u8>,
}

/// The Opus setup in `dops`, a `dOps` payload.
pub(crate) fn parse_dops(dops: &[u8]) -> Result<OpusSetup, ParseError> {
    let mut fields = Fields::of_payload(*b"dOps", dops);
    let version = fields.u8()?;
    if version != 0 {
        return Err(unsupported(format!("dOps has version {version}")));
    }
    let channels = fields.u8()?;
    let pre_skip = fields.u16()?;
    let input_sample_rate = fields.u32()?;
    let output_gain = fields.u16()? as i16;
    let mapping_family = fields.u8()?;

    // Family 0 is one mono or stereo stream; the others give their streams and mapping.
    let (stream_count, coupled_count, channel_mapping) = match mapping_family {
        0 => match channels {
            1 => (1, 0, vec![0]),
            2 => (1, 1, vec![0, 1]),
            _ => {
                return Err(malformed(format!(
                    "dOps has mapping family 0 with {channels} channels"
                )));
            }
        },
        _ => {
            let streams = fields.u8()?;
            let coupled = fields.u8()?;
            let mapping = fields.bytes(usize::from(channels))?.to_vec();
            (streams, coupled, mapping)
        }
    };
    if stream_count == 0 || coupled_count > stream_count {
        return Err(malformed(format!(
            "dOps has {stream_count} streams, {coupled_count} of them coupled"
        )));
    }
    Ok(OpusSetup {
        channels,
        pre_skip,
        input_sample_rate,
        output_gain,
        mapping_family,
        stream_count,
        coupled_count,
        channel_mapping,
    })
}

/// Big-endian bits read from bytes.
pub(crate) struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The next `count` bits, at most 32, or `None` past the end.
    pub(crate) fn read(&mut self, count: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            value = value << 1 | u32::from(bit);
            self.pos += 1;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::init;
    use crate::mp4::init::tests::{SOUND, TrackSpec, audio_entry, bx, full, init, video_entry};

    /// The codec of the only sample entry of a one-track init.
    fn parse(handler: &[u8; 4], entry: Vec<u8>) -> Result<Codec, ParseError> {
        let entries = [entry];
        let body = Bytes::from(init(&[TrackSpec {
            handler,
            entries: &entries,
            ..SOUND
        }]));
        let parsed = init::parse(&body).unwrap();
        let track = &parsed.tracks[0];
        codec(&body, track, &track.entries[0])
    }

    /// An `esds` with `object_type` and, if given, a DecoderSpecificInfo.
    fn esds(object_type: u8, specific: Option<&[u8]>) -> Vec<u8> {
        let mut decoder_config = vec![object_type, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        if let Some(specific) = specific {
            decoder_config.extend([0x05, specific.len() as u8]);
            decoder_config.extend(specific);
        }
        // Four-byte lengths, as many muxers write them.
        let mut es = vec![0, 1, 0, 0x04, 0x80, 0x80, 0x80, decoder_config.len() as u8];
        es.extend(decoder_config);
        let mut body = vec![0x03, 0x80, 0x80, 0x80, es.len() as u8];
        body.extend(es);
        full(b"esds", 0, 0, &[&body])
    }

    #[test]
    fn aac_lc_and_he_aac() {
        // AAC-LC, 44.1 kHz, stereo: object type 2, frequency index 4, channels 2.
        let lc = parse(
            b"soun",
            audio_entry(b"mp4a", 2, 44_100, &[&esds(0x40, Some(&[0x12, 0x10]))]),
        )
        .unwrap();
        let Codec::Aac(aac) = &lc else {
            panic!("{lc:?}")
        };
        assert_eq!((aac.object_type, aac.sbr), (2, false));
        assert_eq!((aac.audio.sample_rate, aac.audio.channels), (44_100, 2));
        assert_eq!(&aac.audio.config[..], [0x12, 0x10]);
        assert_eq!(lc.default_preroll(), Duration::from_secs(1024) / 44_100);

        // HE-AAC, explicit: object type 5, 24 kHz core, stereo, 48 kHz output, core LC.
        let he = parse(
            b"soun",
            audio_entry(
                b"mp4a",
                2,
                24_000,
                &[&esds(0x40, Some(&[0x2b, 0x11, 0x88, 0x00]))],
            ),
        )
        .unwrap();
        let Codec::Aac(aac) = &he else {
            panic!("{he:?}")
        };
        assert_eq!((aac.object_type, aac.sbr), (2, true));
        assert_eq!((aac.audio.sample_rate, aac.audio.channels), (48_000, 2));
        assert_eq!(he.default_preroll(), Duration::from_secs(4096) / 48_000);

        // HE-AAC, backward compatible: an LC core at 24 kHz, mono, then the 0x2b7 sync
        // extension naming SBR at 48 kHz, as AudioToolbox writes it.
        let compatible = parse(
            b"soun",
            audio_entry(
                b"mp4a",
                1,
                24_000,
                &[&esds(0x40, Some(&[0x13, 0x08, 0x56, 0xe5, 0x98]))],
            ),
        )
        .unwrap();
        let Codec::Aac(aac) = &compatible else {
            panic!("{compatible:?}")
        };
        assert_eq!((aac.object_type, aac.sbr), (2, true));
        assert_eq!((aac.audio.sample_rate, aac.audio.channels), (48_000, 1));
    }

    #[test]
    fn audio_codecs_by_entry_type_and_object_type() {
        let mp3 = parse(
            b"soun",
            audio_entry(b"mp4a", 2, 44_100, &[&esds(0x6b, None)]),
        )
        .unwrap();
        assert!(
            matches!(&mp3, Codec::Mp3(audio) if audio.sample_rate == 44_100 && audio.config.is_empty())
        );
        assert_eq!(
            mp3.default_preroll(),
            Duration::from_secs(3 * 1152) / 44_100
        );

        let dac3 = bx(b"dac3", &[&[0x10, 0x3d, 0xc0]]);
        let ac3 = parse(b"soun", audio_entry(b"ac-3", 6, 48_000, &[&dac3])).unwrap();
        assert!(
            matches!(&ac3, Codec::Ac3(audio) if audio.channels == 6 && audio.config[..] == [0x10, 0x3d, 0xc0])
        );
        assert_eq!(ac3.default_preroll(), Duration::from_millis(32));

        let dec3 = bx(b"dec3", &[&[0; 5]]);
        assert!(matches!(
            parse(b"soun", audio_entry(b"ec-3", 6, 48_000, &[&dec3])).unwrap(),
            Codec::Eac3(_)
        ));

        // Opus: version 0, 2 channels, pre-skip 312, 44.1 kHz input.
        let dops = bx(b"dOps", &[&[0, 2, 0x01, 0x38, 0, 0, 0xac, 0x44, 0, 0, 0]]);
        let opus = parse(b"soun", audio_entry(b"Opus", 2, 48_000, &[&dops])).unwrap();
        assert!(
            matches!(&opus, Codec::Opus(opus) if opus.pre_skip == 312 && opus.audio.sample_rate == 48_000)
        );
        assert_eq!(opus.default_preroll(), Duration::from_millis(80));

        let pcmc = full(b"pcmC", 0, 0, &[&[0x01, 24]]);
        let pcm = parse(b"soun", audio_entry(b"ipcm", 2, 48_000, &[&pcmc])).unwrap();
        assert!(
            matches!(&pcm, Codec::Pcm(pcm) if pcm.bits_per_sample == 24 && pcm.little_endian && !pcm.float)
        );
        assert_eq!(pcm.default_preroll(), Duration::ZERO);
    }

    #[test]
    fn flac_reads_streaminfo() {
        // STREAMINFO: block sizes and frame sizes, then 22.05 kHz, 1 channel, 16 bits.
        let mut streaminfo = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
        streaminfo.extend([0x05, 0x62, 0x20, 0xf0]);
        streaminfo.resize(34, 0);
        let mut dfla = vec![0x80, 0, 0, 34];
        dfla.extend(&streaminfo);
        let flac = parse(
            b"soun",
            audio_entry(b"fLaC", 2, 0, &[&full(b"dfLa", 0, 0, &[&dfla])]),
        )
        .unwrap();
        assert!(
            matches!(&flac, Codec::Flac(audio) if audio.sample_rate == 22_050 && audio.channels == 1)
        );

        // A truncated STREAMINFO, and a first block of another type (1, PADDING).
        let reason = |dfla: &[u8]| match parse(
            b"soun",
            audio_entry(b"fLaC", 2, 0, &[&full(b"dfLa", 0, 0, &[dfla])]),
        ) {
            Err(ParseError::Malformed(reason)) => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(reason(&dfla[..dfla.len() - 1]), "truncated dfLa box");
        let mut padding = dfla.clone();
        padding[0] = 0x81;
        assert_eq!(reason(&padding), "dfLa does not start with STREAMINFO");
    }

    #[test]
    fn truncated_dops_is_malformed() {
        let dops = bx(b"dOps", &[&[0, 2, 0x01]]);
        assert!(matches!(
            parse(b"soun", audio_entry(b"Opus", 2, 48_000, &[&dops])),
            Err(ParseError::Malformed(reason)) if reason == "truncated dOps box"
        ));
    }

    #[test]
    fn video_codecs_carry_their_configuration() {
        for (kind, config, check) in [
            (
                b"avc1",
                b"avcC",
                (|codec: &Codec| matches!(codec, Codec::H264(_))) as fn(&Codec) -> bool,
            ),
            (b"hev1", b"hvcC", |codec| matches!(codec, Codec::Hevc(_))),
            (b"av01", b"av1C", |codec| matches!(codec, Codec::Av1(_))),
            (b"vp09", b"vpcC", |codec| matches!(codec, Codec::Vp9(_))),
        ] {
            let codec = parse(
                b"vide",
                video_entry(kind, 1280, 720, &[&bx(config, &[&[9, 8, 7]])]),
            )
            .unwrap();
            assert!(check(&codec), "{codec:?}");
            let (Codec::H264(video) | Codec::Hevc(video) | Codec::Av1(video) | Codec::Vp9(video)) =
                codec
            else {
                unreachable!()
            };
            assert_eq!((video.width, video.height), (1280, 720));
            assert_eq!(&video.config[..], [9, 8, 7]);
            assert_eq!(Codec::H264(video).default_preroll(), Duration::ZERO);
        }
        assert!(matches!(
            parse(b"vide", video_entry(b"avc1", 1, 1, &[])),
            Err(ParseError::Malformed(reason)) if reason == "avc1 sample entry has no avcC box"
        ));
    }

    #[test]
    fn protected_entries_report_the_original_codec_and_others_are_unknown() {
        let sinf = bx(b"sinf", &[&bx(b"frma", &[b"avc1"])]);
        let encv = video_entry(b"encv", 640, 360, &[&bx(b"avcC", &[&[1]]), &sinf]);
        assert!(matches!(parse(b"vide", encv).unwrap(), Codec::H264(_)));

        let mp4v = video_entry(b"mp4v", 640, 360, &[]);
        let len = mp4v.len();
        assert!(matches!(
            parse(b"vide", mp4v).unwrap(),
            Codec::Unknown(unknown) if unknown.fourcc == *b"mp4v" && unknown.sample_entry.len() == len
        ));
        assert!(matches!(
            parse(b"text", bx(b"wvtt", &[&[0; 8]])).unwrap(),
            Codec::Unknown(unknown) if unknown.fourcc == *b"wvtt"
        ));
    }
}
