//! MPEG-DASH `SegmentTemplate` expansion.
//!
//! Fills `$RepresentationID$`, `$Number$`, `$Time$` and `$Bandwidth$` URL
//! identifiers (with optional `%0[width]d` padding and `$$` escapes), and lists
//! media segments with the timing in [`super::segments`].

use std::time::Duration;

use crate::vendor::dash_mpd_core::SegmentTemplate;

use super::resolve_url;
use super::segments::{Addressed, Addressing, Timing, initialization_location};
use crate::error::Error;
use crate::segment::SegmentLocation;
use url::Url;

/// Values substituted for the `$...$` identifiers of a URL template.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TemplateValues<'a> {
    pub representation_id: &'a str,
    pub bandwidth: Option<u64>,
    pub number: Option<u64>,
    pub time: Option<u64>,
}

/// A DASH URL identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identifier {
    RepresentationId,
    Bandwidth,
    Number,
    Time,
}

impl Identifier {
    /// This identifier's value among `values`.
    fn value(self, values: &TemplateValues) -> Option<String> {
        match self {
            Identifier::RepresentationId => Some(values.representation_id.to_owned()),
            Identifier::Bandwidth => values.bandwidth.map(|bandwidth| bandwidth.to_string()),
            Identifier::Number => values.number.map(|number| number.to_string()),
            Identifier::Time => values.time.map(|time| time.to_string()),
        }
    }
}

/// A piece of a URL template: literal text, or an identifier to substitute,
/// zero-padded to `width` digits when given.
enum Piece<'a> {
    Text(&'a str),
    Identifier {
        /// The identifier's name as the template writes it.
        name: &'a str,
        identifier: Identifier,
        width: Option<usize>,
    },
}

/// Splits a URL template into its pieces.
///
/// Identifiers match regardless of case. `$$` becomes a literal `$`. A `$` that
/// does not start a known identifier is kept as-is, matching common players.
fn pieces(template: &str) -> Result<Vec<Piece<'_>>, Error> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        pieces.push(Piece::Text(&rest[..start]));
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            rest = &rest[start..];
            break;
        };
        match piece(template, &after[..end])? {
            Some(piece) => {
                pieces.push(piece);
                rest = &after[end + 1..];
            }
            None => {
                pieces.push(Piece::Text("$"));
                rest = after;
            }
        }
    }
    pieces.push(Piece::Text(rest));
    Ok(pieces)
}

/// The piece a `$name$` or `$name%0[width]d$` token stands for, or `None` if
/// `token` is not an identifier.
fn piece<'a>(template: &str, token: &'a str) -> Result<Option<Piece<'a>>, Error> {
    if token.is_empty() {
        return Ok(Some(Piece::Text("$")));
    }
    let (name, format) = match token.split_once('%') {
        Some((name, format)) => (name, Some(format)),
        None => (token, None),
    };
    // Identifiers are case-sensitive in DASH, but some manifests write `$number$`.
    let identifier = match name.to_ascii_lowercase().as_str() {
        "representationid" => Identifier::RepresentationId,
        "bandwidth" => Identifier::Bandwidth,
        "number" => Identifier::Number,
        "time" => Identifier::Time,
        _ => return Ok(None),
    };

    // DASH-IF IOP: "only %0[width]d is permitted".
    let width = format
        .map(|format| {
            format
                .strip_prefix('0')
                .and_then(|format| format.strip_suffix('d'))
                .and_then(|width| width.parse::<usize>().ok())
                .ok_or_else(|| {
                    invalid_template(template, &format!("unsupported format tag %{format}"))
                })
        })
        .transpose()?;
    Ok(Some(Piece::Identifier {
        name,
        identifier,
        width,
    }))
}

/// `value` zero-padded to `width` digits, when given.
fn padded(value: &str, width: Option<usize>) -> String {
    match width {
        Some(width) => format!("{value:0>width$}"),
        None => value.to_owned(),
    }
}

/// Substitutes DASH identifiers in a URL template, as [`pieces`] reads them.
///
/// Identifiers without a value (such as `$Number$` in an initialization
/// template) are rejected.
pub(crate) fn fill(template: &str, values: &TemplateValues) -> Result<String, Error> {
    let mut url = String::with_capacity(template.len());
    for piece in pieces(template)? {
        match piece {
            Piece::Text(text) => url.push_str(text),
            Piece::Identifier {
                name,
                identifier,
                width,
            } => {
                let value = identifier.value(values).ok_or_else(|| {
                    invalid_template(template, &format!("${name}$ has no value here"))
                })?;
                url.push_str(&padded(&value, width));
            }
        }
    }
    Ok(url)
}

fn invalid_template(template: &str, reason: &str) -> Error {
    Error::DashManifestInvalidSegments(format!("{template}: {reason}"))
}

/// A media URL template with every identifier but `$Number$` and `$Time$`
/// filled in, which differ per segment.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UrlTemplate {
    parts: Vec<UrlPart>,
}

/// A part of a [`UrlTemplate`].
#[derive(Debug, PartialEq, Eq)]
enum UrlPart {
    Text(String),
    Number { width: Option<usize> },
    Time { width: Option<usize> },
}

impl UrlTemplate {
    /// Reads `template`, filling its identifiers from `values` except
    /// `$Number$` and `$Time$`. Fails as [`fill`] does.
    pub(crate) fn parse(template: &str, values: &TemplateValues) -> Result<Self, Error> {
        let mut parts = Vec::new();
        let mut text = String::new();
        for piece in pieces(template)? {
            let part = match piece {
                Piece::Text(piece) => {
                    text.push_str(piece);
                    continue;
                }
                Piece::Identifier {
                    identifier: Identifier::Number,
                    width,
                    ..
                } => UrlPart::Number { width },
                Piece::Identifier {
                    identifier: Identifier::Time,
                    width,
                    ..
                } => UrlPart::Time { width },
                Piece::Identifier {
                    name,
                    identifier,
                    width,
                } => {
                    let value = identifier.value(values).ok_or_else(|| {
                        invalid_template(template, &format!("${name}$ has no value here"))
                    })?;
                    text.push_str(&padded(&value, width));
                    continue;
                }
            };
            parts.push(UrlPart::Text(std::mem::take(&mut text)));
            parts.push(part);
        }
        parts.push(UrlPart::Text(text));
        Ok(Self { parts })
    }

    /// The URL of the segment with `$Number$` `number` and `$Time$` `time`.
    fn render(&self, number: u64, time: u64) -> String {
        let mut url = String::new();
        for part in &self.parts {
            match part {
                UrlPart::Text(text) => url.push_str(text),
                UrlPart::Number { width } => url.push_str(&padded(&number.to_string(), *width)),
                UrlPart::Time { width } => url.push_str(&padded(&time.to_string(), *width)),
            }
        }
        url
    }
}

/// The locations of a representation's templated media segments: its media
/// template, resolved against its base URL.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MediaUrls {
    template: UrlTemplate,
    base: Option<Url>,
}

impl MediaUrls {
    pub(crate) fn new(template: UrlTemplate, base: Option<Url>) -> Self {
        Self { template, base }
    }

    /// The URL of the segment with `$Number$` `number` and `$Time$` `time`.
    /// Fails with [`Error::DashManifestUrl`] if it does not resolve.
    fn resolve(&self, number: u64, time: u64) -> Result<Url, Error> {
        resolve_url(self.base.as_ref(), &self.template.render(number, time))
    }

    /// Validates that all substitutions produce resolvable URLs on one origin.
    ///
    /// Fails with [`Error::DashManifestUrl`] if a URL does not resolve, and
    /// with [`Error::DashManifestInvalidSegments`] if the template puts
    /// `$Number$` or `$Time$` in the scheme, host or port.
    pub(crate) fn check(&self) -> Result<(), Error> {
        // Two numeric substitutions are sufficient to detect placeholders in the origin.
        let (zero, one) = (self.resolve(0, 0)?, self.resolve(1, 1)?);
        let server = |url: &Url| {
            (
                url.scheme().to_owned(),
                url.host().map(|host| host.to_owned()),
                url.port(),
            )
        };
        if server(&zero) != server(&one) {
            return Err(Error::DashManifestInvalidSegments(format!(
                "media template {zero} puts $Number$ or $Time$ in its scheme, host or port"
            )));
        }
        Ok(())
    }

    /// Returns a segment location after [`Self::check`] has validated the template.
    pub(crate) fn location(&self, number: u64, time: u64) -> SegmentLocation {
        let url = self
            .resolve(number, time)
            .expect("media templates are checked when their index is built");
        SegmentLocation::new(url)
    }
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

    /// The initialization segment's location, with an unresolved URL.
    ///
    /// It comes from `@initialization`, or else an inherited `Initialization`
    /// element.
    pub fn init_location(self, values: TemplateValues) -> Result<SegmentLocation, Error> {
        match self.get(|template| template.initialization.as_deref()) {
            Some(initialization) => Ok(SegmentLocation::new(fill(initialization, &values)?)),
            None => initialization_location(
                self.get(|template| template.Initialization.as_ref())
                    .ok_or(Error::DashManifestMissingUrls)?,
            ),
        }
    }

    /// The media segments' URL template, from `@media`.
    pub fn media_template(self, values: TemplateValues) -> Result<UrlTemplate, Error> {
        let media = self
            .get(|template| template.media.as_deref())
            .ok_or(Error::DashManifestMissingMediaTemplate)?;
        UrlTemplate::parse(media, &values)
    }

    /// Builds segment timing, allowing an open-ended timeline when `live` is `true`.
    pub fn addressed(
        self,
        period_duration: Option<Duration>,
        live: bool,
    ) -> Result<Addressed, Error> {
        Timing {
            live,
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
        .addressed(period_duration)
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

    #[test]
    fn a_url_template_renders_as_fill_does() {
        for template in [
            "$RepresentationID$/$Bandwidth$/$Time$-$Number$.m4s",
            "$representationid$/$time%012d$-$NUMBER%03d$.m4s",
            "a$$b/$Unknown$Number$/c$",
            "plain.m4s",
        ] {
            let parsed = UrlTemplate::parse(template, &values()).unwrap();
            assert_eq!(
                parsed.render(42, 176_128),
                fill(template, &values()).unwrap(),
                "{template}"
            );
        }
    }

    #[test]
    fn a_url_template_needs_its_values_when_read() {
        let values = TemplateValues {
            bandwidth: None,
            number: None,
            time: None,
            ..values()
        };
        assert!(UrlTemplate::parse("$Number$-$Time$.m4s", &values).is_ok());
        assert!(matches!(
            UrlTemplate::parse("$Bandwidth$/$Number$.m4s", &values),
            Err(Error::DashManifestInvalidSegments(message)) if message.contains("$Bandwidth$")
        ));
        assert!(matches!(
            UrlTemplate::parse("$Number%5x$.m4s", &values),
            Err(Error::DashManifestInvalidSegments(message)) if message.contains("%5x")
        ));
    }

    #[test]
    fn media_urls_resolve_against_their_base() {
        let template = UrlTemplate::parse("$Number$.m4s", &values()).unwrap();
        let base = Url::parse("https://cdn.example/audio/").unwrap();
        let urls = MediaUrls::new(template, Some(base));
        assert_eq!(
            urls.location(7, 0),
            SegmentLocation::new("https://cdn.example/audio/7.m4s")
        );

        let template = UrlTemplate::parse("$Number$.m4s", &values()).unwrap();
        let relative = MediaUrls::new(template, None);
        assert!(matches!(relative.check(), Err(Error::DashManifestUrl(..))));
    }

    #[test]
    fn media_urls_keep_numbers_and_times_out_of_the_server() {
        let urls = |template: &str| {
            let template = UrlTemplate::parse(template, &values()).unwrap();
            MediaUrls::new(template, Some(Url::parse("https://cdn.example/").unwrap()))
        };
        for allowed in [
            "$Number$.m4s",
            "https://other.example/$Time$/a.m4s?n=$Number$#t=$Time$",
            "https://user$Number$@other.example/a.m4s",
        ] {
            assert!(urls(allowed).check().is_ok(), "{allowed}");
        }
        for server in [
            "https://cdn$Number$.example/a.m4s",
            "https://cdn.example:$Number%04d$/a.m4s",
            "//$Time$.example/a.m4s",
        ] {
            assert!(
                matches!(
                    urls(server).check(),
                    Err(Error::DashManifestInvalidSegments(_))
                ),
                "{server}"
            );
        }
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

    fn summarize(addressed: &Addressed) -> Vec<(u64, u64, u64)> {
        addressed
            .segments
            .times()
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
            .addressed(None, false)
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
            .addressed(None, false)
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
            .addressed(Some(Duration::from_millis(2500)), false)
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
            .addressed(Some(Duration::from_millis(2500)), false)
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
            TemplateChain::new([Some(&template), None, None]).addressed(None, false),
            Err(Error::DashManifestMissingDuration)
        ));
    }

    #[test]
    fn end_number_caps_timeline() {
        let mut template = template(Some(vec![entry(None, 1000, Some(-1))]));
        template.startNumber = Some(5);
        template.endNumber = Some(7);
        let segments = TemplateChain::new([Some(&template), None, None])
            .addressed(None, false)
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
            .addressed(Some(Duration::from_millis(10_500)), false)
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
            .addressed(Some(Duration::from_millis(3004)), false)
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
            .addressed(None, false)
            .unwrap();
        assert_eq!(summarize(&segments), [(3, 0, 2000), (4, 2000, 2000)]);
    }

    #[test]
    fn duration_addressing_needs_a_bound() {
        let mut template = template(None);
        template.duration = Some(2000.0);
        assert!(matches!(
            TemplateChain::new([Some(&template), None, None]).addressed(None, false),
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
            .addressed(None, false)
            .unwrap();
        assert_eq!(summarize(&segments), [(1, 0, 500), (2, 500, 500)]);
    }

    #[test]
    fn rejects_zero_timescale() {
        let mut template = template(Some(vec![entry(None, 1000, None)]));
        template.timescale = Some(0);
        assert!(matches!(
            TemplateChain::new([Some(&template), None, None]).addressed(None, false),
            Err(Error::DashManifestMissingTimescale)
        ));
    }
}
