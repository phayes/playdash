//! MPEG-DASH `SegmentTemplate` expansion.
//!
//! Fills `$RepresentationID$`, `$Number$`, `$Time$` and `$Bandwidth$` URL
//! identifiers (with optional `%0[width]d` padding and `$$` escapes), and lists
//! media segments with the timing in [`super::segments`].

use std::time::Duration;

use crate::vendor::dash_mpd_core::SegmentTemplate;

use super::segments::{Addressing, Segments, Timing, initialization_fragment};
use crate::error::Error;
use crate::fragment::{Fragment, Fragments};

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
/// Identifiers match regardless of case. `$$` becomes a literal `$`. A `$` that
/// does not start a known identifier is kept as-is, matching common players.
/// Identifiers without a value (such as
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
    // Identifiers are case-sensitive in DASH, but some manifests write `$number$`.
    let value = match name.to_ascii_lowercase().as_str() {
        "representationid" => Some(values.representation_id.to_owned()),
        "number" => values.number.map(|number| number.to_string()),
        "time" => values.time.map(|time| time.to_string()),
        "bandwidth" => values.bandwidth.map(|bandwidth| bandwidth.to_string()),
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

    /// Initialization and media segment fragments, with unresolved URLs.
    ///
    /// The initialization comes from `@initialization`, or else an inherited
    /// `Initialization` element.
    pub fn fragments(
        self,
        values: TemplateValues,
        segments: &Segments,
    ) -> Result<Fragments, Error> {
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

        let media = segments
            .segments
            .iter()
            .map(|segment| {
                let values = TemplateValues {
                    number: Some(segment.number),
                    time: Some(segment.time),
                    ..values
                };
                Ok(Fragment::new(fill(media, &values)?))
            })
            .collect::<Result<_, Error>>()?;
        Ok(Fragments {
            init: initialization,
            media,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::dash_mpd_core::{S, SegmentTimeline};

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
    fn fills_identifiers_in_any_case() {
        assert_eq!(
            fill(
                "$representationid$/$BANDWIDTH$/$time$-$number%03d$.m4s",
                &values()
            )
            .unwrap(),
            "FLAC,44100,16/1411200/176128-042.m4s"
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
            [(1, 10_000, 1000), (2, 11_000, 1000), (3, 12_000, 500)]
        );
    }

    #[test]
    fn timeline_is_clipped_to_period_end() {
        let mut template = template(Some(vec![entry(Some(10_000), 1000, Some(49))]));
        template.presentationTimeOffset = Some(10_000);
        let segments = TemplateChain::new([Some(&template), None, None])
            .segments(Some(Duration::from_millis(2500)))
            .unwrap();
        assert_eq!(
            summarize(&segments),
            [(1, 10_000, 1000), (2, 11_000, 1000), (3, 12_000, 500)]
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
