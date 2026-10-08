//! MPEG-DASH `SegmentBase` addressing: one file whose `sidx` box, at
//! `@indexRange`, lists the byte range and duration of each subsegment.
//!
//! Once fetched, the index is rewritten as an equivalent `SegmentList` of
//! `SegmentURL@mediaRange` entries with a `SegmentTimeline`, so the rest of the
//! crate only ever sees `SegmentList` or `SegmentTemplate` addressing.

use std::ops::RangeInclusive;

use dash_mpd_core::{Initialization, S, SegmentBase, SegmentList, SegmentTimeline, SegmentURL};

use crate::error::Error;
use crate::stream::{Fragment, Transport, parse_byte_range};

/// `SegmentBase` elements a representation inherits from, most specific first
/// (Representation, AdaptationSet, Period).
#[derive(Clone, Copy)]
pub(crate) struct BaseChain<'a> {
    levels: [Option<&'a SegmentBase>; 3],
}

impl<'a> BaseChain<'a> {
    pub fn new(levels: [Option<&'a SegmentBase>; 3]) -> Self {
        Self { levels }
    }

    /// Whether any level has a `SegmentBase`.
    pub fn is_present(self) -> bool {
        self.levels.iter().any(Option::is_some)
    }

    /// First value of a `SegmentBase` attribute, honoring DASH inheritance.
    fn get<T>(self, field: impl Fn(&'a SegmentBase) -> Option<T>) -> Option<T> {
        self.levels.into_iter().flatten().find_map(field)
    }

    /// Fetches the `sidx` at `@indexRange` of the resource at `url` and
    /// rewrites it as a `SegmentList`.
    ///
    /// Without an `Initialization` element, the initialization is taken to be
    /// every byte before the index, where `ftyp` and `moov` normally sit.
    pub fn segment_list(self, url: &str, transport: &dyn Transport) -> Result<SegmentList, Error> {
        let index_range = self
            .get(|base| base.indexRange.as_deref())
            .ok_or_else(|| invalid("SegmentBase has no @indexRange".to_owned()))
            .and_then(parse_byte_range)?;
        let fragment = Fragment::with_range(url, index_range.clone());
        let body = transport.get(&fragment, fragment.known_size())?;
        let index = SegmentIndex::parse(&body, *index_range.start())?;

        let initialization = match self.get(|base| base.Initialization.as_ref()) {
            Some(initialization) => initialization.clone(),
            None if *index_range.start() > 0 => Initialization {
                sourceURL: None,
                range: Some(format!("0-{}", index_range.start() - 1)),
            },
            None => return Err(Error::DashManifestMissingUrls),
        };

        Ok(SegmentList {
            timescale: Some(u64::from(index.timescale)),
            Initialization: Some(initialization),
            SegmentTimeline: Some(SegmentTimeline {
                segments: index
                    .subsegments
                    .iter()
                    .map(|subsegment| S {
                        d: subsegment.duration,
                        ..S::default()
                    })
                    .collect(),
            }),
            segment_urls: index
                .subsegments
                .iter()
                .map(|subsegment| SegmentURL {
                    mediaRange: Some(format!(
                        "{}-{}",
                        subsegment.range.start(),
                        subsegment.range.end()
                    )),
                    ..SegmentURL::default()
                })
                .collect(),
            ..SegmentList::default()
        })
    }
}

/// One media subsegment referenced by a `sidx`.
#[derive(Debug, PartialEq, Eq)]
struct Subsegment {
    /// Absolute byte range within the resource.
    range: RangeInclusive<u64>,
    /// Duration in `sidx` timescale units.
    duration: u64,
}

/// The parsed `sidx` (ISO/IEC 14496-12 §8.16.3) of a `SegmentBase` resource.
#[derive(Debug, PartialEq, Eq)]
struct SegmentIndex {
    timescale: u32,
    subsegments: Vec<Subsegment>,
}

impl SegmentIndex {
    /// Parses the first `sidx` box in `body`, the bytes of `@indexRange`
    /// starting at absolute offset `body_offset`.
    fn parse(body: &[u8], body_offset: u64) -> Result<Self, Error> {
        let (box_start, box_end) = find_sidx(body)?;
        let mut fields = Fields {
            buf: &body[..box_end],
            pos: box_start,
        };
        let version = fields.u8()?;
        fields.skip(3 + 4)?; // flags, reference_ID
        let timescale = fields.u32()?;
        if timescale == 0 {
            return Err(Error::DashManifestMissingTimescale);
        }
        let first_offset = if version == 0 {
            fields.skip(4)?; // earliest_presentation_time
            u64::from(fields.u32()?)
        } else {
            fields.skip(8)?;
            fields.u64()?
        };
        fields.skip(2)?; // reserved
        let reference_count = fields.u16()?;

        // Offsets count from the first byte after the sidx box.
        let mut start = body_offset
            .checked_add(box_end as u64)
            .and_then(|end| end.checked_add(first_offset))
            .ok_or_else(|| invalid("sidx offsets overflow".to_owned()))?;
        let mut subsegments = Vec::with_capacity(usize::from(reference_count));
        for _ in 0..reference_count {
            let reference = fields.u32()?;
            let duration = u64::from(fields.u32()?);
            fields.skip(4)?; // SAP fields
            if reference & 0x8000_0000 != 0 {
                return Err(invalid(
                    "hierarchical sidx (references to other sidx boxes) is not supported"
                        .to_owned(),
                ));
            }
            let size = u64::from(reference & 0x7fff_ffff);
            if size == 0 {
                return Err(invalid("sidx references an empty subsegment".to_owned()));
            }
            let end = start
                .checked_add(size - 1)
                .ok_or_else(|| invalid("sidx offsets overflow".to_owned()))?;
            subsegments.push(Subsegment {
                range: start..=end,
                duration,
            });
            start = end.saturating_add(1);
        }
        if subsegments.is_empty() {
            return Err(invalid("sidx has no references".to_owned()));
        }
        Ok(Self {
            timescale,
            subsegments,
        })
    }
}

/// Payload start and end of the first top-level `sidx` box in `body`.
fn find_sidx(body: &[u8]) -> Result<(usize, usize), Error> {
    let mut pos = 0;
    while pos + 8 <= body.len() {
        let mut fields = Fields { buf: body, pos };
        let size = fields.u32()?;
        let kind = fields.array::<4>()?;
        let size = match size {
            0 => (body.len() - pos) as u64,
            1 => fields.u64()?,
            size => u64::from(size),
        };
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| pos.checked_add(size))
            .filter(|&end| end <= body.len() && end >= fields.pos)
            .ok_or_else(|| invalid("index range ends inside an MP4 box".to_owned()))?;
        if &kind == b"sidx" {
            return Ok((fields.pos, end));
        }
        pos = end;
    }
    Err(invalid("index range contains no sidx box".to_owned()))
}

/// Big-endian fields read from a box.
struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Fields<'_> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let bytes = self
            .buf
            .get(self.pos..self.pos + N)
            .ok_or_else(|| invalid("truncated sidx box".to_owned()))?;
        self.pos += N;
        Ok(bytes.try_into().expect("length checked"))
    }

    fn skip(&mut self, len: usize) -> Result<(), Error> {
        if self.pos + len > self.buf.len() {
            return Err(invalid("truncated sidx box".to_owned()));
        }
        self.pos += len;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.array().map(u32::from_be_bytes)
    }

    fn u64(&mut self) -> Result<u64, Error> {
        self.array().map(u64::from_be_bytes)
    }
}

fn invalid(reason: String) -> Error {
    Error::DashManifestInvalidSegments(format!("SegmentBase: {reason}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `sidx` box with `first_offset` and one `(size, duration)` reference each.
    pub(crate) fn sidx(
        version: u8,
        timescale: u32,
        first_offset: u32,
        refs: &[(u32, u32)],
    ) -> Vec<u8> {
        let mut payload = vec![version, 0, 0, 0];
        payload.extend(1u32.to_be_bytes()); // reference_ID
        payload.extend(timescale.to_be_bytes());
        if version == 0 {
            payload.extend(0u32.to_be_bytes());
            payload.extend(first_offset.to_be_bytes());
        } else {
            payload.extend(0u64.to_be_bytes());
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
    fn parses_sidx_into_absolute_ranges() {
        for version in [0, 1] {
            let body = sidx(version, 44_100, 10, &[(100, 4096), (50, 1024)]);
            let end = 838 + body.len() as u64;
            assert_eq!(
                SegmentIndex::parse(&body, 838).unwrap(),
                SegmentIndex {
                    timescale: 44_100,
                    subsegments: vec![
                        Subsegment {
                            range: end + 10..=end + 109,
                            duration: 4096,
                        },
                        Subsegment {
                            range: end + 110..=end + 159,
                            duration: 1024,
                        },
                    ],
                }
            );
        }
    }

    #[test]
    fn skips_boxes_before_sidx() {
        let mut body = vec![0, 0, 0, 12];
        body.extend(b"free");
        body.extend([0; 4]);
        body.extend(sidx(0, 1000, 0, &[(7, 1)]));
        let end = body.len() as u64;
        assert_eq!(
            SegmentIndex::parse(&body, 0).unwrap().subsegments,
            [Subsegment {
                range: end..=end + 6,
                duration: 1,
            }]
        );
    }

    #[test]
    fn rejects_bad_sidx() {
        let reject = |body: &[u8]| SegmentIndex::parse(body, 0).unwrap_err().to_string();
        assert!(reject(b"").contains("no sidx box"));
        assert!(reject(&sidx(0, 1, 0, &[(1, 1)])[..30]).contains("inside an MP4 box"));
        assert!(reject(&sidx(0, 1, 0, &[])).contains("no references"));
        assert!(reject(&sidx(0, 1, 0, &[(0x8000_0001, 1)])).contains("hierarchical"));
        assert!(matches!(
            SegmentIndex::parse(&sidx(0, 0, 0, &[(1, 1)]), 0),
            Err(Error::DashManifestMissingTimescale)
        ));
    }
}
