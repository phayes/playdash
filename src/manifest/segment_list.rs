//! MPEG-DASH `SegmentList` expansion: explicit `SegmentURL` media, optionally
//! narrowed by `@mediaRange`.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::vendor::dash_mpd_core::{SegmentList, SegmentURL};

use super::segments::{Addressing, Timing, initialization_location};
use crate::error::Error;
use crate::segment::{MediaSegment, SegmentLocation, parse_byte_range};

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

    /// The list's segments, with unresolved URLs: one media segment per
    /// `SegmentURL`, timed by `@duration` or a `SegmentTimeline`. A missing
    /// `@media` means the BaseURL itself.
    ///
    /// The list length bounds the timing, so `@duration` lists need no Period
    /// duration. Segments past the end of the Period are dropped.
    pub fn segments(self, period_duration: Option<Duration>) -> Result<ListSegments, Error> {
        let segment_urls = self.segment_urls();
        let count = segment_urls.len() as u64;
        if count == 0 {
            return Err(Error::DashManifestInvalidSegments(
                "SegmentList has no SegmentURL elements".to_owned(),
            ));
        }
        let addressed = Timing {
            live: false,
            timescale: self.get(|list| list.timescale),
            start_number: None,
            end_number: Some(count),
            presentation_time_offset: self.get(|list| list.presentationTimeOffset),
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
        .addressed(period_duration)?;

        // A SegmentList is finite; its entry count supplies the missing end number.
        #[cfg(feature = "log")]
        if let Some(timed) = addressed.segments.len()
            && timed < count
        {
            log::warn!(
                "SegmentList timing covers {timed} of {count} SegmentURL elements; ignoring the rest"
            );
        }
        let initialization = self
            .get(|list| list.Initialization.as_ref())
            .ok_or(Error::DashManifestMissingUrls)?;
        let init = initialization_location(initialization)?;

        // Each SegmentURL in turn with each timed segment, as far as both go.
        let media = segment_urls
            .iter()
            .zip(addressed.segments.times())
            .map(|(segment_url, time)| {
                let location = SegmentLocation {
                    url: segment_url.media.clone().unwrap_or_default(),
                    range: segment_url
                        .mediaRange
                        .as_deref()
                        .map(parse_byte_range)
                        .transpose()?,
                };
                Ok(MediaSegment {
                    location,
                    start: time.time,
                    duration: time.duration,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(ListSegments {
            init,
            timescale: addressed.timescale,
            presentation_time_offset: addressed.presentation_time_offset,
            media,
        })
    }
}

/// A representation's `SegmentList` segments, with unresolved URLs.
pub(crate) struct ListSegments {
    pub init: SegmentLocation,
    pub timescale: NonZeroU32,
    /// `@presentationTimeOffset`: media time at the start of the Period.
    pub presentation_time_offset: u64,
    /// Media segments in timeline order, on the `timescale` clock.
    pub media: Vec<MediaSegment>,
}
