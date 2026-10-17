//! H.264 in Annex-B or length-prefixed form, from the avcC and length-prefixed samples of
//! MP4.

use super::nal::{self, Framing};
use super::{
    DecoderFormat, DecoderInitialization, DecoderPacket, DecoderParameters, config_error,
    unsupported_format, wrong_codec,
};
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::mp4::packet::Packet;
use crate::mp4::reader::{Fields, ParseError, malformed, unsupported};

/// The formats an H.264 track converts to.
const FORMATS: [DecoderFormat; 2] = [DecoderFormat::H264AnnexB, DecoderFormat::H264LengthPrefixed];

/// A converter of an H.264 track's packets to [`DecoderFormat::H264AnnexB`] or
/// [`DecoderFormat::H264LengthPrefixed`], for use without a
/// [`DecoderPacketStream`](crate::DecoderPacketStream).
///
/// # Examples
///
/// ```no_run
/// use playdash::{DashManifest, DecoderFormat, PacketStream};
/// use playdash::decoder::H264Adapter;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
/// let packets = PacketStream::new(manifest.stream("video")?)?;
/// let adapter = H264Adapter::new(packets.codec(), DecoderFormat::H264AnnexB)?;
/// let parameter_sets = &adapter.initialization().data;
/// # let _ = parameter_sets;
/// for packet in packets {
///     let packet = adapter.convert(packet?)?;
///     # let _ = packet;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct H264Adapter {
    /// Bytes of each NAL unit's length prefix in the track's samples.
    length_size: usize,
    framing: Framing,
    initialization: DecoderInitialization,
}

impl H264Adapter {
    /// Creates an adapter converting the packets of a track with `codec` to `format`:
    /// [`DecoderFormat::H264AnnexB`] or [`DecoderFormat::H264LengthPrefixed`].
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if `codec` is not H.264, `format` is
    /// another format, or the avcC has a version other than 1, and with
    /// [`Error::MalformedDecoderInput`] if the avcC is truncated or inconsistent.
    pub fn new(codec: &Codec, format: DecoderFormat) -> Result<Self, Error> {
        let Codec::H264(video) = codec else {
            return Err(wrong_codec(codec, format));
        };
        let framing = match format {
            DecoderFormat::H264AnnexB => Framing::AnnexB,
            DecoderFormat::H264LengthPrefixed => Framing::LengthPrefixed,
            _ => return Err(unsupported_format(codec, format, &FORMATS)),
        };
        let avcc = parse_avcc(&video.config).map_err(|error| config_error(codec, format, error))?;

        // Annex B carries the parameter sets framed like its packets, the other the record.
        let data = match framing {
            Framing::AnnexB => {
                let all = [&avcc.sps[..], &avcc.pps[..], &avcc.sps_extensions[..]].concat();
                nal::from_nal_units(&all)
            }
            Framing::LengthPrefixed => video.config.clone(),
        };
        let units = |nals: &[&[u8]]| nals.iter().map(|nal| video.config.slice_ref(nal)).collect();
        Ok(Self {
            length_size: avcc.length_size,
            framing,
            initialization: DecoderInitialization {
                format,
                data,
                parameters: DecoderParameters::Video {
                    width: video.width,
                    height: video.height,
                    vps: Vec::new(),
                    sps: units(&avcc.sps),
                    pps: units(&avcc.pps),
                },
            },
        })
    }

    /// The initialization for a decoder of this adapter's packets.
    pub fn initialization(&self) -> &DecoderInitialization {
        &self.initialization
    }

    /// Converts `packet`, one of the track's, to the adapter's format: its NAL units
    /// framed by start codes, or kept as they are.
    ///
    /// Fails with [`Error::MalformedDecoderInput`] if the packet's NAL unit lengths do not
    /// fit its data.
    pub fn convert(&self, packet: Packet) -> Result<DecoderPacket, Error> {
        let format = self.initialization.format;
        nal::convert(packet, self.length_size, self.framing, "H.264", format)
    }
}

/// What an AVCDecoderConfigurationRecord (ISO/IEC 14496-15 §5.3.3.1) says about a track's
/// samples.
struct AvcConfig<'a> {
    /// Bytes of each NAL unit's length prefix.
    length_size: usize,
    sps: Vec<&'a [u8]>,
    pps: Vec<&'a [u8]>,
    sps_extensions: Vec<&'a [u8]>,
}

/// The configuration in `avcc`, an avcC payload.
fn parse_avcc(avcc: &[u8]) -> Result<AvcConfig<'_>, ParseError> {
    let mut fields = Fields::of_payload(*b"avcC", avcc);

    // The header.
    let version = fields.u8()?;
    if version != 1 {
        return Err(unsupported(format!("avcC has version {version}")));
    }
    let profile = fields.u8()?;
    fields.skip(2)?; // profile_compatibility, AVCLevelIndication
    let length_size = nal::length_size(fields.u8()?, "avcC")?;

    // The parameter sets.
    let (mut sps, mut pps, mut sps_extensions) = (Vec::new(), Vec::new(), Vec::new());
    let sps_count = fields.u8()? & 0x1f;
    read_sets(&mut fields, sps_count, "SPS", &mut sps)?;
    let pps_count = fields.u8()?;
    read_sets(&mut fields, pps_count, "PPS", &mut pps)?;

    // Profiles other than Baseline, Main and Extended may add the chroma format, bit
    // depths and SPS extensions.
    if !matches!(profile, 66 | 77 | 88) && fields.remaining() > 0 {
        fields.skip(3)?; // chroma_format, bit_depth_luma_minus8, bit_depth_chroma_minus8
        let ext_count = fields.u8()?;
        read_sets(&mut fields, ext_count, "SPS extension", &mut sps_extensions)?;
    }
    Ok(AvcConfig {
        length_size,
        sps,
        pps,
        sps_extensions,
    })
}

/// Reads `count` parameter sets of `kind`, each after a 16-bit length, onto `sets`.
fn read_sets<'a>(
    fields: &mut Fields<'a>,
    count: u8,
    kind: &str,
    sets: &mut Vec<&'a [u8]>,
) -> Result<(), ParseError> {
    for _ in 0..count {
        let len = fields.u16()?;
        if len == 0 {
            return Err(malformed(format!("avcC has an empty {kind}")));
        }
        sets.push(fields.bytes(usize::from(len))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::codec::{AudioConfig, VideoConfig};
    use bytes::Bytes;

    const SPS: &[u8] = &[0x67, 0x42, 0x00, 0x1e, 0x9a];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];

    /// An avcC payload of `profile` with `length_size`-byte NAL unit lengths, and with
    /// the high-profile fields if `extensions` is given.
    fn avcc(
        profile: u8,
        length_size: u8,
        sps: &[&[u8]],
        pps: &[&[u8]],
        extensions: Option<&[&[u8]]>,
    ) -> Vec<u8> {
        let mut out = vec![1, profile, 0, 30, 0xfc | (length_size - 1)];
        let sets = |out: &mut Vec<u8>, sets: &[&[u8]]| {
            for set in sets {
                out.extend((set.len() as u16).to_be_bytes());
                out.extend(*set);
            }
        };
        out.push(0xe0 | sps.len() as u8);
        sets(&mut out, sps);
        out.push(pps.len() as u8);
        sets(&mut out, pps);
        if let Some(extensions) = extensions {
            out.extend([0xfd, 0xf8, 0xf8, extensions.len() as u8]);
            sets(&mut out, extensions);
        }
        out
    }

    fn annex_b(codec: &Codec) -> Result<H264Adapter, Error> {
        H264Adapter::new(codec, DecoderFormat::H264AnnexB)
    }

    fn h264(config: Vec<u8>) -> Codec {
        Codec::H264(VideoConfig {
            width: 640,
            height: 360,
            config: Bytes::from(config),
        })
    }

    fn packet(data: Vec<u8>) -> Packet {
        Packet {
            data: Bytes::from(data),
            pts: 3_000,
            dts: 1_500,
            duration: 1_500,
            timescale: 90_000,
            keyframe: true,
            trim_start: 0,
            trim_end: 0,
        }
    }

    #[test]
    fn initialization_is_the_parameter_sets_with_start_codes() {
        let adapter = annex_b(&h264(avcc(66, 4, &[SPS], &[PPS], None))).unwrap();
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(SPS);
        expected.extend([0, 0, 0, 1]);
        expected.extend(PPS);
        assert_eq!(
            adapter.initialization(),
            &DecoderInitialization {
                format: DecoderFormat::H264AnnexB,
                data: Bytes::from(expected),
                parameters: DecoderParameters::Video {
                    width: 640,
                    height: 360,
                    vps: vec![],
                    sps: vec![Bytes::from_static(SPS)],
                    pps: vec![Bytes::from_static(PPS)],
                },
            }
        );

        // High profile: SPS extensions follow the PPS, but are not parameter sets.
        let extension: &[u8] = &[0x6d, 0x01];
        let config = avcc(100, 4, &[SPS, SPS], &[PPS], Some(&[extension]));
        let adapter = annex_b(&h264(config)).unwrap();
        let mut expected = Vec::new();
        for nal in [SPS, SPS, PPS, extension] {
            expected.extend([0, 0, 0, 1]);
            expected.extend(nal);
        }
        assert_eq!(adapter.initialization().data, expected);
        assert!(matches!(
            &adapter.initialization().parameters,
            DecoderParameters::Video { sps, pps, .. } if sps.len() == 2 && pps.len() == 1
        ));

        // avc3 may leave every parameter set to the packets.
        let adapter = annex_b(&h264(avcc(100, 4, &[], &[], None))).unwrap();
        assert!(adapter.initialization().data.is_empty());
        assert!(matches!(
            &adapter.initialization().parameters,
            DecoderParameters::Video { sps, pps, .. } if sps.is_empty() && pps.is_empty()
        ));
    }

    #[test]
    fn ffmpeg_high_444_avcc() {
        // From tests/files/packets/h264_init.mp4: High 4:4:4 Predictive, with the
        // high-profile fields and no SPS extensions.
        let config = [
            "01f4000affe1001767f4000a919b28c4d808800000030080000019078912cb01000668",
            "ebec448440fff8f800",
        ]
        .concat();
        let config: Vec<u8> = (0..config.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&config[i..i + 2], 16).unwrap())
            .collect();
        let adapter = annex_b(&h264(config.clone())).unwrap();
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(&config[8..8 + 0x17]);
        expected.extend([0, 0, 0, 1]);
        expected.extend(&config[8 + 0x17 + 3..8 + 0x17 + 3 + 6]);
        assert_eq!(adapter.initialization().data, expected);
    }

    #[test]
    fn packets_are_rewritten_at_every_length_size() {
        let idr: &[u8] = &[0x65, 0x88, 0x84, 0x00];
        let sei: &[u8] = &[0x06, 0x05, 0x01, 0xff];
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(sei);
        expected.extend([0, 0, 0, 1]);
        expected.extend(idr);
        for length_size in [1u8, 2, 4] {
            let config = avcc(66, length_size, &[SPS], &[PPS], None);
            let adapter = annex_b(&h264(config)).unwrap();
            let mut data = Vec::new();
            for nal in [sei, idr] {
                data.extend(&(nal.len() as u32).to_be_bytes()[4 - usize::from(length_size)..]);
                data.extend(nal);
            }
            let converted = adapter.convert(packet(data)).unwrap();
            assert_eq!(converted.data, expected, "length size {length_size}");
            assert_eq!(
                (converted.pts, converted.dts, converted.keyframe),
                (3_000, 1_500, true)
            );
        }
    }

    #[test]
    fn packets_not_following_the_configuration_are_malformed() {
        let adapter = annex_b(&h264(avcc(66, 4, &[SPS], &[PPS], None))).unwrap();
        // Two-byte lengths, where the configuration says four.
        let error = adapter.convert(packet(vec![0, 3, 0x65, 1, 2])).unwrap_err();
        assert!(
            matches!(
                &error,
                Error::MalformedDecoderInput { codec, format: DecoderFormat::H264AnnexB, reason }
                    if codec == "H.264" && reason == "NAL unit at byte 0 declares 222465 bytes but 1 remain"
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "Malformed H.264 decoder input in H.264 Annex B: NAL unit at byte 0 declares \
             222465 bytes but 1 remain"
        );
    }

    #[test]
    fn bad_configurations_are_refused() {
        let malformed = |config: Vec<u8>| match annex_b(&h264(config)) {
            Err(Error::MalformedDecoderInput { reason, .. }) => reason,
            other => panic!("{other:?}"),
        };

        // Truncated anywhere.
        let config = avcc(66, 4, &[SPS], &[PPS], None);
        for len in 0..config.len() {
            assert!(
                malformed(config[..len].to_vec()).starts_with("truncated avcC"),
                "{len}"
            );
        }
        // The high-profile fields are optional, but must be whole when present.
        let config = avcc(100, 4, &[SPS], &[PPS], Some(&[&[0x6d]]));
        let pps_end = config.len() - 4 - 3;
        assert!(annex_b(&h264(config[..pps_end].to_vec())).is_ok());
        for len in pps_end + 1..config.len() {
            assert!(
                malformed(config[..len].to_vec()).starts_with("truncated avcC"),
                "{len}"
            );
        }

        assert_eq!(
            malformed(avcc(66, 3, &[SPS], &[PPS], None)),
            "avcC gives 3-byte NAL unit lengths"
        );
        assert_eq!(
            malformed(avcc(66, 4, &[&[]], &[PPS], None)),
            "avcC has an empty SPS"
        );

        let mut version_2 = avcc(66, 4, &[SPS], &[PPS], None);
        version_2[0] = 2;
        assert!(matches!(
            annex_b(&h264(version_2)),
            Err(Error::UnsupportedDecoderInput { reason, .. }) if reason == "avcC has version 2"
        ));

        let mp3 = Codec::Mp3(AudioConfig {
            sample_rate: 44_100,
            channels: 2,
            config: Bytes::new(),
        });
        assert!(matches!(
            annex_b(&mp3),
            Err(Error::UnsupportedDecoderInput { codec, format: Some(DecoderFormat::H264AnnexB), .. })
                if codec == "MP3"
        ));
        let config = avcc(66, 4, &[SPS], &[PPS], None);
        assert!(matches!(
            H264Adapter::new(&h264(config), DecoderFormat::HevcAnnexB),
            Err(Error::UnsupportedDecoderInput { format: Some(DecoderFormat::HevcAnnexB), reason, .. })
                if reason == "H.264 converts only to H.264 Annex B or length-prefixed H.264"
        ));
    }

    #[test]
    fn length_prefixed_keeps_the_record_and_the_packets() {
        let config = avcc(66, 2, &[SPS], &[PPS], None);
        let adapter =
            H264Adapter::new(&h264(config.clone()), DecoderFormat::H264LengthPrefixed).unwrap();
        assert_eq!(
            adapter.initialization().format,
            DecoderFormat::H264LengthPrefixed
        );
        assert_eq!(adapter.initialization().data, config);

        let data = vec![0, 4, 0x65, 0x88, 0x84, 0x00, 0, 2, 0x06, 0x05];
        let converted = adapter.convert(packet(data.clone())).unwrap();
        assert_eq!(converted.data, data);
        assert_eq!((converted.pts, converted.dts), (3_000, 1_500));

        // Lengths are still checked.
        assert!(matches!(
            adapter.convert(packet(vec![0, 9, 0x65])),
            Err(Error::MalformedDecoderInput { format: DecoderFormat::H264LengthPrefixed, reason, .. })
                if reason == "NAL unit at byte 0 declares 9 bytes but 1 remain"
        ));
    }
}
