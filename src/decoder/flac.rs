//! FLAC initialization: a `dfLa` box's metadata blocks as a FLAC stream header, and the
//! stream parameters its STREAMINFO gives.

use super::DecoderParameters;
use crate::mp4::codec::{STREAMINFO_LEN, parse_dfla};
use crate::mp4::reader::ParseError;
use bytes::Bytes;

/// The FLAC stream marker.
const MARKER: &[u8; 4] = b"fLaC";

/// Where a `dfLa` payload's metadata blocks start: after its version and flags.
const BLOCKS: usize = 4;

/// Where the STREAMINFO body sits in a stream header: after the marker and its block
/// header.
const STREAMINFO: std::ops::Range<usize> = 8..8 + STREAMINFO_LEN;

/// The FLAC stream header and decoder parameters for `dfla`, a `dfLa` payload. The header
/// is the `fLaC` marker, then the metadata blocks, the first of them STREAMINFO and the
/// last flagged as last.
pub(crate) fn initialization(dfla: &[u8]) -> Result<(Bytes, DecoderParameters), ParseError> {
    let setup = parse_dfla(dfla)?;
    let mut header = MARKER.to_vec();
    header.extend_from_slice(&dfla[BLOCKS..]);
    let header = Bytes::from(header);
    let parameters = DecoderParameters::Flac {
        sample_rate: setup.sample_rate,
        channels: setup.channels,
        bits_per_sample: setup.bits_per_sample,
        streaminfo: header.slice(STREAMINFO),
    };
    Ok((header, parameters))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A metadata block of `kind` holding `body`, flagged last if `last`.
    fn block(last: bool, kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![u8::from(last) << 7 | kind];
        out.extend(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend(body);
        out
    }

    /// A STREAMINFO body for 44.1 kHz stereo at 24 bits.
    fn streaminfo() -> Vec<u8> {
        let mut body = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
        // 44100 in 20 bits, then channels - 1 = 1 and bits - 1 = 23.
        body.extend((44_100u32 << 12 | 1 << 9 | 23 << 4).to_be_bytes());
        body.resize(STREAMINFO_LEN, 0);
        body
    }

    #[test]
    fn the_header_is_the_marker_then_the_blocks() {
        let streaminfo_block = block(false, 0, &streaminfo());
        let padding = block(true, 1, &[0; 3]);
        let mut dfla = vec![0, 0, 0, 0];
        dfla.extend(&streaminfo_block);
        dfla.extend(&padding);
        let mut expected = b"fLaC".to_vec();
        expected.extend(&streaminfo_block);
        expected.extend(&padding);

        let (header, parameters) = initialization(&dfla).unwrap();
        assert_eq!(header, expected);
        assert_eq!(
            parameters,
            DecoderParameters::Flac {
                sample_rate: 44_100,
                channels: 2,
                bits_per_sample: 24,
                streaminfo: Bytes::from(streaminfo()),
            }
        );
    }

    #[test]
    fn inconsistent_blocks_are_malformed() {
        let reason = |blocks: &[Vec<u8>]| {
            let mut dfla = vec![0, 0, 0, 0];
            dfla.extend(blocks.concat());
            match initialization(&dfla) {
                Err(ParseError::Malformed(reason)) => reason,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            reason(&[block(true, 1, &[0; 3])]),
            "dfLa does not start with STREAMINFO"
        );
        assert_eq!(
            reason(&[block(true, 0, &[7; 33])]),
            "dfLa has a 33-byte STREAMINFO"
        );
        // No block is flagged last.
        assert_eq!(
            reason(&[block(false, 0, &streaminfo())]),
            "truncated dfLa box"
        );
        assert_eq!(
            reason(&[block(false, 0, &streaminfo()), block(false, 1, &[0; 2])]),
            "truncated dfLa box"
        );
        assert_eq!(
            reason(&[block(true, 0, &streaminfo()), vec![0]]),
            "dfLa has bytes after its last metadata block"
        );
        let mut overrun = block(true, 0, &streaminfo());
        overrun.pop();
        assert_eq!(reason(&[overrun]), "truncated dfLa box");
        assert!(matches!(
            initialization(&[1, 0, 0, 0]),
            Err(ParseError::Unsupported(reason)) if reason == "dfLa has version 1"
        ));
    }
}
