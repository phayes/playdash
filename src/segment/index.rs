//! Indexes a representation's media segments by [`SegmentNumber`].
//!
//! An index resolves each segment's URL and, when the MPD provides timing,
//! its presentation-time span and live availability.

use super::{MediaSegment, MediaTiming, SegmentLocation, SegmentNumber};
#[cfg(any(feature = "sync", feature = "async"))]
use crate::error::Error;
use crate::manifest::{FixedSegments, MediaUrls, TimelineSegments};
use std::ops::Range;
use std::time::{Duration, SystemTime};

/// Padding applied to advertised media bytes when guessing total length.
/// AAC `bandwidth` is near the real rate; FLAC's is often uncompressed PCM.
const SYMPHONIA_COMPAT_MEDIA_FACTOR: u128 = 4;

/// Extra bytes reserved per media segment for `moof` / box overhead.
const SYMPHONIA_COMPAT_BYTES_PER_SEGMENT: u128 = 64 * 1024;

/// Maps live segment presentation times to their wall-clock availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveWindow {
    /// When presentation time zero became available:
    /// `MPD@availabilityStartTime`.
    pub(crate) availability_start: SystemTime,
    /// Length of the seekable window (`MPD@timeShiftBufferDepth`).
    /// `None` keeps all segments from the presentation start.
    pub(crate) time_shift_buffer_depth: Option<Duration>,
    /// How long before its end a segment may be fetched:
    /// `@availabilityTimeOffset`.
    pub(crate) availability_time_offset: Duration,
}

/// The address and optional timing of a representation's media segments.
/// Segment numbers follow playback order and normally start at zero.
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
        /// The number of the first segment.
        first: SegmentNumber,
        /// Where the segments sit on the wall clock, when live.
        live: Option<LiveWindow>,
    },
    /// Templated media segments of a `SegmentTimeline`.
    Timeline {
        timing: MediaTiming,
        segments: TimelineSegments,
        urls: MediaUrls,
        /// The number of the first segment.
        first: SegmentNumber,
        /// Where the segments sit on the wall clock, when live.
        live: Option<LiveWindow>,
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
    /// Returns the total segment count, or `None` for an open-ended index.
    fn len(&self) -> Option<u64> {
        match self {
            MediaIndex::Untimed(media) => Some(media.len() as u64),
            MediaIndex::List { segments, .. } => Some(segments.len() as u64),
            MediaIndex::Template { segments, .. } => segments.len(),
            MediaIndex::Timeline { segments, .. } => segments.len(),
        }
    }

    /// Returns the first segment number.
    pub(crate) fn first(&self) -> SegmentNumber {
        match self {
            MediaIndex::Untimed(_) | MediaIndex::List { .. } => SegmentNumber(0),
            MediaIndex::Template { first, .. } | MediaIndex::Timeline { first, .. } => *first,
        }
    }

    /// Returns one past the final segment number, or `None` if the index is open-ended.
    pub(crate) fn end(&self) -> Option<SegmentNumber> {
        self.len().map(|len| self.number(len))
    }

    /// Returns all segment numbers as a half-open range.
    ///
    /// # Panics
    ///
    /// Panics if the index is open-ended.
    #[cfg(test)]
    pub(crate) fn numbers(&self) -> Range<SegmentNumber> {
        self.first()..self.end().expect("an index with an end")
    }

    /// Returns wall-clock availability settings for a live index.
    pub(crate) fn live(&self) -> Option<&LiveWindow> {
        match self {
            MediaIndex::Template { live, .. } | MediaIndex::Timeline { live, .. } => live.as_ref(),
            MediaIndex::Untimed(_) | MediaIndex::List { .. } => None,
        }
    }

    /// Returns `true` if segment availability depends on wall-clock time.
    pub(crate) fn is_live(&self) -> bool {
        self.live().is_some()
    }

    /// Renumbers this index so its first segment is `first`.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn numbered_from(mut self, number: SegmentNumber) -> Self {
        if let MediaIndex::Template { first, .. } | MediaIndex::Timeline { first, .. } = &mut self {
            *first = number;
        }
        self
    }

    /// The position among the media segments of the one numbered `number`.
    /// `None` before the first.
    fn position(&self, number: SegmentNumber) -> Option<u64> {
        number.0.checked_sub(self.first().0)
    }

    /// The number of the media segment at `position`.
    fn number(&self, position: u64) -> SegmentNumber {
        SegmentNumber(self.first().0.saturating_add(position))
    }

    /// Returns the segment-number range that may be fetched at `now`.
    /// Static indexes always return their full range.
    pub(crate) fn available(&self, now: SystemTime) -> Range<SegmentNumber> {
        let first = self.first();
        let Some(live) = self.live() else {
            return first..self.end().unwrap_or(first);
        };
        let Ok(elapsed) = now.duration_since(live.availability_start) else {
            return first..first;
        };

        // Keep segments produced by `now` and still inside the time-shift window.
        let ready = elapsed.saturating_add(live.availability_time_offset);
        let end = self.ending_after(ready).unwrap_or(first);
        let start = match live.time_shift_buffer_depth {
            Some(depth) => self
                .ending_after(elapsed.saturating_sub(depth))
                .unwrap_or(first),
            None => first,
        };
        start.min(end)..end
    }

    /// Returns when segment `number` becomes fetchable.
    ///
    /// Returns `None` for static indexes and numbers outside the index.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn available_at(&self, number: SegmentNumber) -> Option<SystemTime> {
        let live = self.live()?;
        let end = self.span(number)?.end;
        let ready = end.saturating_sub(live.availability_time_offset);
        live.availability_start.checked_add(ready)
    }

    /// Returns the current live edge at `now`, or `None` if no segment is available.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn live_edge(&self, now: SystemTime) -> Option<Duration> {
        let available = self.available(now);
        let last = available
            .end
            .checked_prev()
            .filter(|&last| last >= available.start)?;
        Some(self.span(last)?.end)
    }

    /// Returns the segment at `delay` behind the live edge at `now`.
    ///
    /// The result is clamped to the available live window. If the window is
    /// empty, returns the number of the next segment expected to arrive.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn behind_edge(&self, now: SystemTime, delay: Duration) -> SegmentNumber {
        let available = self.available(now);
        let (Some(edge), Some(last)) = (self.live_edge(now), available.end.checked_prev()) else {
            return available.end;
        };
        self.number_at(edge.saturating_sub(delay))
            .unwrap_or(last)
            .clamp(available.start, last)
    }

    /// Clamps `time` to media available at `now`.
    ///
    /// Times before the live window move to its start. Times at or beyond the
    /// live edge move to `delay` behind the edge, no later than the start of
    /// the newest segment. A static index returns `time` unchanged. Returns
    /// `None` if a live index currently has no available segment.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn clamp_time(
        &self,
        time: Duration,
        now: SystemTime,
        delay: Duration,
    ) -> Option<Duration> {
        if !self.is_live() {
            return Some(time);
        }
        let available = self.available(now);
        let last = available
            .end
            .checked_prev()
            .filter(|&last| last >= available.start)?;
        let start = self.span(available.start)?.start;
        let newest = self.span(last)?;
        if time < start {
            return Some(start);
        }
        if time >= newest.end {
            let latest = newest.end.saturating_sub(delay).min(newest.start);
            return Some(latest.max(start));
        }
        Some(time)
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

    /// Returns the longest segment duration, or `None` for untimed or empty media.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn max_segment_duration(&self) -> Option<Duration> {
        let timing = self.timing()?;
        let ticks = match self {
            MediaIndex::Untimed(_) => return None,
            MediaIndex::List { segments, .. } => {
                segments.iter().map(|segment| segment.duration).max()?
            }
            MediaIndex::Template { segments, .. } => {
                let seconds = segments.duration() / f64::from(timing.timescale.get());
                return Duration::try_from_secs_f64(seconds).ok();
            }
            MediaIndex::Timeline { segments, .. } => segments.max_duration(),
        };
        Some(timing.duration(ticks))
    }

    /// The media start and duration, in ticks, of the segment numbered
    /// `number`. `None` when untimed or outside the segments.
    pub(crate) fn ticks(&self, number: SegmentNumber) -> Option<(u64, u64)> {
        let position = self.position(number)?;
        match self {
            MediaIndex::Untimed(_) => None,
            MediaIndex::List { segments, .. } => {
                let segment = segments.get(slot(position)?)?;
                Some((segment.start, segment.duration))
            }
            MediaIndex::Template { segments, .. } => segments.ticks(position),
            MediaIndex::Timeline { segments, .. } => segments.ticks(position),
        }
    }

    /// The presentation time the segment numbered `number` spans. `None` when
    /// untimed or outside the segments.
    pub(crate) fn span(&self, number: SegmentNumber) -> Option<Range<Duration>> {
        let timing = self.timing()?;
        let (start, duration) = self.ticks(number)?;
        let end = start.saturating_add(duration);
        Some(timing.presentation_time(start)..timing.presentation_time(end))
    }

    /// The location of the segment numbered `number`. `None` outside the
    /// segments.
    pub(crate) fn location(&self, number: SegmentNumber) -> Option<SegmentLocation> {
        let position = self.position(number)?;
        match self {
            MediaIndex::Untimed(media) => media.get(slot(position)?).cloned(),
            MediaIndex::List { segments, .. } => {
                Some(segments.get(slot(position)?)?.location.clone())
            }
            MediaIndex::Template { segments, urls, .. } => {
                let (time, _) = segments.ticks(position)?;
                Some(urls.location(segments.number(position)?, time))
            }
            MediaIndex::Timeline { segments, urls, .. } => {
                let (time, _) = segments.ticks(position)?;
                Some(urls.location(segments.number(position)?, time))
            }
        }
    }

    /// The segment numbered `number`. `None` outside the segments.
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

    /// Returns the segment containing presentation time `time`.
    ///
    /// Times before or after the media are clamped to the first or last
    /// segment. Returns `None` for untimed or empty media.
    pub(crate) fn number_at(&self, time: Duration) -> Option<SegmentNumber> {
        let found = self.ending_after(time)?;
        match self.end() {
            Some(end) => Some(found.min(end.checked_prev().filter(|&last| last >= self.first())?)),
            None => Some(found),
        }
    }

    /// The presentation time at which the last segment ends. `None` when
    /// untimed, without media, or without end.
    pub(crate) fn end_time(&self) -> Option<Duration> {
        let last = self.end()?.checked_prev()?;
        Some(self.span(last)?.end)
    }

    /// Returns a conservative byte-length estimate for the initialization
    /// segment, `init_len` bytes, and the media segments numbered from `first`
    /// on at `bitrate_bps`. `None` when untimed, without end, without media
    /// from `first`, or for a zero `bitrate_bps`.
    pub(crate) fn estimate_len(
        &self,
        first: SegmentNumber,
        init_len: u64,
        bitrate_bps: u32,
    ) -> Option<u64> {
        let timing = self.timing()?;
        let media = first..self.end()?;
        if bitrate_bps == 0 || media.is_empty() {
            return None;
        }

        // Allow for bitrate underestimates and per-segment container overhead.
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
        let position = |number| self.position(number).unwrap_or(0);
        let range = position(range.start)..position(range.end);
        match self {
            MediaIndex::Untimed(_) => 0,
            MediaIndex::List { segments, .. } => {
                let slot = |position: u64| {
                    slot(position).map_or(segments.len(), |slot| slot.min(segments.len()))
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
        let holds = |position: u64| {
            self.span(self.number(position))
                .is_some_and(|span| before(&span))
        };

        // For an open-ended index, exponentially widen the search before binary search.
        let (mut low, mut high) = match self.len() {
            Some(len) => (0, len),
            None => {
                let (mut low, mut high, mut step) = (0u64, 0u64, 1u64);
                while high < u64::MAX && holds(high) {
                    low = high + 1;
                    high = high.saturating_add(step);
                    step = step.saturating_mul(2);
                }
                (low, high)
            }
        };
        while low < high {
            let middle = low + (high - low) / 2;
            match holds(middle) {
                true => low = middle + 1,
                false => high = middle,
            }
        }
        Some(self.number(low))
    }

    /// Assigns segment numbers to this refreshed index that continue `old`.
    ///
    /// Template indexes retain their existing numbering. Timeline indexes are
    /// aligned by presentation time so overlapping segments keep stable
    /// numbers. A gap after the old timeline leaves a skipped number, allowing
    /// readers to recognize that the expected segment is no longer available.
    ///
    /// Fails with [`Error::DashManifestUnsupportedLive`] when the refresh
    /// changed the segments' clock or addressing, or would number a segment
    /// below zero.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn aligned_to(self, old: &MediaIndex) -> Result<Self, Error> {
        let unsupported = |reason: &str| Err(Error::DashManifestUnsupportedLive(reason.to_owned()));
        if self.timing() != old.timing() {
            return unsupported(
                "a refresh changed a representation's timescale, presentation time offset or period start",
            );
        }
        let first = match (&self, old) {
            (
                MediaIndex::Template { segments, .. },
                MediaIndex::Template {
                    segments: before, ..
                },
            ) => match segments.same_times(before) {
                true => old.first(),
                false => {
                    return unsupported("a refresh changed a representation's segment @duration");
                }
            },
            (MediaIndex::Timeline { .. }, MediaIndex::Timeline { .. }) => {
                self.timeline_first(old)?
            }
            _ => {
                return unsupported(
                    "a refresh changed how a representation's segments are addressed",
                );
            }
        };
        Ok(self.numbered_from(first))
    }

    /// The number this refreshed timeline's first segment takes after `old`;
    /// see [`Self::aligned_to`].
    #[cfg(any(feature = "sync", feature = "async"))]
    fn timeline_first(&self, old: &MediaIndex) -> Result<SegmentNumber, Error> {
        let (Some(start), Some(old_start)) = (
            self.span(self.first()).map(|span| span.start),
            old.span(old.first()).map(|span| span.start),
        ) else {
            return Ok(old.first());
        };

        // Number newly discovered earlier segments backward from the old first segment.
        if start < old_start {
            let ahead = self
                .starting_from(old_start)
                .and_then(|number| self.position(number))
                .unwrap_or(0);
            return old
                .first()
                .0
                .checked_sub(ahead)
                .map(SegmentNumber)
                .ok_or_else(|| {
                    Error::DashManifestUnsupportedLive(
                        "a refreshed timeline would number its first segments below zero"
                            .to_owned(),
                    )
                });
        }

        // Align with an overlapping segment, its successor, or one number past a gap.
        let at = old.ending_after(start).unwrap_or(old.first());
        if old.end() == Some(at)
            && old.end_time().is_some_and(|old_end| start > old_end)
        {
            return Ok(at.next());
        }
        Ok(at)
    }
}

/// Converts a `u64` position to an in-memory list index.
fn slot(position: u64) -> Option<usize> {
    usize::try_from(position).ok()
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

    /// The index of a live representation addressed by `template`, in a
    /// presentation available from the epoch with an eight-second time-shift
    /// window and a period starting at `period_start`.
    fn live(period_start: &str, template: &str) -> MediaIndex {
        let xml = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                    availabilityStartTime="1970-01-01T00:00:00Z" timeShiftBufferDepth="PT8S">
                 <Period start="{period_start}">
                   <AdaptationSet mimeType="audio/mp4">
                     <Representation id="live" bandwidth="1">{template}</Representation>
                   </AdaptationSet>
                 </Period>
               </MPD>"#
        );
        DashManifest::new(xml)
            .and_then(|manifest| manifest.with_base_url("https://cdn.example/"))
            .unwrap()
            .index_for_test("live")
    }

    /// A live timeline index of `entries`, numbered from `start_number`.
    fn live_timeline(start_number: u64, entries: &str) -> MediaIndex {
        live(
            "PT0S",
            &format!(
                r#"<SegmentTemplate initialization="i.mp4" media="$Time$.m4s" startNumber="{start_number}">
                     <SegmentTimeline>{entries}</SegmentTimeline>
                   </SegmentTemplate>"#
            ),
        )
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn n(number: u64) -> SegmentNumber {
        SegmentNumber(number)
    }

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// The numbers available at `now`, by a scan of the first `count`
    /// segments' spans: those ended by `now`, and not yet out of the
    /// eight-second window.
    fn scanned(index: &MediaIndex, now: u64, count: u64) -> Range<SegmentNumber> {
        let numbers = each(index.first()..index.first().plus(count as usize));
        let ready: Vec<SegmentNumber> = numbers
            .filter(|&number| {
                let span = index.span(number).unwrap();
                span.end <= secs(now) && span.end > secs(now.saturating_sub(8))
            })
            .collect();
        match (ready.first(), ready.last()) {
            (Some(&first), Some(&last)) => first..last.next(),
            _ => {
                let end = index.ending_after(secs(now)).unwrap();
                end..end
            }
        }
    }

    #[test]
    fn a_live_template_is_available_as_its_segments_end() {
        let index = live(
            "PT0S",
            r#"<SegmentTemplate initialization="i.mp4" media="$Number$.m4s" duration="2" startNumber="5"/>"#,
        );
        assert!(index.is_live());
        assert_eq!(index.first(), n(5), "numbered by $Number$");
        assert_eq!(index.end(), None);
        assert_eq!(index.end_time(), None);
        assert_eq!(
            index.location(n(7)),
            Some(SegmentLocation::new("https://cdn.example/7.m4s"))
        );

        // Nothing has ended at the start; then each segment as it ends.
        assert_eq!(index.available(at(0)), n(5)..n(5));
        assert_eq!(index.live_edge(at(0)), None);
        assert_eq!(index.available(at(5)), n(5)..n(7));
        assert_eq!(index.live_edge(at(5)), Some(secs(4)));
        assert_eq!(index.available_at(n(7)), Some(at(6)));

        // Segments leave once their end falls out of the eight-second window.
        assert_eq!(index.available(at(21)), n(11)..n(15));
        for now in [0, 1, 2, 3, 7, 8, 9, 10, 21, 1_000, 1_000_001] {
            assert_eq!(
                index.available(at(now)),
                scanned(&index, now, now / 2 + 2),
                "{now}"
            );
        }
        assert_eq!(index.max_segment_duration(), Some(secs(2)));

        // Before the presentation, nothing is.
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(index.available(before), n(5)..n(5));
    }

    #[test]
    fn availability_follows_the_offset_period_start_and_presentation_offset() {
        // Available a second before each segment ends.
        let early = live(
            "PT0S",
            r#"<SegmentTemplate initialization="i.mp4" media="$Number$.m4s" duration="2"
                                availabilityTimeOffset="1"/>"#,
        );
        assert_eq!(early.available(at(5)), n(1)..n(4));
        assert_eq!(early.available_at(n(3)), Some(at(5)));

        // A period from 100 s whose media time starts at 50.
        let late = live(
            "PT100S",
            r#"<SegmentTemplate initialization="i.mp4" media="$Time$.m4s" duration="2"
                                presentationTimeOffset="50"/>"#,
        );
        assert_eq!(late.span(n(1)), Some(secs(100)..secs(102)));
        assert_eq!(late.available(at(101)), n(1)..n(1));
        assert_eq!(late.available(at(103)), n(1)..n(2));
        assert_eq!(
            late.location(n(1)),
            Some(SegmentLocation::new("https://cdn.example/50.m4s"))
        );
    }

    #[test]
    fn a_live_timeline_repeats_its_last_entry_without_end() {
        let index = live_timeline(1, r#"<S t="10" d="2" r="2"/><S d="3" r="-1"/>"#);
        assert_eq!(index.end(), None);
        assert_eq!(index.first(), n(1));
        assert_eq!(index.span(n(4)), Some(secs(16)..secs(19)));
        assert_eq!(index.span(n(6)), Some(secs(22)..secs(25)));

        // Listed and repeated segments become available as they end.
        assert_eq!(index.available(at(17)), n(1)..n(4));
        assert_eq!(
            index.available(at(20)),
            n(2)..n(5),
            "10..12 s left the window"
        );
        assert_eq!(index.live_edge(at(20)), Some(secs(19)));
        for now in [9, 12, 20, 25, 99, 10_007] {
            assert_eq!(
                index.available(at(now)),
                scanned(&index, now, now / 2 + 2),
                "{now}"
            );
        }
        assert_eq!(index.max_segment_duration(), Some(secs(3)));
    }

    #[test]
    fn a_refreshed_timeline_keeps_the_numbers_it_shares() {
        let old = live_timeline(1, r#"<S t="0" d="2" r="4"/>"#);
        assert_eq!(old.numbers(), n(1)..n(6));
        let aligned = |entries: &str| live_timeline(1, entries).aligned_to(&old);

        // Starting in the old segments, or just after the last.
        let overlapping = aligned(r#"<S t="4" d="2" r="4"/>"#).unwrap();
        assert_eq!(overlapping.numbers(), n(3)..n(8));
        assert_eq!(overlapping.span(n(3)), old.span(n(3)));
        assert_eq!(aligned(r#"<S t="10" d="2"/>"#).unwrap().first(), n(6));
        assert_eq!(
            aligned(r#"<S t="5" d="2"/>"#).unwrap().first(),
            n(3),
            "within a segment"
        );

        // Starting earlier, numbered back from the old first segment.
        let later = live_timeline(10, r#"<S t="4" d="2" r="2"/>"#);
        let earlier = live_timeline(1, r#"<S t="0" d="2" r="4"/>"#).aligned_to(&later);
        assert_eq!(earlier.unwrap().numbers(), n(8)..n(13));
        assert!(matches!(
            live_timeline(1, r#"<S t="0" d="2" r="4"/>"#)
                .aligned_to(&live_timeline(1, r#"<S t="4" d="2"/>"#)),
            Err(Error::DashManifestUnsupportedLive(_))
        ));

        // A gap consumes one number so a waiting reader can detect missing media.
        let past_gap = aligned(r#"<S t="12" d="2"/>"#).unwrap();
        assert_eq!(past_gap.numbers(), n(7)..n(8));
        assert_eq!(past_gap.span(n(7)), Some(secs(12)..secs(14)));
    }

    #[test]
    fn a_refreshed_template_keeps_its_numbers() {
        let template = |attributes: &str| {
            live(
                "PT0S",
                &format!(
                    r#"<SegmentTemplate initialization="i.mp4" media="$Number$.m4s" {attributes}/>"#
                ),
            )
        };
        let old = template(r#"duration="2" startNumber="5""#);
        let renumbered = template(r#"duration="2" startNumber="9""#)
            .aligned_to(&old)
            .unwrap();
        assert_eq!(renumbered.first(), n(5));
        assert_eq!(renumbered.span(n(7)), old.span(n(7)));

        // A different clock or addressing cannot be aligned.
        for (name, changed) in [
            ("duration", template(r#"duration="3""#)),
            ("timescale", template(r#"duration="2" timescale="2""#)),
            ("timeline", live_timeline(5, r#"<S t="0" d="2" r="-1"/>"#)),
        ] {
            assert!(
                matches!(
                    changed.aligned_to(&old),
                    Err(Error::DashManifestUnsupportedLive(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn live_manifests_need_template_addressing() {
        let xml = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                          availabilityStartTime="1970-01-01T00:00:00Z">
                       <Period><AdaptationSet mimeType="audio/mp4">
                         <Representation id="list" bandwidth="1">
                           <SegmentList duration="2"><Initialization sourceURL="i.mp4"/>
                             <SegmentURL media="a.m4s"/></SegmentList>
                         </Representation>
                       </AdaptationSet></Period>
                     </MPD>"#;
        let manifest = DashManifest::new(xml).unwrap();
        assert!(manifest.is_live());
        assert!(matches!(
            manifest.segments("list"),
            Err(Error::DashManifestUnsupportedLive(_))
        ));
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
