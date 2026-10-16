//! MPEG-DASH `SegmentBase` addressing: one file whose `sidx` box, at
//! `@indexRange`, lists the byte range and duration of each subsegment.
//!
//! Once fetched, the index is rewritten as an equivalent `SegmentList` of
//! `SegmentURL@mediaRange` entries with a `SegmentTimeline`, so the rest of the
//! crate only ever sees `SegmentList` or `SegmentTemplate` addressing.
//!
//! Only built with a network feature: without one, nothing can fetch an index.

use std::fmt;
use std::ops::RangeInclusive;

use url::Url;

use crate::vendor::dash_mpd_core::{
    Initialization, S, SegmentBase, SegmentList, SegmentTimeline, SegmentURL,
};

use super::{DashManifest, Selected};
use crate::error::Error;
use crate::fragment::{Fragment, parse_byte_range};

/// A `SegmentBase` index a [`DashManifest`] needs before one of its
/// representations can stream: the `sidx` byte range of that
/// representation's media file. Returned by [`DashManifest::index_requests`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexRequest {
    /// The representation's position in document order.
    position: usize,
    representation: String,
    fragment: Fragment,
}

impl IndexRequest {
    /// The bytes to fetch: a ranged [`Fragment`] of the media file.
    pub(crate) fn fragment(&self) -> &Fragment {
        &self.fragment
    }
}

impl DashManifest {
    /// Lists the `SegmentBase` indexes still to fetch, in document order.
    /// Hand each fetched body to [`DashManifest::apply_index`], or a failure
    /// to [`DashManifest::fail_index`].
    ///
    /// Empty once every index is loaded, and for manifests that address
    /// segments with `SegmentTemplate` or `SegmentList`. A representation
    /// whose index cannot even be located has its error recorded instead, to
    /// report when that representation is streamed.
    pub(crate) fn index_requests(&mut self) -> Vec<IndexRequest> {
        let mut requests = Vec::new();
        let mut failures = Vec::new();
        for (position, selected) in self.selections().enumerate() {
            match selected.index_fragment(self.base_url.as_ref()) {
                Ok(Some(fragment)) => requests.push(IndexRequest {
                    position,
                    representation: selected.id().to_owned(),
                    fragment,
                }),
                Ok(None) => {}
                Err(error) => failures.push((selected.id().to_owned(), error)),
            }
        }
        for (representation, error) in failures {
            self.record_index_failure(representation, error.to_string());
        }
        requests
    }

    /// Loads the index `request` asked for from its fetched `body`, which
    /// makes that representation streamable. A body that is not a usable
    /// `sidx` is recorded as that representation's failure.
    pub(crate) fn apply_index(&mut self, request: &IndexRequest, body: &[u8]) {
        let list = self
            .selections()
            .nth(request.position)
            .expect("an index request names a representation of this manifest")
            .bases()
            .segment_list(&request.fragment, body);
        match list {
            Ok(list) => {
                // `selections` yields representations in this same document order.
                let representation = self
                    .mpd
                    .periods
                    .iter_mut()
                    .flat_map(|period| &mut period.adaptations)
                    .flat_map(|adaptation| &mut adaptation.representations)
                    .nth(request.position)
                    .expect("an index request names a representation of this manifest");
                representation.SegmentList = Some(list);
                self.segment_index_errors.remove(&request.representation);
            }
            Err(error) => {
                self.record_index_failure(request.representation.clone(), error.to_string())
            }
        }
    }

    /// Records that fetching the index `request` asked for failed, so
    /// streaming that representation reports `reason`. Other representations
    /// are unaffected.
    pub(crate) fn fail_index(&mut self, request: &IndexRequest, reason: impl fmt::Display) {
        self.record_index_failure(request.representation.clone(), reason.to_string());
    }

    fn record_index_failure(&mut self, representation: String, reason: String) {
        #[cfg(feature = "log")]
        log::warn!(
            "SegmentBase index for representation {representation} failed to load: {reason}"
        );
        self.segment_index_errors.insert(representation, reason);
    }
}

impl<'a> Selected<'a> {
    /// Segment bases from the most to the least specific level.
    fn bases(self) -> BaseChain<'a> {
        BaseChain::new([
            self.representation.SegmentBase.as_ref(),
            self.adaptation.SegmentBase.as_ref(),
            self.period.SegmentBase.as_ref(),
        ])
    }

    /// The location of this representation's `SegmentBase` index, or `None` if it
    /// does not need one or its URL is relative and there is no manifest URL
    /// yet.
    fn index_fragment(self, manifest_url: Option<&Url>) -> Result<Option<Fragment>, Error> {
        if !self.needs_segment_index() {
            return Ok(None);
        }
        let url = match self.base_url(manifest_url) {
            Ok(Some(url)) => url,
            Ok(None) | Err(Error::DashManifestUrl(_, url::ParseError::RelativeUrlWithoutBase))
                if manifest_url.is_none() =>
            {
                return Ok(None);
            }
            Ok(None) => return Err(Error::DashManifestMissingUrls),
            Err(error) => return Err(error),
        };
        self.bases().index_fragment(url.as_str()).map(Some)
    }
}

/// `SegmentBase` elements a representation inherits from, most specific first
/// (Representation, AdaptationSet, Period).
#[derive(Clone, Copy)]
struct BaseChain<'a> {
    levels: [Option<&'a SegmentBase>; 3],
}

impl<'a> BaseChain<'a> {
    fn new(levels: [Option<&'a SegmentBase>; 3]) -> Self {
        Self { levels }
    }

    /// First value of a `SegmentBase` attribute, honoring DASH inheritance.
    fn get<T>(self, field: impl Fn(&'a SegmentBase) -> Option<T>) -> Option<T> {
        self.levels.into_iter().flatten().find_map(field)
    }

    /// The `sidx` bytes at `@indexRange` of the resource at `url`, to fetch
    /// and hand to [`Self::segment_list`].
    fn index_fragment(self, url: &str) -> Result<Fragment, Error> {
        let index_range = self
            .get(|base| base.indexRange.as_deref())
            .ok_or_else(|| invalid("SegmentBase has no @indexRange".to_owned()))
            .and_then(parse_byte_range)?;
        Ok(Fragment::with_range(url, index_range))
    }

    /// Rewrites the fetched `body` of `index`, from [`Self::index_fragment`],
    /// as a `SegmentList`.
    ///
    /// Without an `Initialization` element, the initialization is taken to be
    /// every byte before the index, where `ftyp` and `moov` normally sit.
    fn segment_list(self, index: &Fragment, body: &[u8]) -> Result<SegmentList, Error> {
        let index_range = index
            .range
            .clone()
            .ok_or_else(|| invalid("index fragment has no byte range".to_owned()))?;
        let index = SegmentIndex::parse(body, *index_range.start())?;

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
    use crate::fragment::Fragments;
    use crate::manifest::tests::ranged;

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

    /// Serves byte ranges of one in-memory file and records each request.
    #[cfg(feature = "sync")]
    struct OneFile {
        body: Vec<u8>,
        requests: std::sync::Mutex<Vec<Fragment>>,
    }

    #[cfg(feature = "sync")]
    impl crate::Transport for OneFile {
        fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<crate::Bytes, Error> {
            self.requests.lock().unwrap().push(fragment.clone());
            let range = fragment
                .range
                .clone()
                .unwrap_or(0..=self.body.len() as u64 - 1);
            Ok(crate::Bytes::copy_from_slice(
                &self.body[*range.start() as usize..=*range.end() as usize],
            ))
        }
    }

    /// An 838-byte initialization, a `sidx` for two subsegments, then their media.
    pub(crate) fn segment_base_file() -> (Vec<u8>, String) {
        let sidx = sidx(0, 44_100, 0, &[(100, 4096), (50, 1024)]);
        let index_range = format!("838-{}", 838 + sidx.len() - 1);
        let mut body = vec![b'i'; 838];
        body.extend(sidx);
        body.extend([b'a'; 100]);
        body.extend([b'b'; 50]);
        (body, index_range)
    }

    fn segment_base_manifest(segment_base: &str) -> DashManifest {
        let manifest = DashManifest::new(format!(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation id="FLAC">
                <BaseURL>track.mp4</BaseURL>{segment_base}
            </Representation></AdaptationSet></Period></MPD>"#
        ))
        .unwrap();
        // Parsing never fetches the index.
        assert!(matches!(
            manifest.fragments("FLAC"),
            Err(Error::DashManifestInvalidSegments(reason)) if reason.contains("not loaded")
        ));
        manifest
    }

    #[test]
    #[cfg(feature = "sync")]
    fn segment_base_index_becomes_ranged_fragments() {
        use crate::fragment::MediaTimeline;
        use crate::stream::FragmentCache;
        use std::io::Read;
        use std::sync::Arc;

        let (body, index_range) = segment_base_file();
        let index_start = *parse_range(&index_range).start();
        let index_end = *parse_range(&index_range).end();
        let mut manifest =
            segment_base_manifest(&format!(r#"<SegmentBase indexRange="{index_range}"/>"#));
        manifest.base_url = Some(Url::parse("https://cdn.example/audio/").unwrap());
        let transport = Arc::new(OneFile {
            body: body.clone(),
            requests: Default::default(),
        });
        manifest.load_segment_indexes(transport.as_ref());

        let url = "https://cdn.example/audio/track.mp4";
        assert_eq!(
            *transport.requests.lock().unwrap(),
            [ranged(url, index_start, index_end)]
        );
        // Without an Initialization element, everything before the index is the init.
        assert_eq!(
            manifest.fragments("FLAC").unwrap(),
            Fragments::new(
                ranged(url, 0, 837),
                [
                    ranged(url, index_end + 1, index_end + 100),
                    ranged(url, index_end + 101, index_end + 150),
                ]
            )
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap(),
            MediaTimeline {
                timescale: 44_100,
                media_durations: vec![4096, 1024],
            }
        );

        let mut reader = manifest
            .stream_with_cache("FLAC", Arc::new(FragmentCache::new(transport)))
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        let mut expected = body[..838].to_vec();
        expected.extend(&body[index_end as usize + 1..]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn segment_base_keeps_its_initialization_element() {
        let (body, index_range) = segment_base_file();
        let mut manifest = segment_base_manifest(&format!(
            r#"<SegmentBase indexRange="{index_range}"><Initialization range="0-99"/></SegmentBase>"#
        ));
        manifest.base_url = Some(Url::parse("https://cdn.example/").unwrap());
        let requests = manifest.index_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].representation, "FLAC");
        let index = requests[0].fragment().range.clone().unwrap();
        manifest.apply_index(
            &requests[0],
            &body[*index.start() as usize..=*index.end() as usize],
        );
        assert!(manifest.index_requests().is_empty());
        assert_eq!(
            manifest.fragments("FLAC").unwrap().init,
            ranged("https://cdn.example/track.mp4", 0, 99)
        );
    }

    #[test]
    #[cfg(feature = "sync")]
    fn failed_segment_base_index_only_fails_its_representation() {
        let mut manifest = DashManifest::new(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
                <Representation id="FLAC">
                    <BaseURL>track.mp4</BaseURL><SegmentBase indexRange="0-9"/>
                </Representation>
                <Representation id="AACLC">
                    <SegmentTemplate initialization="init" media="$Number$">
                        <SegmentTimeline><S d="1"/></SegmentTimeline>
                    </SegmentTemplate>
                </Representation>
            </AdaptationSet></Period></MPD>"#,
        )
        .unwrap();
        manifest.base_url = Some(Url::parse("https://cdn.example/").unwrap());
        let transport = OneFile {
            body: vec![0; 10],
            requests: Default::default(),
        };
        manifest.load_segment_indexes(&transport);

        let error = manifest.fragments("FLAC").unwrap_err();
        assert!(matches!(
            &error,
            Error::DashManifestSegmentIndex { representation, reason }
                if representation == "FLAC" && reason.contains("no sidx box")
        ));
        assert!(matches!(
            manifest.media_timeline("FLAC"),
            Err(Error::DashManifestSegmentIndex { .. })
        ));
        assert_eq!(manifest.mime_type("FLAC").unwrap(), Some("audio/mp4"));
        assert_eq!(manifest.fragments("AACLC").unwrap().media.len(), 1);

        // A later successful load clears the failure.
        let (body, index_range) = segment_base_file();
        manifest.mpd.periods[0].adaptations[0].representations[0]
            .SegmentBase
            .as_mut()
            .unwrap()
            .indexRange = Some(index_range);
        manifest.load_segment_indexes(&OneFile {
            body,
            requests: Default::default(),
        });
        assert_eq!(manifest.fragments("FLAC").unwrap().media.len(), 2);
    }

    #[cfg(feature = "sync")]
    fn parse_range(value: &str) -> std::ops::RangeInclusive<u64> {
        crate::fragment::parse_byte_range(value).unwrap()
    }
}
