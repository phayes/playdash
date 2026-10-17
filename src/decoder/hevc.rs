//! HEVC in Annex-B or length-prefixed form, from the hvcC and length-prefixed samples of
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

/// The formats an HEVC track converts to.
const FORMATS: [DecoderFormat; 2] = [DecoderFormat::HevcAnnexB, DecoderFormat::HevcLengthPrefixed];

/// A converter of an HEVC track's packets to [`DecoderFormat::HevcAnnexB`] or
/// [`DecoderFormat::HevcLengthPrefixed`], for use without a
/// [`DecoderPacketStream`](crate::DecoderPacketStream).
#[derive(Clone, Debug)]
pub struct HevcAdapter {
    /// Bytes of each NAL unit's length prefix in the track's samples.
    length_size: usize,
    framing: Framing,
    initialization: DecoderInitialization,
}

impl HevcAdapter {
    /// Creates an adapter converting the packets of a track with `codec` to `format`:
    /// [`DecoderFormat::HevcAnnexB`] or [`DecoderFormat::HevcLengthPrefixed`].
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if `codec` is not HEVC, `format` is
    /// another format, or the hvcC has a version other than 1, and with
    /// [`Error::MalformedDecoderInput`] if the hvcC is truncated or inconsistent.
    pub fn new(codec: &Codec, format: DecoderFormat) -> Result<Self, Error> {
        let Codec::Hevc(video) = codec else {
            return Err(wrong_codec(codec, format));
        };
        let framing = match format {
            DecoderFormat::HevcAnnexB => Framing::AnnexB,
            DecoderFormat::HevcLengthPrefixed => Framing::LengthPrefixed,
            _ => return Err(unsupported_format(codec, format, &FORMATS)),
        };
        let hvcc = parse_hvcc(&video.config).map_err(|error| config_error(codec, format, error))?;

        // Annex B carries the parameter sets framed like its packets, the other the record.
        let data = match framing {
            Framing::AnnexB => {
                let all: Vec<&[u8]> = hvcc.nal_units.iter().map(|&(_, nal)| nal).collect();
                nal::from_nal_units(&all)
            }
            Framing::LengthPrefixed => video.config.clone(),
        };
        let units = |kind: u8| {
            hvcc.nal_units
                .iter()
                .filter(|&&(nal_type, _)| nal_type == kind)
                .map(|&(_, nal)| video.config.slice_ref(nal))
                .collect()
        };
        Ok(Self {
            length_size: hvcc.length_size,
            framing,
            initialization: DecoderInitialization {
                format,
                data,
                parameters: DecoderParameters::Video {
                    width: video.width,
                    height: video.height,
                    vps: units(NAL_VPS),
                    sps: units(NAL_SPS),
                    pps: units(NAL_PPS),
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
        nal::convert(packet, self.length_size, self.framing, "HEVC", format)
    }
}

/// What an HEVCDecoderConfigurationRecord (ISO/IEC 14496-15 §8.3.3.1) says about a
/// track's samples.
struct HevcConfig<'a> {
    /// Bytes of each NAL unit's length prefix.
    length_size: usize,
    /// The NAL units of every array, in order, each with its array's NAL unit type:
    /// usually VPS, SPS, PPS and SEI.
    nal_units: Vec<(u8, &'a [u8])>,
}

/// The NAL unit types of HEVC parameter sets.
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;

/// The configuration in `hvcc`, an hvcC payload.
fn parse_hvcc(hvcc: &[u8]) -> Result<HevcConfig<'_>, ParseError> {
    let mut fields = Fields::of_payload(*b"hvcC", hvcc);

    // The header: the profile, tier, level, format and frame rate fields, then the length
    // size in the last byte.
    let version = fields.u8()?;
    if version != 1 {
        return Err(unsupported(format!("hvcC has version {version}")));
    }
    fields.skip(20)?;
    let length_size = nal::length_size(fields.u8()?, "hvcC")?;

    // Arrays of NAL units, each of one type.
    let mut nal_units = Vec::new();
    let arrays = fields.u8()?;
    for _ in 0..arrays {
        let nal_type = fields.u8()? & 0x3f;
        let count = fields.u16()?;
        for _ in 0..count {
            let len = fields.u16()?;
            if len == 0 {
                return Err(malformed(format!(
                    "hvcC has an empty NAL unit of type {nal_type}"
                )));
            }
            nal_units.push((nal_type, fields.bytes(usize::from(len))?));
        }
    }
    Ok(HevcConfig {
        length_size,
        nal_units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::codec::VideoConfig;
    use bytes::Bytes;

    const VPS: &[u8] = &[0x40, 0x01, 0x0c];
    const SPS: &[u8] = &[0x42, 0x01, 0x01, 0x01];
    const PPS: &[u8] = &[0x44, 0x01, 0xc1];

    /// An hvcC payload with `length_size`-byte NAL unit lengths and `arrays` of NAL units,
    /// each with its type.
    fn hvcc(length_size: u8, arrays: &[(u8, &[&[u8]])]) -> Vec<u8> {
        let mut out = vec![
            1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5d, 0xf0, 0, 0xfc,
        ];
        out.extend([0xfd, 0xf8, 0xf8, 0, 0, 0x0c | (length_size - 1)]);
        out.push(arrays.len() as u8);
        for (nal_type, nals) in arrays {
            out.push(0x80 | nal_type);
            out.extend((nals.len() as u16).to_be_bytes());
            for nal in *nals {
                out.extend((nal.len() as u16).to_be_bytes());
                out.extend(*nal);
            }
        }
        out
    }

    fn annex_b(codec: &Codec) -> Result<HevcAdapter, Error> {
        HevcAdapter::new(codec, DecoderFormat::HevcAnnexB)
    }

    fn hevc(config: Vec<u8>) -> Codec {
        Codec::Hevc(VideoConfig {
            width: 1920,
            height: 1080,
            config: Bytes::from(config),
        })
    }

    fn packet(data: Vec<u8>) -> Packet {
        Packet {
            data: Bytes::from(data),
            pts: 0,
            dts: 0,
            duration: 1,
            timescale: 25,
            keyframe: true,
            trim_start: 0,
            trim_end: 0,
        }
    }

    #[test]
    fn initialization_is_every_array_with_start_codes() {
        let sei: &[u8] = &[0x4e, 0x01, 0x05];
        let config = hvcc(4, &[(32, &[VPS]), (33, &[SPS]), (34, &[PPS]), (39, &[sei])]);
        let adapter = annex_b(&hevc(config)).unwrap();
        let mut expected = Vec::new();
        for nal in [VPS, SPS, PPS, sei] {
            expected.extend([0, 0, 0, 1]);
            expected.extend(nal);
        }
        assert_eq!(
            adapter.initialization(),
            &DecoderInitialization {
                format: DecoderFormat::HevcAnnexB,
                data: Bytes::from(expected),
                parameters: DecoderParameters::Video {
                    width: 1920,
                    height: 1080,
                    vps: vec![Bytes::from_static(VPS)],
                    sps: vec![Bytes::from_static(SPS)],
                    pps: vec![Bytes::from_static(PPS)],
                },
            }
        );
    }

    #[test]
    fn packets_are_rewritten_at_every_length_size() {
        let slice: &[u8] = &[0x26, 0x01, 0xaf];
        let suffix_sei: &[u8] = &[0x50, 0x01, 0x81];
        let mut expected = Vec::new();
        for nal in [slice, suffix_sei] {
            expected.extend([0, 0, 0, 1]);
            expected.extend(nal);
        }
        for length_size in [1u8, 2, 4] {
            let config = hvcc(length_size, &[(32, &[VPS]), (33, &[SPS]), (34, &[PPS])]);
            let adapter = annex_b(&hevc(config)).unwrap();
            let mut data = Vec::new();
            for nal in [slice, suffix_sei] {
                data.extend(&(nal.len() as u32).to_be_bytes()[4 - usize::from(length_size)..]);
                data.extend(nal);
            }
            assert_eq!(
                adapter.convert(packet(data)).unwrap().data,
                expected,
                "length size {length_size}"
            );
        }

        let adapter = annex_b(&hevc(hvcc(4, &[]))).unwrap();
        assert!(matches!(
            adapter.convert(packet(vec![0, 0, 0, 9, 1])),
            Err(Error::MalformedDecoderInput { codec, format: DecoderFormat::HevcAnnexB, .. })
                if codec == "HEVC"
        ));
    }

    #[test]
    fn bad_configurations_are_refused() {
        let malformed = |config: Vec<u8>| match annex_b(&hevc(config)) {
            Err(Error::MalformedDecoderInput { reason, .. }) => reason,
            other => panic!("{other:?}"),
        };
        let config = hvcc(4, &[(32, &[VPS]), (33, &[SPS, SPS]), (34, &[PPS])]);
        for len in 0..config.len() {
            assert!(
                malformed(config[..len].to_vec()).starts_with("truncated hvcC"),
                "{len}"
            );
        }
        assert_eq!(
            malformed(hvcc(3, &[(32, &[VPS])])),
            "hvcC gives 3-byte NAL unit lengths"
        );
        assert_eq!(
            malformed(hvcc(4, &[(33, &[&[]])])),
            "hvcC has an empty NAL unit of type 33"
        );

        let mut version_0 = hvcc(4, &[]);
        version_0[0] = 0;
        assert!(matches!(
            annex_b(&hevc(version_0)),
            Err(Error::UnsupportedDecoderInput { reason, .. }) if reason == "hvcC has version 0"
        ));
        let h264 = Codec::H264(VideoConfig {
            width: 1,
            height: 1,
            config: Bytes::new(),
        });
        assert!(matches!(
            annex_b(&h264),
            Err(Error::UnsupportedDecoderInput { reason, .. })
                if reason == "H.264 cannot be HEVC Annex B"
        ));
        assert!(matches!(
            HevcAdapter::new(&hevc(hvcc(4, &[])), DecoderFormat::AacRaw),
            Err(Error::UnsupportedDecoderInput { reason, .. })
                if reason == "HEVC converts only to HEVC Annex B or length-prefixed HEVC"
        ));
    }

    #[test]
    fn length_prefixed_keeps_the_record_and_the_packets() {
        let config = hvcc(1, &[(32, &[VPS]), (33, &[SPS]), (34, &[PPS])]);
        let adapter =
            HevcAdapter::new(&hevc(config.clone()), DecoderFormat::HevcLengthPrefixed).unwrap();
        assert_eq!(adapter.initialization().data, config);

        let data = vec![3, 0x26, 0x01, 0xaf, 3, 0x50, 0x01, 0x81];
        assert_eq!(adapter.convert(packet(data.clone())).unwrap().data, data);
        assert!(matches!(
            adapter.convert(packet(vec![3, 0x26])),
            Err(Error::MalformedDecoderInput {
                format: DecoderFormat::HevcLengthPrefixed,
                ..
            })
        ));
    }
}
