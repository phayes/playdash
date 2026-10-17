//! Opus initialization: a `dOps` box as an Ogg Opus identification header and decoder
//! parameters.

use super::DecoderParameters;
use crate::mp4::codec::{OpusSetup, parse_dops};
use crate::mp4::reader::ParseError;
use bytes::Bytes;

/// The `OpusHead` (RFC 7845 §5.1) and decoder parameters for `dops`, a `dOps` payload.
pub(crate) fn initialization(dops: &[u8]) -> Result<(Bytes, DecoderParameters), ParseError> {
    let setup = parse_dops(dops)?;
    let parameters = DecoderParameters::Opus {
        channels: u16::from(setup.channels),
        pre_skip: setup.pre_skip,
        output_gain: setup.output_gain,
        mapping_family: setup.mapping_family,
        stream_count: setup.stream_count,
        coupled_count: setup.coupled_count,
        channel_mapping: setup.channel_mapping.clone(),
    };
    Ok((opus_head(&setup), parameters))
}

/// `setup` as an `OpusHead`, whose fields are little-endian, unlike `dOps`'s.
fn opus_head(setup: &OpusSetup) -> Bytes {
    let mut head = b"OpusHead".to_vec();
    head.extend([1, setup.channels]);
    head.extend(setup.pre_skip.to_le_bytes());
    head.extend(setup.input_sample_rate.to_le_bytes());
    head.extend(setup.output_gain.to_le_bytes());
    head.push(setup.mapping_family);
    if setup.mapping_family != 0 {
        head.extend([setup.stream_count, setup.coupled_count]);
        head.extend(&setup.channel_mapping);
    }
    Bytes::from(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_0_fills_in_its_implied_mapping() {
        // Stereo, pre-skip 312, 48 kHz input, gain -1 dB (-256 in Q7.8).
        let dops = [0, 2, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0xff, 0x00, 0];
        let (head, parameters) = initialization(&dops).unwrap();
        let mut expected = b"OpusHead".to_vec();
        expected.extend([1, 2, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0x00, 0xff, 0]);
        assert_eq!(head, expected);
        assert_eq!(
            parameters,
            DecoderParameters::Opus {
                channels: 2,
                pre_skip: 312,
                output_gain: -256,
                mapping_family: 0,
                stream_count: 1,
                coupled_count: 1,
                channel_mapping: vec![0, 1],
            }
        );
    }

    #[test]
    fn other_families_carry_their_mapping() {
        // 5.1 in family 1: four streams, two coupled.
        let dops = [
            0, 6, 0, 0, 0, 0, 0xbb, 0x80, 0, 0, 1, 4, 2, 0, 4, 1, 2, 3, 5,
        ];
        let (head, parameters) = initialization(&dops).unwrap();
        assert_eq!(&head[18..], [1, 4, 2, 0, 4, 1, 2, 3, 5]);
        assert!(matches!(
            parameters,
            DecoderParameters::Opus { stream_count: 4, coupled_count: 2, channel_mapping, .. }
                if channel_mapping == [0, 4, 1, 2, 3, 5]
        ));
    }

    #[test]
    fn bad_configurations_are_refused() {
        let reason = |dops: &[u8]| match initialization(dops) {
            Err(ParseError::Malformed(reason) | ParseError::Unsupported(reason)) => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            reason(&[0, 3, 0, 0, 0, 0, 0xbb, 0x80, 0, 0, 0]),
            "dOps has mapping family 0 with 3 channels"
        );
        assert_eq!(
            reason(&[0, 2, 0, 0, 0, 0, 0xbb, 0x80, 0, 0, 1, 1, 2, 0, 1]),
            "dOps has 1 streams, 2 of them coupled"
        );
        assert_eq!(
            reason(&[0, 6, 0, 0, 0, 0, 0xbb, 0x80, 0, 0, 1, 4, 2, 0]),
            "truncated dOps box"
        );
        assert_eq!(reason(&[1, 2]), "dOps has version 1");
    }
}
