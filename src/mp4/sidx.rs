//! Segment index boxes (`sidx`, ISO/IEC 14496-12 §8.16.3).

use super::reader::{ParseError, atoms};

/// A parsed `sidx` box.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SegmentIndex {
    /// Ticks per second of `earliest_presentation_time` and each reference's duration.
    pub(crate) timescale: u32,
    /// Presentation time of the first referenced media, in `timescale` ticks.
    pub(crate) earliest_presentation_time: u64,
    /// Bytes from the end of this box to the first referenced byte.
    pub(crate) first_offset: u64,
    pub(crate) references: Vec<Reference>,
    /// Offset just past this box in the bytes it was found in.
    pub(crate) end: usize,
}

/// One `sidx` reference: a subsegment, or another `sidx`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reference {
    /// Whether this references another `sidx` rather than media.
    pub(crate) is_index: bool,
    /// Size of the referenced bytes.
    pub(crate) size: u32,
    /// Duration in `timescale` ticks.
    pub(crate) duration: u32,
    /// Whether the referenced media starts with a stream access point.
    pub(crate) starts_with_sap: bool,
    /// Stream access point type (ISO/IEC 14496-12 Annex I), `0` when unknown.
    pub(crate) sap_type: u8,
}

/// Parses the first top-level `sidx` box in `body`, or `None` if there is none.
pub(crate) fn find(body: &[u8]) -> Result<Option<SegmentIndex>, ParseError> {
    let Some(sidx) = super::reader::find(atoms(body, 0, body.len()), b"sidx")? else {
        return Ok(None);
    };
    let mut fields = sidx.fields(body);
    let (version, _) = fields.version_flags()?;
    fields.skip(4)?; // reference_ID
    let timescale = fields.u32()?;
    let (earliest_presentation_time, first_offset) = match version {
        0 => (u64::from(fields.u32()?), u64::from(fields.u32()?)),
        _ => (fields.u64()?, fields.u64()?),
    };
    fields.skip(2)?; // reserved
    let count = fields.u16()?;

    let mut references = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let reference = fields.u32()?;
        let duration = fields.u32()?;
        let sap = fields.u32()?;
        references.push(Reference {
            is_index: reference & 0x8000_0000 != 0,
            size: reference & 0x7fff_ffff,
            duration,
            starts_with_sap: sap & 0x8000_0000 != 0,
            sap_type: ((sap >> 28) & 0x7) as u8,
        });
    }
    Ok(Some(SegmentIndex {
        timescale,
        earliest_presentation_time,
        first_offset,
        references,
        end: sidx.end,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `sidx` box with `first_offset` and one `(size, duration)` reference each,
    /// every one starting with a type 1 stream access point.
    pub(crate) fn sidx(
        version: u8,
        timescale: u32,
        first_offset: u32,
        refs: &[(u32, u32)],
    ) -> Vec<u8> {
        sidx_at(version, timescale, 0, first_offset, refs)
    }

    /// [`sidx`] with an earliest presentation time of `start`.
    pub(crate) fn sidx_at(
        version: u8,
        timescale: u32,
        start: u32,
        first_offset: u32,
        refs: &[(u32, u32)],
    ) -> Vec<u8> {
        let mut payload = vec![version, 0, 0, 0];
        payload.extend(1u32.to_be_bytes()); // reference_ID
        payload.extend(timescale.to_be_bytes());
        if version == 0 {
            payload.extend(start.to_be_bytes());
            payload.extend(first_offset.to_be_bytes());
        } else {
            payload.extend(u64::from(start).to_be_bytes());
            payload.extend(u64::from(first_offset).to_be_bytes());
        }
        payload.extend([0, 0]);
        payload.extend((refs.len() as u16).to_be_bytes());
        for &(size, duration) in refs {
            payload.extend(size.to_be_bytes());
            payload.extend(duration.to_be_bytes());
            payload.extend(0x9000_0000u32.to_be_bytes()); // starts_with_SAP, SAP type 1
        }
        let mut sidx = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        sidx.extend(b"sidx");
        sidx.extend(payload);
        sidx
    }

    #[test]
    fn reads_both_versions_and_sap_fields() {
        for version in [0, 1] {
            let body = sidx_at(version, 44_100, 7, 10, &[(100, 4096), (0x8000_0032, 1024)]);
            let index = find(&body).unwrap().unwrap();
            assert_eq!(index.timescale, 44_100);
            assert_eq!(index.earliest_presentation_time, 7);
            assert_eq!(index.first_offset, 10);
            assert_eq!(index.end, body.len());
            assert_eq!(
                index.references,
                [
                    Reference {
                        is_index: false,
                        size: 100,
                        duration: 4096,
                        starts_with_sap: true,
                        sap_type: 1,
                    },
                    Reference {
                        is_index: true,
                        size: 50,
                        duration: 1024,
                        starts_with_sap: true,
                        sap_type: 1,
                    },
                ]
            );
        }
    }

    #[test]
    fn absent_and_truncated() {
        assert_eq!(find(b"").unwrap(), None);
        let body = sidx(0, 1, 0, &[(1, 1)]);
        assert!(find(&body[..30]).is_err());
        // A reference count larger than the box holds.
        let mut short = sidx(0, 1, 0, &[(1, 1)]);
        let at = short.len() - 12 - 2;
        short[at..at + 2].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(find(&short), Err(ParseError::Malformed(_))));
    }
}
