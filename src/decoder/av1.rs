//! AV1 as decoders read it: the low-overhead bitstream format, MP4's samples with their
//! temporal delimiters restored and every OBU sized; or the samples as MP4 stores them,
//! with every OBU sized.

use super::{
    DecoderFormat, DecoderInitialization, MediaPacket, config_error, unsupported_format, video,
    wrong_codec,
};
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::mp4::packet::Packet;
use crate::mp4::reader::{ParseError, malformed, unsupported};
use bytes::Bytes;

/// The formats an AV1 track converts to.
const FORMATS: [DecoderFormat; 2] = [DecoderFormat::Av1Obu, DecoderFormat::Av1Samples];

/// A temporal delimiter OBU: type 2, with a size field of zero.
const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

/// The OBU type of a temporal delimiter.
const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// A converter of an AV1 track's packets to [`DecoderFormat::Av1Obu`] or
/// [`DecoderFormat::Av1Samples`], for use without a
/// [`MediaStream`](crate::MediaStream).
#[derive(Clone, Debug)]
pub struct Av1Adapter {
    initialization: DecoderInitialization,
}

impl Av1Adapter {
    /// Creates an adapter converting the packets of a track with `codec` to `format`:
    /// [`DecoderFormat::Av1Obu`] or [`DecoderFormat::Av1Samples`].
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if `codec` is not AV1, `format` is
    /// another format, or the av1C has a version other than 1, and with
    /// [`Error::MalformedDecoderInput`] if the av1C is truncated or its configuration OBUs
    /// are inconsistent.
    pub fn new(codec: &Codec, format: DecoderFormat) -> Result<Self, Error> {
        let Codec::Av1(config) = codec else {
            return Err(wrong_codec(codec, format));
        };
        if !FORMATS.contains(&format) {
            return Err(unsupported_format(codec, format, &FORMATS));
        }
        let config_obus =
            parse_av1c(&config.config).map_err(|error| config_error(codec, format, error))?;
        let sized = sized_obus(config_obus).map_err(|reason| {
            config_error(
                codec,
                format,
                malformed(format!("av1C configOBUs: {reason}")),
            )
        })?;

        // The bitstream carries its configuration OBUs, the samples the whole record.
        let data = match format {
            DecoderFormat::Av1Obu => Bytes::from(sized),
            _ => config.config.clone(),
        };
        Ok(Self {
            initialization: DecoderInitialization {
                format,
                data,
                parameters: video(config.width, config.height),
            },
        })
    }

    /// The initialization for a decoder of this adapter's packets.
    pub fn initialization(&self) -> &DecoderInitialization {
        &self.initialization
    }

    /// Converts `packet`, one of the track's, to the adapter's format: a temporal unit
    /// that starts with a temporal delimiter and gives every OBU a size field, or the
    /// sample with every OBU given a size field.
    ///
    /// Fails with [`Error::MalformedDecoderInput`] if the packet's OBUs do not fit its
    /// data.
    pub fn convert(&self, packet: Packet) -> Result<MediaPacket, Error> {
        let format = self.initialization.format;
        let malformed = |reason| Error::MalformedDecoderInput {
            codec: "AV1".to_owned(),
            format,
            reason,
        };
        if format == DecoderFormat::Av1Samples {
            // Kept as stored unless an OBU, which can only be the last, lacks a size field.
            let mut all_sized = true;
            for obu in obus(&packet.data) {
                all_sized &= obu.map_err(malformed)?.sized;
            }
            let data = if all_sized {
                packet.data.clone()
            } else {
                Bytes::from(sized_obus(&packet.data).map_err(malformed)?)
            };
            return Ok(MediaPacket::from_packet(packet, data));
        }
        let sized = sized_obus(&packet.data).map_err(malformed)?;

        // Samples normally leave out the temporal delimiter; put one back where missing.
        let starts_with_delimiter = sized
            .first()
            .is_some_and(|&header| header >> 3 & 0x0f == OBU_TEMPORAL_DELIMITER);
        let mut data = Vec::with_capacity(TEMPORAL_DELIMITER.len() + sized.len());
        if !starts_with_delimiter {
            data.extend_from_slice(&TEMPORAL_DELIMITER);
        }
        data.extend_from_slice(&sized);
        Ok(MediaPacket::from_packet(packet, Bytes::from(data)))
    }
}

/// The configuration OBUs of `av1c`, an av1C payload (AV1 in ISOBMFF §2.3.3).
fn parse_av1c(av1c: &[u8]) -> Result<&[u8], ParseError> {
    let Some(&marker_version) = av1c.first() else {
        return Err(malformed("truncated av1C box"));
    };
    if marker_version & 0x80 == 0 {
        return Err(malformed("av1C has no marker bit"));
    }
    let version = marker_version & 0x7f;
    if version != 1 {
        return Err(unsupported(format!("av1C has version {version}")));
    }
    // Profile, level, tier, bit depth and chroma fields, and the presentation delay.
    av1c.get(4..).ok_or_else(|| malformed("truncated av1C box"))
}

/// One OBU of a sequence.
struct Obu<'a> {
    /// The OBU header, and its extension byte if it has one.
    headers: &'a [u8],
    /// The whole OBU: headers, size field if any, and payload.
    whole: &'a [u8],
    /// The payload.
    payload: &'a [u8],
    /// Whether the OBU has a size field.
    sized: bool,
}

/// The OBUs of `data`, where an OBU without a size field runs to the end. Yields an error,
/// with the reason, if an OBU's header or size runs past the end of `data` or a header's
/// forbidden bit is set, and stops after it.
fn obus(data: &[u8]) -> impl Iterator<Item = Result<Obu<'_>, String>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= data.len() {
            return None;
        }
        let start = pos;
        // Stop after an error, so it is seen once.
        pos = data.len();

        let header = data[start];
        if header & 0x80 != 0 {
            return Some(Err(format!(
                "OBU at byte {start} has its forbidden bit set"
            )));
        }
        let header_len = 1 + usize::from(header & 0x04 != 0);
        let sized = header & 0x02 != 0;
        let Some(headers) = data.get(start..start + header_len) else {
            return Some(Err(format!("OBU header at byte {start} is truncated")));
        };
        let after_headers = start + header_len;

        // An unsized OBU runs to the end; a sized one says where it ends.
        let (payload_start, end) = if sized {
            let Some((size, size_len)) = leb128(&data[after_headers..]) else {
                return Some(Err(format!("OBU size at byte {start} is truncated")));
            };
            let payload_start = after_headers + size_len;
            let end = usize::try_from(size)
                .ok()
                .and_then(|size| payload_start.checked_add(size))
                .filter(|&end| end <= data.len());
            let Some(end) = end else {
                return Some(Err(format!(
                    "OBU at byte {start} declares {size} bytes but {} remain",
                    data.len() - payload_start
                )));
            };
            (payload_start, end)
        } else {
            (after_headers, data.len())
        };
        pos = end;
        Some(Ok(Obu {
            headers,
            whole: &data[start..end],
            payload: &data[payload_start..end],
            sized,
        }))
    })
}

/// `data`, a sequence of OBUs where only the last may lack a size field, with a size
/// field on every OBU.
///
/// Fails, with the reason, if an OBU's header or size runs past the end of `data`, or a
/// header's forbidden bit is set.
fn sized_obus(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len() + 8);
    for obu in obus(data) {
        let obu = obu?;
        if obu.sized {
            out.extend_from_slice(obu.whole);
        } else {
            // The header with its size flag set, then the size the OBU lacked.
            out.push(obu.headers[0] | 0x02);
            out.extend_from_slice(&obu.headers[1..]);
            write_leb128(&mut out, obu.payload.len() as u64);
            out.extend_from_slice(obu.payload);
        }
    }
    Ok(out)
}

/// The leb128 value at the start of `bytes` and its length, or `None` if it is truncated
/// or longer than the eight bytes AV1 allows.
fn leb128(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in bytes.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// Appends `value` to `out` in leb128.
fn write_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::codec::VideoConfig;

    /// A sequence header OBU, sized.
    const SEQUENCE_HEADER: &[u8] = &[0x0a, 0x03, 0x00, 0x00, 0x00];
    /// A frame OBU, sized.
    const FRAME: &[u8] = &[0x32, 0x02, 0xaa, 0xbb];

    fn av1(config: Vec<u8>) -> Codec {
        Codec::Av1(VideoConfig {
            width: 96,
            height: 64,
            config: Bytes::from(config),
        })
    }

    fn av1c(config_obus: &[u8]) -> Vec<u8> {
        let mut out = vec![0x81, 0x00, 0x0c, 0x00];
        out.extend(config_obus);
        out
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

    fn obu_adapter(codec: &Codec) -> Result<Av1Adapter, Error> {
        Av1Adapter::new(codec, DecoderFormat::Av1Obu)
    }

    #[test]
    fn samples_keep_the_record_and_the_packets() {
        let config = av1c(SEQUENCE_HEADER);
        let adapter = Av1Adapter::new(&av1(config.clone()), DecoderFormat::Av1Samples).unwrap();
        assert_eq!(adapter.initialization().format, DecoderFormat::Av1Samples);
        assert_eq!(adapter.initialization().data, config);

        // Kept as stored when every OBU is sized.
        let mut data = FRAME.to_vec();
        data.extend(FRAME);
        assert_eq!(adapter.convert(packet(data.clone())).unwrap().data, data);

        // An unsized last OBU gains a size field, without a temporal delimiter.
        let mut data = FRAME.to_vec();
        data.extend([0x30, 0x55, 0x55]);
        let mut expected = FRAME.to_vec();
        expected.extend([0x32, 0x02, 0x55, 0x55]);
        assert_eq!(adapter.convert(packet(data)).unwrap().data, expected);

        // Still checked.
        assert!(matches!(
            adapter.convert(packet(vec![0x32, 0x05, 0xaa])),
            Err(Error::MalformedDecoderInput {
                format: DecoderFormat::Av1Samples,
                ..
            })
        ));

        assert!(matches!(
            Av1Adapter::new(&av1(av1c(&[])), DecoderFormat::Vp9Frames),
            Err(Error::UnsupportedDecoderInput { reason, .. })
                if reason == "AV1 converts only to AV1 OBUs or AV1 samples"
        ));
    }

    #[test]
    fn initialization_is_the_configuration_obus() {
        let adapter = obu_adapter(&av1(av1c(SEQUENCE_HEADER))).unwrap();
        assert_eq!(
            adapter.initialization(),
            &DecoderInitialization {
                format: DecoderFormat::Av1Obu,
                data: Bytes::from_static(SEQUENCE_HEADER),
                parameters: video(96, 64),
            }
        );
        let empty = obu_adapter(&av1(av1c(&[]))).unwrap();
        assert!(empty.initialization().data.is_empty());
    }

    #[test]
    fn packets_gain_a_temporal_delimiter() {
        let adapter = obu_adapter(&av1(av1c(&[]))).unwrap();
        let mut data = SEQUENCE_HEADER.to_vec();
        data.extend(FRAME);
        let mut expected = vec![0x12, 0x00];
        expected.extend(&data);
        assert_eq!(adapter.convert(packet(data)).unwrap().data, expected);

        // One already there is kept, not doubled.
        let mut delimited = vec![0x12, 0x00];
        delimited.extend(FRAME);
        assert_eq!(
            adapter.convert(packet(delimited.clone())).unwrap().data,
            delimited
        );
    }

    #[test]
    fn an_unsized_last_obu_gains_a_size_field() {
        let adapter = obu_adapter(&av1(av1c(&[]))).unwrap();
        // A frame OBU with an extension header and no size field, 200 bytes of payload.
        let mut data = FRAME.to_vec();
        data.extend([0x34, 0x28]);
        data.extend([0x55; 200]);
        let mut expected = vec![0x12, 0x00];
        expected.extend(FRAME);
        expected.extend([0x36, 0x28, 0xc8, 0x01]);
        expected.extend([0x55; 200]);
        assert_eq!(adapter.convert(packet(data)).unwrap().data, expected);
    }

    #[test]
    fn packets_with_inconsistent_obus_are_malformed() {
        let adapter = obu_adapter(&av1(av1c(&[]))).unwrap();
        let reason = |data: &[u8]| match adapter.convert(packet(data.to_vec())) {
            Err(Error::MalformedDecoderInput { codec, reason, .. }) if codec == "AV1" => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            reason(&[0x32, 0x05, 0xaa]),
            "OBU at byte 0 declares 5 bytes but 1 remain"
        );
        assert_eq!(reason(&[0x32, 0x80]), "OBU size at byte 0 is truncated");
        assert_eq!(reason(&[0x36]), "OBU header at byte 0 is truncated");
        assert_eq!(
            reason(&[0x32, 0x00, 0x92, 0x00]),
            "OBU at byte 2 has its forbidden bit set"
        );
    }

    #[test]
    fn bad_configurations_are_refused() {
        let reason = |config: Vec<u8>| match obu_adapter(&av1(config)) {
            Err(Error::MalformedDecoderInput { reason, .. }) => reason,
            Err(Error::UnsupportedDecoderInput { reason, .. }) => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(reason(vec![]), "truncated av1C box");
        assert_eq!(reason(vec![0x81, 0, 0]), "truncated av1C box");
        assert_eq!(reason(vec![0x01, 0, 0, 0]), "av1C has no marker bit");
        assert_eq!(reason(vec![0x82, 0, 0, 0]), "av1C has version 2");
        assert_eq!(
            reason(av1c(&[0x0a, 0x09, 0x00])),
            "av1C configOBUs: OBU at byte 0 declares 9 bytes but 1 remain"
        );
    }

    #[test]
    fn leb128_round_trips() {
        for value in [0, 1, 127, 128, 200, 16_383, 16_384, u64::from(u32::MAX)] {
            let mut bytes = Vec::new();
            write_leb128(&mut bytes, value);
            assert_eq!(leb128(&bytes), Some((value, bytes.len())), "{value}");
        }
        assert_eq!(leb128(&[0x80]), None);
        assert_eq!(leb128(&[0xff; 9]), None);
    }
}
