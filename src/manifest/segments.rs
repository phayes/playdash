//! MPEG-DASH segment timing shared by `SegmentTemplate` and `SegmentList`
//! addressing: media segments from either a `SegmentTimeline` or a fixed
//! `@duration`, and `Initialization` elements as fragments.

use std::time::Duration;

use crate::vendor::dash_mpd_core::{Initialization, S, SegmentTimeline};

use crate::error::Error;
use crate::fragment::{Fragment, MediaTimeline, parse_byte_range};

/// One media segment generated from a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    /// Value of `$Number$`.
    pub number: u64,
    /// Value of `$Time$`: media time in timescale units, including `@presentationTimeOffset`.
    pub time: u64,
    /// Duration in timescale units.
    pub duration: u64,
}

/// Media segments of a representation, in timeline order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segments {
    pub timescale: u32,
    pub segments: Vec<Segment>,
}

impl Segments {
    pub fn media_timeline(&self) -> MediaTimeline {
        MediaTimeline {
            timescale: self.timescale,
            origin: self.segments.first().map_or(0, |segment| segment.time),
            media_durations: self
                .segments
                .iter()
                .map(|segment| segment.duration)
                .collect(),
        }
    }
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
}

impl Timing<'_> {
    /// Lists media segments.
    ///
    /// `period_duration` bounds open-ended `S@r="-1"` repeats and `@duration`
    /// addressing; it may be omitted when `end_number` provides the bound instead.
    pub fn segments(self, period_duration: Option<Duration>) -> Result<Segments, Error> {
        let timescale = u32::try_from(self.timescale.unwrap_or(1))
            .ok()
            .filter(|&timescale| timescale > 0)
            .ok_or(Error::DashManifestMissingTimescale)?;
        let bounds = Bounds {
            start_number: self.start_number.unwrap_or(1),
            end_number: self.end_number,
            offset: self.presentation_time_offset.unwrap_or(0),
            period_ticks: period_duration.map(|duration| to_ticks(duration, timescale)),
        };

        let mut segments = match self.addressing.ok_or(Error::DashManifestMissingTimeline)? {
            Addressing::Timeline(timeline) => bounds.timeline_segments(&timeline.segments)?,
            Addressing::Duration(duration) => bounds.duration_segments(duration)?,
        };
        if let Some(end_number) = bounds.end_number {
            segments.retain(|segment| segment.number <= end_number);
        }

        Ok(Segments {
            timescale,
            segments,
        })
    }
}

/// An `Initialization` element as a fragment; a missing `@sourceURL` means the BaseURL itself.
pub(crate) fn initialization_fragment(initialization: &Initialization) -> Result<Fragment, Error> {
    Ok(Fragment {
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
}

impl Bounds {
    fn timeline_segments(&self, entries: &[S]) -> Result<Vec<Segment>, Error> {
        if entries.is_empty() {
            return Err(Error::DashManifestMissingTimeline);
        }

        let mut segments = Vec::new();
        let mut number = self.start_number;
        let mut time = 0;
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
                        .or(self.period_ticks.map(|ticks| self.offset + ticks));
                    match (end, self.end_number) {
                        (Some(end), _) => end.saturating_sub(time).div_ceil(entry.d),
                        (None, Some(end_number)) => (end_number + 1).saturating_sub(number),
                        (None, None) => return Err(Error::DashManifestMissingDuration),
                    }
                }
            };
            for _ in 0..count {
                segments.push(Segment {
                    number,
                    time,
                    duration: entry.d,
                });
                number += 1;
                time += entry.d;
            }
        }

        // Drop segments from the end of the Period on, and cut the last one short there.
        if let Some(ticks) = self.period_ticks {
            let end = self.offset + ticks;
            segments.retain(|segment| segment.time < end);
            if let Some(last) = segments.last_mut() {
                last.duration = last.duration.min(end - last.time);
            }
        }
        Ok(segments)
    }

    /// Segments of a fixed `duration`; the last one is cut short at the end of the Period.
    fn duration_segments(&self, duration: f64) -> Result<Vec<Segment>, Error> {
        if !(duration.is_finite() && duration > 0.0) {
            return Err(Error::DashManifestInvalidSegments(format!(
                "segment @duration {duration} is not positive"
            )));
        }
        if self.period_ticks.is_none() && self.end_number.is_none() {
            return Err(Error::DashManifestMissingDuration);
        }

        // @duration may be fractional, so round each boundary rather than accumulating.
        let boundary = |index: u64| (index as f64 * duration).round() as u64;
        let mut segments = Vec::new();
        for index in 0.. {
            let number = self.start_number + index;
            let start = boundary(index);
            let past_period = self.period_ticks.is_some_and(|ticks| start >= ticks);
            let past_end_number = self.end_number.is_some_and(|end| number > end);
            if past_period || past_end_number {
                break;
            }
            let end = match self.period_ticks {
                Some(ticks) => boundary(index + 1).min(ticks),
                None => boundary(index + 1),
            };
            segments.push(Segment {
                number,
                time: self.offset + start,
                duration: end - start,
            });
        }
        Ok(segments)
    }
}

fn to_ticks(duration: Duration, timescale: u32) -> u64 {
    let ticks = (duration.as_nanos() * u128::from(timescale) + 500_000_000) / 1_000_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}
