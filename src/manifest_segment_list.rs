//! MPEG-DASH `SegmentList` expansion: explicit `SegmentURL` media, optionally
//! narrowed by `@mediaRange`.

use std::time::Duration;

use dash_mpd_core::{SegmentList, SegmentURL};

use crate::error::Error;
use crate::manifest_template::{Addressing, Segments, Timing, initialization_fragment};
use crate::stream::{Fragment, parse_byte_range};

/// `SegmentList` elements a representation inherits from, most specific first
/// (Representation, AdaptationSet, Period).
#[derive(Clone, Copy)]
pub(crate) struct ListChain<'a> {
    levels: [Option<&'a SegmentList>; 3],
}

impl<'a> ListChain<'a> {
    pub fn new(levels: [Option<&'a SegmentList>; 3]) -> Self {
        Self { levels }
    }

    fn lists(self) -> impl Iterator<Item = &'a SegmentList> {
        self.levels.into_iter().flatten()
    }

    /// First value of a `SegmentList` attribute, honoring DASH inheritance.
    fn get<T>(self, field: impl Fn(&'a SegmentList) -> Option<T>) -> Option<T> {
        self.lists().find_map(field)
    }

    /// `SegmentURL` elements of the most specific list that has any.
    fn segment_urls(self) -> &'a [SegmentURL] {
        self.lists()
            .map(|list| list.segment_urls.as_slice())
            .find(|urls| !urls.is_empty())
            .unwrap_or_default()
    }

    /// One media segment per `SegmentURL`, timed by `@duration` or a `SegmentTimeline`.
    ///
    /// The list length bounds the timing, so `@duration` lists need no Period
    /// duration. Segments past the end of the Period are dropped.
    pub fn segments(self, period_duration: Option<Duration>) -> Result<Segments, Error> {
        let count = self.segment_urls().len() as u64;
        if count == 0 {
            return Err(Error::DashManifestInvalidSegments(
                "SegmentList has no SegmentURL elements".to_owned(),
            ));
        }
        let segments = Timing {
            timescale: self.get(|list| list.timescale),
            start_number: None,
            end_number: Some(count),
            presentation_time_offset: None,
            // Timeline and @duration are exclusive; the most specific level that sets either wins.
            addressing: self.lists().find_map(|list| {
                list.SegmentTimeline
                    .as_ref()
                    .map(Addressing::Timeline)
                    .or(list
                        .duration
                        .map(|duration| Addressing::Duration(duration as f64)))
            }),
        }
        .segments(period_duration)?;

        #[cfg(feature = "log")]
        if (segments.segments.len() as u64) < count {
            log::warn!(
                "SegmentList timing covers {} of {count} SegmentURL elements; ignoring the rest",
                segments.segments.len()
            );
        }
        Ok(segments)
    }

    /// Initialization fragment followed by one fragment per timed segment, with
    /// unresolved URLs. A missing `@media` means the BaseURL itself.
    pub fn fragments(self, segments: &Segments) -> Result<Vec<Fragment>, Error> {
        let initialization = self
            .get(|list| list.Initialization.as_ref())
            .ok_or(Error::DashManifestMissingUrls)?;

        let mut fragments = Vec::with_capacity(segments.segments.len() + 1);
        fragments.push(initialization_fragment(initialization)?);
        for segment_url in self.segment_urls().iter().take(segments.segments.len()) {
            fragments.push(Fragment {
                url: segment_url.media.clone().unwrap_or_default(),
                range: segment_url
                    .mediaRange
                    .as_deref()
                    .map(parse_byte_range)
                    .transpose()?,
            });
        }
        Ok(fragments)
    }
}
