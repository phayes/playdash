//! NAL unit framing for H.264 and HEVC: MP4's length-prefixed NAL units checked, or
//! rewritten as Annex-B byte streams with start codes.

use super::{DecoderFormat, DecoderPacket};
use crate::error::Error;
use crate::mp4::packet::Packet;
use crate::mp4::reader::{ParseError, malformed};
use bytes::Bytes;

/// How an adapter frames the NAL units of its packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    /// Each after a start code.
    AnnexB,
    /// Each after its length, as MP4 stores them.
    LengthPrefixed,
}

/// The start code written before every NAL unit.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Appends `nal` to `out` after a start code.
fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// `nals`, each after a start code.
pub(crate) fn from_nal_units(nals: &[&[u8]]) -> Bytes {
    let mut out = Vec::new();
    for nal in nals {
        push_nal(&mut out, nal);
    }
    Bytes::from(out)
}

/// The NAL unit length size a `kind` configuration record's byte ends with, in its low
/// two bits as `lengthSizeMinusOne`: 1, 2 or 4 bytes.
pub(crate) fn length_size(byte: u8, kind: &str) -> Result<usize, ParseError> {
    match byte & 0x03 {
        2 => Err(malformed(format!("{kind} gives 3-byte NAL unit lengths"))),
        size => Ok(usize::from(size) + 1),
    }
}

/// `packet`, of `codec` with `length_size`-byte NAL unit lengths, in `framing`: with start
/// codes in place of the lengths, or as it is once its lengths are checked. Fails with a
/// malformed `format` error if the lengths do not fit its data.
pub(crate) fn convert(
    packet: Packet,
    length_size: usize,
    framing: Framing,
    codec: &str,
    format: DecoderFormat,
) -> Result<DecoderPacket, Error> {
    let malformed = |reason| Error::MalformedDecoderInput {
        codec: codec.to_owned(),
        format,
        reason,
    };
    let data = match framing {
        Framing::AnnexB => {
            Bytes::from(from_length_prefixed(&packet.data, length_size).map_err(malformed)?)
        }
        Framing::LengthPrefixed => {
            for nal in nal_units(&packet.data, length_size) {
                nal.map_err(malformed)?;
            }
            packet.data.clone()
        }
    };
    Ok(DecoderPacket::from_packet(packet, data))
}

/// `data`'s NAL units, each prefixed by a big-endian length of `length_size` bytes (1, 2
/// or 4), with start codes in place of the lengths. An empty `data` gives empty output.
///
/// Fails, with the reason, if a length is truncated, a NAL unit runs past the end of
/// `data`, or a NAL unit is empty.
fn from_length_prefixed(data: &[u8], length_size: usize) -> Result<Vec<u8>, String> {
    // Validate and count first, so the output is allocated once.
    let mut count = 0;
    for nal in nal_units(data, length_size) {
        nal?;
        count += 1;
    }

    // Then copy each NAL unit after a start code.
    let mut out = Vec::with_capacity(data.len() + count * (START_CODE.len() - length_size));
    for nal in nal_units(data, length_size) {
        push_nal(&mut out, nal.expect("validated"));
    }
    Ok(out)
}

/// The NAL units of `data`, each prefixed by a big-endian length of `length_size` bytes.
fn nal_units(data: &[u8], length_size: usize) -> impl Iterator<Item = Result<&[u8], String>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= data.len() {
            return None;
        }
        let start = pos;
        // Stop after an error, so it is seen once.
        pos = data.len();

        let Some(prefix) = data.get(start..start + length_size) else {
            return Some(Err(format!(
                "NAL unit length at byte {start} is truncated: {} of {length_size} bytes",
                data.len() - start
            )));
        };
        let len = prefix
            .iter()
            .fold(0usize, |len, &byte| len << 8 | usize::from(byte));
        let body = start + length_size;
        if len == 0 {
            return Some(Err(format!("NAL unit at byte {start} is empty")));
        }
        let Some(nal) = data.get(body..body + len) else {
            return Some(Err(format!(
                "NAL unit at byte {start} declares {len} bytes but {} remain",
                data.len() - body
            )));
        };
        pos = body + len;
        Some(Ok(nal))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `nals` with big-endian lengths of `length_size` bytes.
    fn length_prefixed(nals: &[&[u8]], length_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend(&(nal.len() as u32).to_be_bytes()[4 - length_size..]);
            out.extend(*nal);
        }
        out
    }

    #[test]
    fn rewrites_every_nal_unit_at_every_length_size() {
        let nals: [&[u8]; 3] = [&[0x65, 1, 2, 3], &[0x06, 0, 0, 3, 1], &[0x41; 300]];
        let mut expected = vec![0, 0, 0, 1, 0x65, 1, 2, 3, 0, 0, 0, 1, 0x06, 0, 0, 3, 1];
        expected.extend([0, 0, 0, 1]);
        expected.extend([0x41; 300]);

        // One-byte lengths cannot carry the 300-byte unit.
        let short = length_prefixed(&nals[..2], 1);
        assert_eq!(from_length_prefixed(&short, 1).unwrap(), expected[..17]);
        for length_size in [2, 4] {
            let data = length_prefixed(&nals, length_size);
            let out = from_length_prefixed(&data, length_size).unwrap();
            assert_eq!(out, expected, "length size {length_size}");
        }
        assert!(from_length_prefixed(&[], 4).unwrap().is_empty());
    }

    #[test]
    fn inconsistent_lengths_are_errors() {
        assert_eq!(
            from_length_prefixed(&[0, 0, 0, 2, 9, 9, 0, 0], 4),
            Err("NAL unit length at byte 6 is truncated: 2 of 4 bytes".to_owned())
        );
        assert_eq!(
            from_length_prefixed(&[0, 5, 9, 9, 9], 2),
            Err("NAL unit at byte 0 declares 5 bytes but 3 remain".to_owned())
        );
        assert_eq!(
            from_length_prefixed(&[1, 9, 0, 2, 9], 1),
            Err("NAL unit at byte 2 is empty".to_owned())
        );
    }
}
