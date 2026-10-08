//! MPEG-DASH `SegmentTemplate` expansion.
//!
//! Fills `$RepresentationID$`, `$Number$`, `$Time$` and `$Bandwidth$` URL
//! identifiers (with optional `%0[width]d` padding and `$$` escapes), and lists
//! media segments from either a `SegmentTimeline` or a fixed `@duration`. The
//! segment timing here is shared with `SegmentList` addressing.

use std::time::Duration;

use dash_mpd_core::{Initialization, S, SegmentTemplate, SegmentTimeline};

use crate::error::Error;
use crate::stream::{Fragment, MediaTimeline};

/// Values substituted for the `$...$` identifiers of a URL template.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TemplateValues<'a> {
    pub representation_id: &'a str,
    pub bandwidth: Option<u64>,
    pub number: Option<u64>,
    pub time: Option<u64>,
}

/// Substitutes DASH identifiers in a URL template.
///
/// `$$` becomes a literal `$`. A `$` that does not start a known identifier is
/// kept as-is, matching common players. Identifiers without a value (such as
/// `$Number$` in an initialization template) are rejected.
pub(crate) fn fill(template: &str, values: &TemplateValues) -> Result<String, Error> {
    let mut url = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        url.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            rest = &rest[start..];
            break;
        };
        match identifier_value(template, &after[..end], values)? {
            Some(value) => {
                url.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                url.push('$');
                rest = after;
            }
        }
    }
    url.push_str(rest);
    Ok(url)
}

/// Value of a `$name$` or `$name%0[width]d$` token, or `None` if `token` is not an identifier.
fn identifier_value(
    template: &str,
    token: &str,
    values: &TemplateValues,
) -> Result<Option<String>, Error> {
    if token.is_empty() {
        return Ok(Some("$".to_owned()));
    }
    let (name, format) = match token.split_once('%') {
        Some((name, format)) => (name, Some(format)),
        None => (token, None),
    };
    let value = match name {
        "RepresentationID" => Some(values.representation_id.to_owned()),
        "Number" => values.number.map(|number| number.to_string()),
        "Time" => values.time.map(|time| time.to_string()),
        "Bandwidth" => values.bandwidth.map(|bandwidth| bandwidth.to_string()),
        _ => return Ok(None),
    }
    .ok_or_else(|| invalid_template(template, &format!("${name}$ has no value here")))?;

    let Some(format) = format else {
        return Ok(Some(value));
    };
    // DASH-IF IOP: "only %0[width]d is permitted".
    let width = format
        .strip_prefix('0')
        .and_then(|format| format.strip_suffix('d'))
        .and_then(|width| width.parse::<usize>().ok())
        .ok_or_else(|| invalid_template(template, &format!("unsupported format tag %{format}")))?;
    Ok(Some(format!("{value:0>width$}")))
}

fn invalid_template(template: &str, reason: &str) -> Error {
    Error::DashManifestInvalidSegments(format!("{template}: {reason}"))
}

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
            media_durations: self
                .segments
                .iter()
                .map(|segment| segment.duration)
                .collect(),
        }
    }
}

/// How segments are timed.
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
            .map(str::parse)
            .transpose()?,
    })
}

/// `SegmentTemplate` elements a representation inherits from, most specific first
/// (Representation, AdaptationSet, Period).
#[derive(Clone, Copy)]
pub(crate) struct TemplateChain<'a> {
    levels: [Option<&'a SegmentTemplate>; 3],
}

impl<'a> TemplateChain<'a> {
    pub fn new(levels: [Option<&'a SegmentTemplate>; 3]) -> Self {
        Self { levels }
    }

    fn templates(self) -> impl Iterator<Item = &'a SegmentTemplate> {
        self.levels.into_iter().flatten()
    }

    /// First value of a `SegmentTemplate` attribute, honoring DASH inheritance.
    pub fn get<T>(self, field: impl Fn(&'a SegmentTemplate) -> Option<T>) -> Option<T> {
        self.templates().find_map(field)
    }

    /// Initialization fragment followed by media segment fragments, with unresolved URLs.
    ///
    /// The initialization comes from `@initialization`, or else an inherited
    /// `Initialization` element.
    pub fn fragments(
        self,
        values: TemplateValues,
        segments: &Segments,
    ) -> Result<Vec<Fragment>, Error> {
        let initialization = match self.get(|template| template.initialization.as_deref()) {
            Some(initialization) => Fragment::new(fill(initialization, &values)?),
            None => initialization_fragment(
                self.get(|template| template.Initialization.as_ref())
                    .ok_or(Error::DashManifestMissingUrls)?,
            )?,
        };
        let media = self
            .get(|template| template.media.as_deref())
            .ok_or(Error::DashManifestMissingMediaTemplate)?;

        let mut fragments = Vec::with_capacity(segments.segments.len() + 1);
        fragments.push(initialization);
        for segment in &segments.segments {
            let values = TemplateValues {
                number: Some(segment.number),
                time: Some(segment.time),
                ..values
            };
            fragments.push(Fragment::new(fill(media, &values)?));
        }
        Ok(fragments)
    }

    /// Lists media segments; see [`Timing::segments`].
    pub fn segments(self, period_duration: Option<Duration>) -> Result<Segments, Error> {
        Timing {
            timescale: self.get(|template| template.timescale),
            start_number: self.get(|template| template.startNumber),
            end_number: self.get(|template| template.endNumber),
            presentation_time_offset: self.get(|template| template.presentationTimeOffset),
            // Timeline and @duration are exclusive; the most specific level that sets either wins.
            addressing: self.templates().find_map(|template| {
                template
                    .SegmentTimeline
                    .as_ref()
                    .map(Addressing::Timeline)
                    .or(template.duration.map(Addressing::Duration))
            }),
        }
        .segments(period_duration)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> TemplateValues<'static> {
        TemplateValues {
            representation_id: "FLAC,44100,16",
            bandwidth: Some(1_411_200),
            number: Some(42),
            time: Some(176_128),
        }
    }

    #[test]
    fn fills_identifiers() {
        assert_eq!(
            fill(
                "$RepresentationID$/$Bandwidth$/$Time$-$Number$.m4s",
                &values()
            )
            .unwrap(),
            "FLAC,44100,16/1411200/176128-42.m4s"
        );
    }

    #[test]
    fn fills_width_format_tags() {
        assert_eq!(
            fill("$Number%05d$/$Number%010d$/$Time%02d$", &values()).unwrap(),
            "00042/0000000042/176128"
        );
    }

    #[test]
    fn fills_dollar_escapes_and_keeps_unknown_dollars() {
        assert_eq!(
            fill("a$$b/$Unknown$Number$/c$", &values()).unwrap(),
            "a$b/$Unknown42/c$"
        );
    }

    #[test]
    fn rejects_identifier_without_value() {
        let values = TemplateValues {
            number: None,
            ..values()
        };
        assert!(matches!(
            fill("init-$Number$.mp4", &values),
            Err(Error::DashManifestInvalidSegments(message)) if message.contains("$Number$")
        ));
    }

    #[test]
    fn rejects_unsupported_format_tag() {
        assert!(matches!(
            fill("$Number%5x$.m4s", &values()),
            Err(Error::DashManifestInvalidSegments(message)) if message.contains("%5x")
        ));
    }

    fn template(timeline: Option<Vec<S>>) -> SegmentTemplate {
        SegmentTemplate {
            timescale: Some(1000),
            SegmentTimeline: timeline.map(|segments| SegmentTimeline { segments }),
            ..Default::default()
        }
    }

    fn entry(t: Option<u64>, d: u64, r: Option<i64>) -> S {
        S {
            t,
            d,
            r,
            ..Default::default()
        }
    }

    fn summarize(segments: &Segments) -> Vec<(u64, u64, u64)> {
        segments
            .segments
            .iter()
            .map(|segment| (segment.number, segment.time, segment.duration))
            .collect()
    }

    #[test]
    fn timeline_tracks_time_and_explicit_starts() {
        let template = template(Some(vec![
            entry(Some(500), 1000, Some(1)),
            entry(Some(3000), 400, None),
        ]));
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(None)
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [(1, 500, 1000), (2, 1500, 1000), (3, 3000, 400)]
        );
    }

    #[test]
    fn open_ended_repeat_runs_to_next_start() {
        let template = template(Some(vec![
            entry(Some(0), 1000, Some(-1)),
            entry(Some(2500), 500, None),
        ]));
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(None)
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [
                (1, 0, 1000),
                (2, 1000, 1000),
                (3, 2000, 1000),
                (4, 2500, 500)
            ]
        );
    }

    #[test]
    fn open_ended_repeat_runs_to_period_end() {
        let mut template = template(Some(vec![entry(None, 1000, Some(-1))]));
        template.presentationTimeOffset = Some(10_000);
        template.SegmentTimeline.as_mut().unwrap().segments[0].t = Some(10_000);
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(Some(Duration::from_millis(2500)))
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [(1, 10_000, 1000), (2, 11_000, 1000), (3, 12_000, 1000)]
        );
    }

    #[test]
    fn open_ended_repeat_needs_a_bound() {
        let template = template(Some(vec![entry(None, 1000, Some(-1))]));
        assert!(matches!(
            TemplateChain::new([Some(&template), None, None]).segments(None),
            Err(Error::DashManifestMissingDuration)
        ));
    }

    #[test]
    fn end_number_caps_timeline() {
        let mut template = template(Some(vec![entry(None, 1000, Some(-1))]));
        template.startNumber = Some(5);
        template.endNumber = Some(7);
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(None)
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [(5, 0, 1000), (6, 1000, 1000), (7, 2000, 1000)]
        );
    }

    #[test]
    fn duration_addressing_rounds_up_and_trims_last_segment() {
        let mut template = template(None);
        template.duration = Some(4000.0);
        template.startNumber = Some(0);
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(Some(Duration::from_millis(10_500)))
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [(0, 0, 4000), (1, 4000, 4000), (2, 8000, 2500)]
        );
    }

    #[test]
    fn duration_addressing_handles_fractional_durations() {
        let mut template = template(None);
        template.duration = Some(1001.5);
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(Some(Duration::from_millis(3004)))
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [(1, 0, 1002), (2, 1002, 1001), (3, 2003, 1001)]
        );
    }

    #[test]
    fn duration_addressing_uses_end_number_without_period_duration() {
        let mut template = template(None);
        template.duration = Some(2000.0);
        template.startNumber = Some(3);
        template.endNumber = Some(4);
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(None)
            .unwrap();
        assert_eq!(summarize(&segments), [(3, 0, 2000), (4, 2000, 2000)]);
    }

    #[test]
    fn duration_addressing_needs_a_bound() {
        let mut template = template(None);
        template.duration = Some(2000.0);
        assert!(matches!(
            TemplateChain::new([Some(&template), None, None]).segments(None),
            Err(Error::DashManifestMissingDuration)
        ));
    }

    #[test]
    fn most_specific_addressing_wins() {
        let mut representation = template(None);
        representation.duration = Some(500.0);
        representation.endNumber = Some(2);
        let adaptation = template(Some(vec![entry(None, 1000, Some(3))]));
        let segments = TemplateChain::new([Some(&representation), Some(&adaptation), None])
            .segments(None)
            .unwrap();
        assert_eq!(summarize(&segments), [(1, 0, 500), (2, 500, 500)]);
    }

    #[test]
    fn rejects_zero_timescale() {
        let mut template = template(Some(vec![entry(None, 1000, None)]));
        template.timescale = Some(0);
        assert!(matches!(
            TemplateChain::new([Some(&template), None, None]).segments(None),
            Err(Error::DashManifestMissingTimescale)
        ));
    }
}
