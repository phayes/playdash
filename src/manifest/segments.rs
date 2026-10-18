//! MPEG-DASH segment timing shared by `SegmentTemplate` and `SegmentList`
//! addressing: media segments from either a `SegmentTimeline` or a fixed
//! `@duration`, and `Initialization` elements as segment locations.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::vendor::dash_mpd_core::{Initialization, S, SegmentTimeline};

use crate::error::Error;
use crate::segment::{SegmentLocation, check_segment, parse_byte_range};

/// One media segment's number and timing, generated from a template or list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentTime {
    /// Value of `$Number$`.
    pub number: u64,
    /// Value of `$Time$`: media time in timescale units, including `@presentationTimeOffset`.
    pub time: u64,
    /// Duration in timescale units.
    pub duration: u64,
}

/// The timing of a representation's media segments, worked out per segment
/// when asked for.
#[derive(Debug)]
pub(crate) struct Addressed {
    pub timescale: NonZeroU32,
    /// `@presentationTimeOffset`: media time at the start of the Period.
    pub presentation_time_offset: u64,
    pub segments: SegmentClock,
}

/// Segment timing: an explicit timeline, or a fixed segment duration.
pub(crate) enum Addressing<'a> {
    Timeline(&'a SegmentTimeline),
    Duration(f64),
}

/// Segment timing attributes shared by `SegmentTemplate` and `SegmentList`,
/// already resolved through DASH inheritance.
pub(crate) struct Timing<'a> {
    pub timescale: Option<u64>,
    pub start_number: Option<u64>,
    pub end_number: Option<u64>,
    pub presentation_time_offset: Option<u64>,
    pub addressing: Option<Addressing<'a>>,
    /// Whether an otherwise unbounded segment sequence may continue live.
    pub live: bool,
}

impl Timing<'_> {
    /// The media segments' timing.
    ///
    /// `period_duration` bounds open-ended `S@r="-1"` repeats and `@duration`
    /// addressing; it may be omitted when `end_number` provides the bound
    /// instead. Without either, a live manifest's segments go on without end,
    /// and a static one fails with [`Error::DashManifestMissingDuration`].
    pub fn addressed(self, period_duration: Option<Duration>) -> Result<Addressed, Error> {
        let timescale = u32::try_from(self.timescale.unwrap_or(1))
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(Error::DashManifestMissingTimescale)?;
        let bounds = Bounds {
            start_number: self.start_number.unwrap_or(1),
            end_number: self.end_number,
            offset: self.presentation_time_offset.unwrap_or(0),
            period_ticks: period_duration.map(|duration| to_ticks(duration, timescale.get())),
            live: self.live,
        };

        let segments = match self.addressing.ok_or(Error::DashManifestMissingTimeline)? {
            Addressing::Timeline(timeline) => {
                SegmentClock::Timeline(TimelineSegments::new(&bounds, &timeline.segments)?)
            }
            Addressing::Duration(duration) => {
                SegmentClock::Fixed(FixedSegments::new(&bounds, duration)?)
            }
        };
        Ok(Addressed {
            timescale,
            presentation_time_offset: bounds.offset,
            segments,
        })
    }
}

/// An `Initialization` element as a segment location; a missing `@sourceURL` means the
/// BaseURL itself.
pub(crate) fn initialization_location(
    initialization: &Initialization,
) -> Result<SegmentLocation, Error> {
    Ok(SegmentLocation {
        url: initialization.sourceURL.clone().unwrap_or_default(),
        range: initialization
            .range
            .as_deref()
            .map(parse_byte_range)
            .transpose()?,
    })
}

/// Inherited attributes that bound segment generation.
struct Bounds {
    start_number: u64,
    end_number: Option<u64>,
    /// `@presentationTimeOffset`: media time at the start of the Period.
    offset: u64,
    /// Period duration in timescale units.
    period_ticks: Option<u64>,
    /// Whether segments may go on without end when nothing else bounds them.
    live: bool,
}

impl Bounds {
    /// Media time at the end of the Period, if known.
    fn period_end(&self) -> Option<u64> {
        self.period_ticks
            .map(|ticks| self.offset.saturating_add(ticks))
    }
}

/// A representation's media segments, from a timeline or a fixed duration,
/// indexed from zero in timeline order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SegmentClock {
    Fixed(FixedSegments),
    Timeline(TimelineSegments),
}

impl SegmentClock {
    /// Returns the segment count, or `None` for an open-ended sequence.
    pub(crate) fn len(&self) -> Option<u64> {
        match self {
            SegmentClock::Fixed(segments) => segments.len(),
            SegmentClock::Timeline(segments) => segments.len(),
        }
    }

    /// The media start and duration, in ticks, of segment `index`. `None`
    /// past the last.
    pub(crate) fn ticks(&self, index: u64) -> Option<(u64, u64)> {
        match self {
            SegmentClock::Fixed(segments) => segments.ticks(index),
            SegmentClock::Timeline(segments) => segments.ticks(index),
        }
    }

    /// The `$Number$` of segment `index`. `None` past the last.
    pub(crate) fn number(&self, index: u64) -> Option<u64> {
        match self {
            SegmentClock::Fixed(segments) => segments.number(index),
            SegmentClock::Timeline(segments) => segments.number(index),
        }
    }

    /// Validates that segments have positive duration and do not overlap.
    ///
    /// Fails with [`Error::InvalidSegmentTiming`], naming the first segment
    /// out of order by its position and the location `location` gives its
    /// `$Number$` and `$Time$`.
    pub(crate) fn check_order(
        &self,
        location: impl Fn(u64, u64) -> SegmentLocation,
    ) -> Result<(), Error> {
        match self {
            SegmentClock::Fixed(segments) => segments.check_order(location),
            SegmentClock::Timeline(segments) => segments.check_order(location),
        }
    }

    /// Iterates over segment number, start time, and duration in timeline order.
    pub(crate) fn times(&self) -> impl Iterator<Item = SegmentTime> + '_ {
        (0..self.len().unwrap_or(u64::MAX)).map_while(|index| {
            let (time, duration) = self.ticks(index)?;
            Some(SegmentTime {
                number: self.number(index)?,
                time,
                duration,
            })
        })
    }
}

/// A segment duration in timescale ticks, possibly fractional: finite and
/// positive.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SegmentDuration(f64);

// Never NaN, so equality is reflexive.
impl Eq for SegmentDuration {}

impl SegmentDuration {
    fn new(duration: f64) -> Result<Self, Error> {
        match duration.is_finite() && duration > 0.0 {
            true => Ok(Self(duration)),
            false => Err(Error::DashManifestInvalidSegments(format!(
                "segment @duration {duration} is not positive"
            ))),
        }
    }

    /// Ticks from the first segment's start to the start of segment `index`.
    /// Each boundary is rounded on its own, so fractional durations do not
    /// accumulate error.
    fn boundary(self, index: u64) -> u64 {
        (index as f64 * self.0).round() as u64
    }
}

/// Media segments of a fixed `@duration`, the last cut short at the end of the
/// Period.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FixedSegments {
    start_number: u64,
    duration: SegmentDuration,
    /// `@presentationTimeOffset`: media time at the start of the Period.
    offset: u64,
    /// Period duration in timescale units.
    period_ticks: Option<u64>,
    /// `None` when the segments go on without end.
    len: Option<u64>,
}

impl FixedSegments {
    /// Segments of `duration` ticks, bounded by the end of the Period or
    /// `@endNumber`, whichever comes first, or without end when live and
    /// neither is known.
    fn new(bounds: &Bounds, duration: f64) -> Result<Self, Error> {
        let duration = SegmentDuration::new(duration)?;

        // As many as start before the end of the Period, up to @endNumber.
        let in_period = bounds.period_ticks.map(|ticks| {
            let after = |index| duration.boundary(index) >= ticks;
            let beyond = ((ticks as f64 / duration.0).ceil() as u64).saturating_add(1);
            first_where(0..beyond, after)
        });
        let to_end_number = bounds
            .end_number
            .map(|end| end.saturating_add(1).saturating_sub(bounds.start_number));
        let len = match (in_period, to_end_number) {
            (Some(in_period), Some(to_end)) => Some(in_period.min(to_end)),
            (Some(len), None) | (None, Some(len)) => Some(len),
            (None, None) if bounds.live => None,
            (None, None) => return Err(Error::DashManifestMissingDuration),
        };

        // In an open-ended sequence, overflow is checked lazily for each requested segment.
        let fits = len.is_none_or(|len| {
            bounds.start_number.checked_add(len).is_some()
                && bounds.offset.checked_add(duration.boundary(len)).is_some()
        });
        if !fits {
            return Err(Error::DashManifestInvalidSegments(
                "segment numbers or times overflow".to_owned(),
            ));
        }
        Ok(Self {
            start_number: bounds.start_number,
            duration,
            offset: bounds.offset,
            period_ticks: bounds.period_ticks,
            len,
        })
    }

    /// The number of media segments, or `None` when they go on without end.
    pub(crate) fn len(&self) -> Option<u64> {
        self.len
    }

    /// Each segment's duration in ticks, possibly fractional. The last one
    /// may be cut short.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn duration(&self) -> f64 {
        self.duration.0
    }

    /// Returns whether overlapping segments in `other` have identical timing.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn same_times(&self, other: &Self) -> bool {
        self.duration == other.duration && self.offset == other.offset
    }

    /// The `$Number$` of the first segment, `@startNumber`.
    pub(crate) fn start_number(&self) -> u64 {
        self.start_number
    }

    /// Returns whether `index` is at or beyond the finite end.
    fn past_end(&self, index: u64) -> bool {
        self.len.is_some_and(|len| index >= len)
    }

    /// The media start and duration, in ticks, of segment `index`. `None`
    /// past the last, or where its time no longer fits.
    pub(crate) fn ticks(&self, index: u64) -> Option<(u64, u64)> {
        if self.past_end(index) {
            return None;
        }
        let start = self.duration.boundary(index);
        let end = self.duration.boundary(index.checked_add(1)?);
        let end = self.period_ticks.map_or(end, |ticks| end.min(ticks));
        Some((self.offset.checked_add(start)?, end - start))
    }

    /// The `$Number$` of segment `index`. `None` past the last, or where it
    /// no longer fits.
    pub(crate) fn number(&self, index: u64) -> Option<u64> {
        match self.past_end(index) {
            true => None,
            false => self.start_number.checked_add(index),
        }
    }

    /// Checks that every segment lasts at least a tick; see
    /// [`SegmentClock::check_order`]. A `@duration` shorter than a tick
    /// rounds some segments to nothing.
    fn check_order(&self, location: impl Fn(u64, u64) -> SegmentLocation) -> Result<(), Error> {
        if self.duration.0 >= 1.0 {
            return Ok(());
        }

        // A few segments in, one rounds to nothing.
        let empty = (0..self.len.unwrap_or(u64::MAX)).find_map(|index| {
            let (start, duration) = self.ticks(index)?;
            (duration == 0).then_some((index, start))
        });
        match empty {
            Some((index, start)) => {
                let number = self.start_number + index;
                check_segment(index, || location(number, start), start, 0, &mut 0)
            }
            None => Ok(()),
        }
    }

    /// The summed durations, in ticks, of the segments indexed in `range`.
    pub(crate) fn media_ticks(&self, range: std::ops::Range<u64>) -> u128 {
        let end = self.len.map_or(range.end, |len| range.end.min(len));
        if range.start >= end {
            return 0;
        }

        // The segments are back to back.
        let (first, _) = self.ticks(range.start).unwrap_or_default();
        let (last, duration) = self.ticks(end - 1).unwrap_or_default();
        (u128::from(last) + u128::from(duration)).saturating_sub(u128::from(first))
    }
}

/// Media segments of a `SegmentTimeline`, as runs of equal durations.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TimelineSegments {
    /// Non-empty timeline runs in order; the final run is endless when `len` is `None`.
    runs: Vec<Run>,
    len: Option<u64>,
}

/// Consecutive media segments of one duration: an `S` entry and its repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Index of the run's first segment among all segments.
    position: u64,
    /// Media time at which the first segment starts.
    start: u64,
    /// Each segment's duration in ticks.
    duration: u64,
    count: u64,
    /// `$Number$` of the first segment.
    number: u64,
}

impl Run {
    /// Media time at which segment `offset` of the run starts, or `None`
    /// where it no longer fits.
    fn start_of(&self, offset: u64) -> Option<u64> {
        self.start.checked_add(offset.checked_mul(self.duration)?)
    }
}

impl TimelineSegments {
    /// The segments of timeline `entries`. Repeats stop at the end of the
    /// Period, which cuts the last segment short, and at `@endNumber`. When
    /// live and neither is known, a last entry with a negative `@r` repeats
    /// without end.
    fn new(bounds: &Bounds, entries: &[S]) -> Result<Self, Error> {
        if entries.is_empty() {
            return Err(Error::DashManifestMissingTimeline);
        }

        // One run per entry.
        let mut runs = Vec::with_capacity(entries.len());
        let mut number = bounds.start_number;
        let mut time = 0u64;
        let mut endless = false;
        for (index, entry) in entries.iter().enumerate() {
            if entry.d == 0 {
                return Err(Error::DashManifestInvalidSegments(
                    "SegmentTimeline entry has a zero duration".to_owned(),
                ));
            }
            number = entry.n.unwrap_or(number);
            time = entry.t.unwrap_or(time);

            let count = match entry.r {
                None => 1,
                Some(repeat) if repeat >= 0 => repeat as u64 + 1,
                // Negative @r repeats until the next S@t, the end of the Period, or @endNumber.
                Some(_) => {
                    let end = entries
                        .get(index + 1)
                        .and_then(|next| next.t)
                        .or(bounds.period_end());
                    match (end, bounds.end_number) {
                        (Some(end), _) => end.saturating_sub(time).div_ceil(entry.d),
                        (None, Some(end_number)) => {
                            end_number.saturating_add(1).saturating_sub(number)
                        }
                        // In live media, the final negative repeat continues as segments appear.
                        (None, None) if bounds.live && index + 1 == entries.len() => {
                            endless = true;
                            1
                        }
                        (None, None) => return Err(Error::DashManifestMissingDuration),
                    }
                }
            };
            runs.push(Run {
                position: 0,
                start: time,
                duration: entry.d,
                count,
                number,
            });
            number = number.saturating_add(count);
            time = time.saturating_add(count.saturating_mul(entry.d));
        }

        // Drop segments from the end of the Period on, and cut the last one short there.
        if let Some(end) = bounds.period_end() {
            for run in &mut runs {
                run.count = match run.start < end {
                    true => run.count.min((end - run.start).div_ceil(run.duration)),
                    false => 0,
                };
            }
            if let Some(last) = runs.iter().rposition(|run| run.count > 0) {
                let run = runs[last];
                // This segment starts within the Period; trim only a segment crossing its end.
                let start = run.start + (run.count - 1) * run.duration;
                if start + run.duration > end {
                    runs[last].count -= 1;
                    let cut = Run {
                        start,
                        duration: end - start,
                        count: 1,
                        number: run.number + run.count - 1,
                        ..run
                    };
                    runs.insert(last + 1, cut);
                }
            }
        }

        // Drop segments numbered past @endNumber.
        if let Some(end_number) = bounds.end_number {
            for run in &mut runs {
                run.count = match run.number <= end_number {
                    true => run.count.min(end_number - run.number + 1),
                    false => 0,
                };
            }
        }

        // Number the runs' segments from zero, checking that every number and time fits.
        runs.retain(|run| run.count > 0);
        let mut len = 0u64;
        for run in &mut runs {
            let fits = run
                .count
                .checked_mul(run.duration)
                .and_then(|ticks| run.start.checked_add(ticks))
                .and(run.number.checked_add(run.count))
                .and(len.checked_add(run.count));
            let Some(next) = fits else {
                return Err(Error::DashManifestInvalidSegments(
                    "segment numbers or times overflow".to_owned(),
                ));
            };
            run.position = len;
            len = next;
        }

        // Represent the endless final run without allowing its position range to overflow.
        if endless && let Some(last) = runs.last_mut() {
            last.count = u64::MAX - last.position;
            return Ok(Self { runs, len: None });
        }
        Ok(Self {
            runs,
            len: Some(len),
        })
    }

    /// Returns the segment count, or `None` when the final run repeats indefinitely.
    pub(crate) fn len(&self) -> Option<u64> {
        self.len
    }

    /// The longest segment duration, in ticks.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn max_duration(&self) -> u64 {
        self.runs.iter().map(|run| run.duration).max().unwrap_or(0)
    }

    /// Checks that each run starts no earlier than the one before it ends;
    /// see [`SegmentClock::check_order`]. Every run's segments have a
    /// duration and follow one another by construction.
    fn check_order(&self, location: impl Fn(u64, u64) -> SegmentLocation) -> Result<(), Error> {
        let mut end = 0;
        for (slot, run) in self.runs.iter().enumerate() {
            // The first segment proves the ordering of a uniform endless run.
            let endless = self.len.is_none() && slot + 1 == self.runs.len();
            let length = match endless {
                true => run.duration,
                false => run.count * run.duration,
            };
            let start = || location(run.number, run.start);
            check_segment(run.position, start, run.start, length, &mut end)?;
        }
        Ok(())
    }

    /// The run holding segment `index`, and the segment's offset in it.
    fn run(&self, index: u64) -> Option<(&Run, u64)> {
        let slot = self
            .runs
            .partition_point(|run| run.position + run.count <= index);
        let run = self.runs.get(slot)?;
        Some((run, index - run.position))
    }

    /// The media start and duration, in ticks, of segment `index`. `None`
    /// past the last, or where its time no longer fits.
    pub(crate) fn ticks(&self, index: u64) -> Option<(u64, u64)> {
        let (run, offset) = self.run(index)?;
        let start = run.start_of(offset)?;
        start.checked_add(run.duration)?;
        Some((start, run.duration))
    }

    /// The `$Number$` of segment `index`. `None` past the last, or where it
    /// no longer fits.
    pub(crate) fn number(&self, index: u64) -> Option<u64> {
        let (run, offset) = self.run(index)?;
        run.number.checked_add(offset)
    }

    /// The summed durations, in ticks, of the segments indexed in `range`.
    pub(crate) fn media_ticks(&self, range: std::ops::Range<u64>) -> u128 {
        self.runs
            .iter()
            .map(|run| {
                let start = range.start.max(run.position);
                let end = range.end.min(run.position + run.count);
                u128::from(end.saturating_sub(start)) * u128::from(run.duration)
            })
            .sum()
    }
}

/// The first value in `range` for which `after` holds, where `after` holds
/// for every value from some point on, or the end of `range` if none.
fn first_where(range: std::ops::Range<u64>, after: impl Fn(u64) -> bool) -> u64 {
    let (mut low, mut high) = (range.start, range.end);
    while low < high {
        let middle = low + (high - low) / 2;
        match after(middle) {
            true => high = middle,
            false => low = middle + 1,
        }
    }
    low
}

fn to_ticks(duration: Duration, timescale: u32) -> u64 {
    let ticks = (duration.as_nanos() * u128::from(timescale) + 500_000_000) / 1_000_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(t: Option<u64>, d: u64, r: Option<i64>, n: Option<u64>) -> S {
        S {
            t,
            d,
            r,
            n,
            ..Default::default()
        }
    }

    fn bounds(start_number: u64, end_number: Option<u64>, period_ticks: Option<u64>) -> Bounds {
        Bounds {
            start_number,
            end_number,
            offset: 0,
            period_ticks,
            live: false,
        }
    }

    /// Each segment's number, time and duration.
    fn summarize(clock: &SegmentClock) -> Vec<(u64, u64, u64)> {
        clock
            .times()
            .map(|segment| (segment.number, segment.time, segment.duration))
            .collect()
    }

    fn timeline(bounds: &Bounds, entries: &[S]) -> SegmentClock {
        SegmentClock::Timeline(TimelineSegments::new(bounds, entries).unwrap())
    }

    #[test]
    fn sub_tick_durations_can_round_to_empty_segments() {
        let fixed = FixedSegments::new(&bounds(1, None, Some(3)), 0.75).unwrap();
        assert_eq!(
            summarize(&SegmentClock::Fixed(fixed)),
            [(1, 0, 1), (2, 1, 1), (3, 2, 0), (4, 2, 1)]
        );
    }

    #[test]
    fn timeline_numbers_follow_explicit_starts() {
        let entries = [
            entry(Some(0), 1_000, Some(1), Some(10)),
            entry(None, 1_000, Some(1), Some(4)),
        ];
        assert_eq!(
            summarize(&timeline(&bounds(1, None, None), &entries)),
            [
                (10, 0, 1_000),
                (11, 1_000, 1_000),
                (4, 2_000, 1_000),
                (5, 3_000, 1_000)
            ]
        );
    }

    #[test]
    fn timeline_clips_mid_run_before_the_end_number() {
        let entries = [entry(Some(0), 1_000, Some(49), None)];
        assert_eq!(
            summarize(&timeline(&bounds(1, None, Some(2_500)), &entries)),
            [(1, 0, 1_000), (2, 1_000, 1_000), (3, 2_000, 500)]
        );
        assert_eq!(
            summarize(&timeline(&bounds(1, Some(2), Some(2_500)), &entries)),
            [(1, 0, 1_000), (2, 1_000, 1_000)]
        );

        // A period ending on a boundary cuts nothing short.
        let entries = [
            entry(Some(0), 500, Some(7), None),
            entry(None, 1_000, Some(20), None),
        ];
        let clock = timeline(&bounds(1, None, Some(5_000)), &entries);
        assert_eq!(clock.len(), Some(9));
        assert_eq!(clock.ticks(8), Some((4_000, 1_000)));
    }

    #[test]
    fn media_ticks_sum_spans_and_skip_gaps() {
        let entries = [
            entry(Some(0), 1_000, Some(1), None),
            entry(Some(5_000), 500, Some(3), None),
        ];
        let segments = TimelineSegments::new(&bounds(1, None, None), &entries).unwrap();
        assert_eq!(segments.media_ticks(0..6), 4_000);
        assert_eq!(segments.media_ticks(1..3), 1_500);
        assert_eq!(segments.media_ticks(5..9), 500);
        assert_eq!(segments.media_ticks(3..3), 0);

        let fixed = FixedSegments::new(&bounds(1, None, Some(10_500)), 4_000.0).unwrap();
        assert_eq!(fixed.media_ticks(0..3), 10_500);
        assert_eq!(fixed.media_ticks(1..2), 4_000);
        assert_eq!(fixed.media_ticks(2..9), 2_500);
    }

    #[test]
    fn overflowing_times_are_rejected() {
        let entries = [entry(Some(u64::MAX - 1_000), 1_000, Some(1), None)];
        assert!(matches!(
            TimelineSegments::new(&bounds(1, None, None), &entries),
            Err(Error::DashManifestInvalidSegments(_))
        ));
        let late = Bounds {
            offset: u64::MAX - 1,
            ..bounds(1, Some(10), None)
        };
        assert!(matches!(
            FixedSegments::new(&late, 1_000.0),
            Err(Error::DashManifestInvalidSegments(_))
        ));
    }
}
