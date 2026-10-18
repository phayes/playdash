//! A representation's media segments as an index: each media segment is named
//! by its [`SegmentNumber`], and its location and timing are worked out when
//! asked for.

use super::{MediaSegment, MediaTiming, SegmentLocation, SegmentNumber};
use crate::manifest::{FixedSegments, MediaUrls, TimelineSegments};
use std::ops::Range;
use std::time::Duration;
#[cfg(any(feature = "sync", feature = "async"))]
use std::time::SystemTime;

/// Padding applied to advertised media bytes when guessing total length.
/// AAC `bandwidth` is near the real rate; FLAC's is often uncompressed PCM.
const SYMPHONIA_COMPAT_MEDIA_FACTOR: u128 = 4;

/// Extra bytes reserved per media segment for `moof` / box overhead.
const SYMPHONIA_COMPAT_BYTES_PER_SEGMENT: u128 = 64 * 1024;

/// A representation's media segments, with their timing when known. Media
/// segments are numbered from zero in playback order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MediaIndex {
    /// Media segment locations without timing, in playback order.
    Untimed(Vec<SegmentLocation>),
    /// Timed media segments, in timeline order and not overlapping.
    List {
        timing: MediaTiming,
        segments: Vec<MediaSegment>,
    },
    /// Templated media segments of a fixed `@duration`.
    Template {
        timing: MediaTiming,
        segments: FixedSegments,
        urls: MediaUrls,
    },
    /// Templated media segments of a `SegmentTimeline`.
    Timeline {
        timing: MediaTiming,
        segments: TimelineSegments,
        urls: MediaUrls,
    },
}

/// A media segment of an index: its location, and the presentation time it
/// spans when timed.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexedSegment {
    pub(crate) location: SegmentLocation,
    pub(crate) span: Option<Range<Duration>>,
}

impl MediaIndex {
    /// The number of media segments.
    fn len(&self) -> u64 {
        match self {
            MediaIndex::Untimed(media) => media.len() as u64,
            MediaIndex::List { segments, .. } => segments.len() as u64,
            MediaIndex::Template { segments, .. } => segments.len(),
            MediaIndex::Timeline { segments, .. } => segments.len(),
        }
    }

    /// The numbers of the media segments, from the first to just past the
    /// last.
    pub(crate) fn numbers(&self) -> Range<SegmentNumber> {
        SegmentNumber(0)..SegmentNumber(self.len())
    }

    /// The numbers of the media segments a client may fetch at `now`. Every
    /// segment of a static index is always available.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn available(&self, _now: SystemTime) -> Range<SegmentNumber> {
        self.numbers()
    }

    /// The clock the media segments are timed on. `None` when untimed.
    pub(crate) fn timing(&self) -> Option<&MediaTiming> {
        match self {
            MediaIndex::Untimed(_) => None,
            MediaIndex::List { timing, .. }
            | MediaIndex::Template { timing, .. }
            | MediaIndex::Timeline { timing, .. } => Some(timing),
        }
    }

    /// The media start and duration, in ticks, of the segment numbered
    /// `number`. `None` when untimed or past the last.
    pub(crate) fn ticks(&self, number: SegmentNumber) -> Option<(u64, u64)> {
        match self {
            MediaIndex::Untimed(_) => None,
            MediaIndex::List { segments, .. } => {
                let segment = segments.get(position(number)?)?;
                Some((segment.start, segment.duration))
            }
            MediaIndex::Template { segments, .. } => segments.ticks(number.0),
            MediaIndex::Timeline { segments, .. } => segments.ticks(number.0),
        }
    }

    /// The presentation time the segment numbered `number` spans. `None` when
    /// untimed or past the last.
    pub(crate) fn span(&self, number: SegmentNumber) -> Option<Range<Duration>> {
        let timing = self.timing()?;
        let (start, duration) = self.ticks(number)?;
        let end = start.saturating_add(duration);
        Some(timing.presentation_time(start)..timing.presentation_time(end))
    }

    /// The location of the segment numbered `number`. `None` past the last.
    pub(crate) fn location(&self, number: SegmentNumber) -> Option<SegmentLocation> {
        match self {
            MediaIndex::Untimed(media) => media.get(position(number)?).cloned(),
            MediaIndex::List { segments, .. } => {
                Some(segments.get(position(number)?)?.location.clone())
            }
            MediaIndex::Template { segments, urls, .. } => {
                let (time, _) = segments.ticks(number.0)?;
                Some(urls.location(segments.number(number.0)?, time))
            }
            MediaIndex::Timeline { segments, urls, .. } => {
                let (time, _) = segments.ticks(number.0)?;
                Some(urls.location(segments.number(number.0)?, time))
            }
        }
    }

    /// The segment numbered `number`. `None` past the last.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn segment(&self, number: SegmentNumber) -> Option<IndexedSegment> {
        Some(IndexedSegment {
            location: self.location(number)?,
            span: self.span(number),
        })
    }

    /// The first segment ending after presentation time `time`, or just past
    /// the last if none does. `None` when untimed.
    pub(crate) fn ending_after(&self, time: Duration) -> Option<SegmentNumber> {
        self.partition(|span| span.end <= time)
    }

    /// The first segment starting at or after presentation time `time`, or
    /// just past the last if none does. `None` when untimed.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn starting_from(&self, time: Duration) -> Option<SegmentNumber> {
        self.partition(|span| span.start < time)
    }

    /// The segment covering presentation time `time`. A time before the first
    /// segment lands in the first, and one at or past the end in the last.
    /// `None` when untimed or without media.
    pub(crate) fn number_at(&self, time: Duration) -> Option<SegmentNumber> {
        let last = self.numbers().end.checked_prev()?;
        Some(self.ending_after(time)?.min(last))
    }

    /// The presentation time at which the last segment ends. `None` when
    /// untimed or without media.
    pub(crate) fn end_time(&self) -> Option<Duration> {
        let last = self.numbers().end.checked_prev()?;
        Some(self.span(last)?.end)
    }

    /// A conservative length estimate for a stream of the initialization
    /// segment, `init_len` bytes, and the media segments numbered from `first`
    /// on at `bitrate_bps`. `None` when untimed, without media from `first`,
    /// or for a zero `bitrate_bps`.
    pub(crate) fn estimate_len(
        &self,
        first: SegmentNumber,
        init_len: u64,
        bitrate_bps: u32,
    ) -> Option<u64> {
        let timing = self.timing()?;
        let media = first..self.numbers().end;
        if bitrate_bps == 0 || media.is_empty() {
            return None;
        }

        // Advertised media bytes, padded for bitrate underestimates and box overhead.
        let count = u128::from(media.end.0 - media.start.0);
        let ticks = self.media_ticks(media);
        let denom = u128::from(timing.timescale.get()) * 8;
        let bytes = (u128::from(bitrate_bps) * ticks).div_ceil(denom);
        let padded = bytes
            .saturating_mul(SYMPHONIA_COMPAT_MEDIA_FACTOR)
            .saturating_add(count.saturating_mul(SYMPHONIA_COMPAT_BYTES_PER_SEGMENT))
            .saturating_add(u128::from(init_len));
        Some(u64::try_from(padded).unwrap_or(u64::MAX))
    }

    /// The summed durations, in ticks, of the segments numbered in `range`.
    fn media_ticks(&self, range: Range<SegmentNumber>) -> u128 {
        let range = range.start.0..range.end.0;
        match self {
            MediaIndex::Untimed(_) => 0,
            MediaIndex::List { segments, .. } => {
                let slot = |number: u64| {
                    usize::try_from(number).map_or(segments.len(), |slot| slot.min(segments.len()))
                };
                let slots = slot(range.start)..slot(range.end);
                segments
                    .get(slots)
                    .unwrap_or_default()
                    .iter()
                    .map(|segment| u128::from(segment.duration))
                    .sum()
            }
            MediaIndex::Template { segments, .. } => segments.media_ticks(range),
            MediaIndex::Timeline { segments, .. } => segments.media_ticks(range),
        }
    }

    /// The first segment whose span fails `before`, which holds for a prefix
    /// of the segments, or just past the last if every span holds. `None` when
    /// untimed.
    fn partition(&self, before: impl Fn(&Range<Duration>) -> bool) -> Option<SegmentNumber> {
        self.timing()?;
        let Range { mut start, mut end } = self.numbers();
        while start < end {
            let middle = SegmentNumber(start.0 + (end.0 - start.0) / 2);
            match self.span(middle).is_some_and(|span| before(&span)) {
                true => start = middle.next(),
                false => end = middle,
            }
        }
        Some(start)
    }
}

/// The position of the segment numbered `number` in a list numbered from zero.
fn position(number: SegmentNumber) -> Option<usize> {
    usize::try_from(number.0).ok()
}

#[cfg(all(test, any(feature = "sync", feature = "async")))]
mod tests {
    use super::*;
    use crate::DashManifest;
    use crate::segment::{Segments, each, timed_media_for_test};

    /// The segments of a templated representation of a ten-second period, on
    /// a clock whose ticks do not land on whole nanoseconds.
    fn templated(id: &str) -> Segments {
        let xml = r#"
            <MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT10S">
              <Period>
                <AdaptationSet mimeType="audio/mp4">
                  <Representation id="fixed" bandwidth="1">
                    <SegmentTemplate initialization="i.mp4" media="$Number$.m4s"
                                     duration="4" timescale="3"/>
                  </Representation>
                  <Representation id="timeline" bandwidth="1">
                    <SegmentTemplate initialization="i.mp4" media="$Time$.m4s" timescale="3">
                      <SegmentTimeline>
                        <S t="0" d="2" r="2"/>
                        <S t="9" d="4" r="-1"/>
                      </SegmentTimeline>
                    </SegmentTemplate>
                  </Representation>
                </AdaptationSet>
              </Period>
            </MPD>
        "#;
        DashManifest::new(xml)
            .and_then(|manifest| manifest.with_base_url("https://cdn.example/"))
            .and_then(|manifest| manifest.segments(id))
            .unwrap()
    }

    /// The same segments as a list.
    fn listed(segments: &Segments) -> Segments {
        let media = timed_media_for_test(segments).unwrap();
        Segments::timed("i.mp4", *segments.timing().unwrap(), media).unwrap()
    }

    /// Times around every span boundary of `index`, and between them.
    fn times(index: &MediaIndex) -> Vec<Duration> {
        let mut times: Vec<Duration> = (0..=80)
            .map(|step| Duration::from_millis(step * 1_000 / 7))
            .collect();
        for span in each(index.numbers()).filter_map(|number| index.span(number)) {
            for edge in [span.start, span.end] {
                times.extend([edge, edge + Duration::from_nanos(1)]);
                times.extend(edge.checked_sub(Duration::from_nanos(1)));
            }
        }
        times
    }

    /// Checks each time query of `index` against a scan of its spans.
    fn check_queries(index: &MediaIndex) {
        let spans: Vec<Range<Duration>> = each(index.numbers())
            .map(|number| index.span(number).unwrap())
            .collect();
        let first = |found: Option<usize>| SegmentNumber(found.unwrap_or(spans.len()) as u64);
        for time in times(index) {
            let ending_after = first(spans.iter().position(|span| span.end > time));
            assert_eq!(index.ending_after(time), Some(ending_after), "{time:?}");
            let starting_from = first(spans.iter().position(|span| span.start >= time));
            assert_eq!(index.starting_from(time), Some(starting_from), "{time:?}");
            let last = SegmentNumber(spans.len() as u64 - 1);
            assert_eq!(
                index.number_at(time),
                Some(ending_after.min(last)),
                "{time:?}"
            );
        }

        // Durations sum over any range.
        let len = spans.len() as u64;
        for start in 0..=len {
            for end in start..=len + 1 {
                let range = SegmentNumber(start)..SegmentNumber(end);
                let summed: u128 = each(range.clone())
                    .filter_map(|number| index.ticks(number))
                    .map(|(_, duration)| u128::from(duration))
                    .sum();
                assert_eq!(index.media_ticks(range), summed, "{start}..{end}");
            }
        }
    }

    #[test]
    fn a_fixed_duration_template_is_arithmetic() {
        let segments = templated("fixed");
        let index = &segments.index;
        assert!(matches!(**index, MediaIndex::Template { .. }));

        // Four-tick segments to the end of the period, the last cut short.
        assert_eq!(index.numbers(), SegmentNumber(0)..SegmentNumber(8));
        assert_eq!(index.ticks(SegmentNumber(0)), Some((0, 4)));
        assert_eq!(index.ticks(SegmentNumber(7)), Some((28, 2)));
        assert_eq!(index.ticks(SegmentNumber(8)), None);
        assert_eq!(
            index.location(SegmentNumber(7)),
            Some(SegmentLocation::new("https://cdn.example/8.m4s"))
        );
        assert_eq!(index.location(SegmentNumber(8)), None);
        check_queries(index);
    }

    #[test]
    fn a_timeline_template_keeps_its_gaps() {
        let segments = templated("timeline");
        let index = &segments.index;
        assert!(matches!(**index, MediaIndex::Timeline { .. }));

        // Three two-tick segments, a gap, then four-tick segments to the period end.
        let ticks: Vec<_> = each(index.numbers())
            .filter_map(|number| index.ticks(number))
            .collect();
        assert_eq!(
            ticks,
            [
                (0, 2),
                (2, 2),
                (4, 2),
                (9, 4),
                (13, 4),
                (17, 4),
                (21, 4),
                (25, 4),
                (29, 1)
            ]
        );
        assert_eq!(
            index.location(SegmentNumber(3)),
            Some(SegmentLocation::new("https://cdn.example/9.m4s"))
        );

        // A time in the gap lands in the segment after it.
        let gap = Duration::from_millis(2_500);
        assert_eq!(index.number_at(gap), Some(SegmentNumber(3)));
        check_queries(index);
    }

    #[test]
    fn templated_indexes_answer_as_their_lists_do() {
        for id in ["fixed", "timeline"] {
            let segments = templated(id);
            let (templated, listed) = (&segments.index, &listed(&segments).index);
            assert_eq!(templated.numbers(), listed.numbers(), "{id}");
            assert_eq!(templated.timing(), listed.timing(), "{id}");
            for number in each(SegmentNumber(0)..templated.numbers().end.next()) {
                assert_eq!(
                    templated.segment(number),
                    listed.segment(number),
                    "{id} {number}"
                );
            }
            for time in times(templated) {
                assert_eq!(
                    templated.number_at(time),
                    listed.number_at(time),
                    "{id} {time:?}"
                );
            }
            let all = templated.numbers();
            assert_eq!(
                templated.media_ticks(all.clone()),
                listed.media_ticks(all),
                "{id}"
            );
        }
    }

    #[test]
    fn untimed_indexes_have_no_times() {
        let index = MediaIndex::Untimed(vec![SegmentLocation::new("a")]);
        assert_eq!(index.numbers(), SegmentNumber(0)..SegmentNumber(1));
        assert_eq!(index.span(SegmentNumber(0)), None);
        assert_eq!(index.ending_after(Duration::ZERO), None);
        assert_eq!(index.number_at(Duration::ZERO), None);
        assert_eq!(index.media_ticks(SegmentNumber(0)..SegmentNumber(1)), 0);
        assert_eq!(
            index.segment(SegmentNumber(0)),
            Some(IndexedSegment {
                location: SegmentLocation::new("a"),
                span: None
            })
        );
    }
}
