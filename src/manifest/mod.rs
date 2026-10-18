//! MPEG-DASH manifests: loading a stream and choosing its audio or video.

#[cfg(feature = "sync")]
mod fetch;
mod representation;
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod segment_base;
mod segment_list;
mod segments;
mod template;
#[cfg(test)]
pub(crate) mod tests;

use std::time::Duration;

use crate::error::Error;
use crate::segment::{MediaSegment, MediaTiming, SegmentLocation, Segments};
use crate::vendor::dash_mpd_core::{
    AdaptationSet, BaseURL, ContentProtection, MPD, Period, Representation as CoreRepresentation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
pub use representation::{ContentKind, FrameRate, Representation};
use segment_list::ListChain;
use segments::SegmentTimes;
use std::collections::HashMap;
use template::{TemplateChain, TemplateValues};
use url::Url;

/// A parsed MPEG-DASH manifest and the starting point for playback.
///
/// A manifest is the small XML document that describes the audio and video choices in a
/// DASH stream. Load one with [`DashManifest::new_from_url`], inspect its
/// [`representations`](Self::representations), then open a representation with
/// `media_stream` when the `sync` or `async` feature is enabled.
///
/// Use [`Self::new`] when you already have the XML. If that XML contains relative URLs,
/// also call [`Self::with_base_url`] with the URL it came from. The `async` feature
/// provides the matching [`nonblocking::DashManifest`](crate::nonblocking::DashManifest).
///
/// # Examples
///
/// ```
/// use playdash::DashManifest;
///
/// let xml = r#"
///     <MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
///          mediaPresentationDuration="PT4S">
///       <Period>
///         <AdaptationSet mimeType="audio/mp4">
///           <Representation id="audio" bandwidth="128000">
///             <SegmentTemplate initialization="init.mp4" media="$Number$.m4s"
///                              duration="2" timescale="1" startNumber="1" />
///           </Representation>
///         </AdaptationSet>
///       </Period>
///     </MPD>
/// "#;
///
/// let manifest = DashManifest::new(xml)?
///     .with_base_url("https://media.example/stream.mpd")?;
/// let audio = manifest.representation("audio").unwrap();
/// assert_eq!(audio.bandwidth, Some(128_000));
/// assert_eq!(manifest.segments("audio")?.media_len(), 2);
/// # Ok::<(), playdash::Error>(())
/// ```
#[derive(Clone, Default, Debug, PartialEq)]
pub struct DashManifest {
    mpd: MPD,
    /// URL the manifest was fetched from; the root of `BaseURL` resolution.
    base_url: Option<Url>,
    /// Why each `SegmentBase` index that failed to load did so, by
    /// representation ID; reported when that representation is streamed.
    segment_index_errors: HashMap<String, String>,
}

/// A representation together with the elements it inherits attributes from.
#[derive(Clone, Copy)]
struct Selected<'a> {
    mpd: &'a MPD,
    period: &'a Period,
    next_period: Option<&'a Period>,
    adaptation: &'a AdaptationSet,
    /// The position of `adaptation` in document order, across all periods.
    adaptation_index: usize,
    representation: &'a CoreRepresentation,
}

fn decode_data_url(data_url: &str) -> Result<String, Error> {
    let (metadata, encoded) = data_url
        .strip_prefix("data:")
        .and_then(|value| value.split_once(','))
        .ok_or(Error::InvalidDataUrl)?;

    if !metadata
        .split(';')
        .skip(1)
        .any(|parameter| parameter.eq_ignore_ascii_case("base64"))
    {
        return Err(Error::InvalidDataUrl);
    }

    let bytes = STANDARD.decode(encoded)?;
    Ok(String::from_utf8(bytes)?)
}

impl<'a> Selected<'a> {
    fn id(self) -> &'a str {
        self.representation.id.as_deref().unwrap_or("")
    }

    /// Segment templates from the most to the least specific level.
    fn templates(self) -> TemplateChain<'a> {
        TemplateChain::new([
            self.representation.SegmentTemplate.as_ref(),
            self.adaptation.SegmentTemplate.as_ref(),
            self.period.SegmentTemplate.as_ref(),
        ])
    }

    /// Segment lists from the most to the least specific level.
    fn lists(self) -> ListChain<'a> {
        ListChain::new([
            self.representation.SegmentList.as_ref(),
            self.adaptation.SegmentList.as_ref(),
            self.period.SegmentList.as_ref(),
        ])
    }

    /// Whether only `SegmentBase` addresses this representation, so its
    /// segments must come from the `sidx` index in the media file.
    fn needs_segment_index(self) -> bool {
        let (representation, adaptation, period) =
            (self.representation, self.adaptation, self.period);
        let explicit = representation.SegmentList.is_some()
            || representation.SegmentTemplate.is_some()
            || adaptation.SegmentList.is_some()
            || adaptation.SegmentTemplate.is_some()
            || period.SegmentList.is_some()
            || period.SegmentTemplate.is_some();
        let base = representation.SegmentBase.is_some()
            || adaptation.SegmentBase.is_some()
            || period.SegmentBase.is_some();
        !explicit && base
    }

    /// Whether the most specific level with segment addressing uses a
    /// `SegmentList` rather than a `SegmentTemplate`.
    fn uses_segment_list(self) -> bool {
        [
            (
                self.representation.SegmentTemplate.is_some(),
                self.representation.SegmentList.is_some(),
            ),
            (
                self.adaptation.SegmentTemplate.is_some(),
                self.adaptation.SegmentList.is_some(),
            ),
            (
                self.period.SegmentTemplate.is_some(),
                self.period.SegmentList.is_some(),
            ),
        ]
        .into_iter()
        .find(|&(template, list)| template || list)
        .is_some_and(|(_, list)| list)
    }

    fn mime_type(self) -> Option<&'a str> {
        self.representation
            .mimeType
            .as_deref()
            .or(self.adaptation.mimeType.as_deref())
    }

    fn codecs(self) -> Option<&'a str> {
        self.representation
            .codecs
            .as_deref()
            .or(self.adaptation.codecs.as_deref())
    }

    fn protection_scheme(self) -> Option<String> {
        common_encryption_scheme(&self.representation.ContentProtection)
            .or_else(|| common_encryption_scheme(&self.adaptation.ContentProtection))
    }

    /// Period length: `Period@duration`, else up to the next `Period@start`,
    /// else up to the end of `MPD@mediaPresentationDuration`.
    fn period_duration(self) -> Option<Duration> {
        if let Some(duration) = self.period.duration {
            return Some(duration);
        }
        let end = match self.next_period {
            Some(next) => next.start,
            None => self.mpd.mediaPresentationDuration,
        }?;
        end.checked_sub(self.period.start.unwrap_or_default())
    }

    fn segment_times(self) -> Result<SegmentTimes, Error> {
        if self.needs_segment_index() {
            return Err(Error::DashManifestInvalidSegments(
                "SegmentBase index not loaded; load it with load_segment_indexes".to_owned(),
            ));
        }
        if self.uses_segment_list() {
            self.lists().segment_times(self.period_duration())
        } else {
            self.templates().segment_times(self.period_duration())
        }
    }

    fn segments(self, manifest_url: Option<&Url>, times: &SegmentTimes) -> Result<Segments, Error> {
        let urls = if self.uses_segment_list() {
            self.lists().urls(times)?
        } else {
            let values = TemplateValues {
                representation_id: self.id(),
                bandwidth: self.representation.bandwidth,
                ..TemplateValues::default()
            };
            self.templates().urls(values, times)?
        };

        // Resolve every URL against the BaseURL chain.
        let base = self.base_url(manifest_url)?;
        let resolve = |location: SegmentLocation| -> Result<SegmentLocation, Error> {
            Ok(SegmentLocation {
                url: resolve_url(base.as_ref(), &location.url)?.into(),
                ..location
            })
        };

        let init = resolve(urls.init)?;

        // Time each media segment, on the Period's clock.
        let timing = MediaTiming {
            timescale: times.timescale,
            presentation_time_offset: times.presentation_time_offset,
            period_start: self.period.start.unwrap_or_default(),
        };
        let media = urls
            .media
            .into_iter()
            .zip(&times.segments)
            .map(|(location, time)| {
                Ok(MediaSegment {
                    location: resolve(location)?,
                    start: time.time,
                    duration: time.duration,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Segments::timed(init, timing, media)
    }

    /// Resolves the first `BaseURL` of each level (MPD, Period, AdaptationSet,
    /// Representation) against the one above it, starting at the manifest URL.
    fn base_url(self, manifest_url: Option<&Url>) -> Result<Option<Url>, Error> {
        let levels: [&[BaseURL]; 4] = [
            &self.mpd.base_url,
            &self.period.BaseURL,
            &self.adaptation.BaseURL,
            &self.representation.BaseURL,
        ];
        let mut base = manifest_url.cloned();
        for element in levels.into_iter().filter_map(<[BaseURL]>::first) {
            base = Some(resolve_url(base.as_ref(), &element.base)?);
        }
        Ok(base)
    }
}

/// Resolves `reference` (RFC 3986) against `base`; without a base it must be absolute.
/// An empty reference is the base itself.
fn resolve_url(base: Option<&Url>, reference: &str) -> Result<Url, Error> {
    let reference = reference.trim();
    match base {
        Some(base) => base.join(reference),
        None => Url::parse(reference),
    }
    .map_err(|error| Error::DashManifestUrl(reference.to_owned(), error))
}

impl DashManifest {
    /// Parses MPEG-DASH manifest XML without making a network request.
    ///
    /// Relative segment and `BaseURL` references cannot be resolved without
    /// knowing where the manifest came from; set it with
    /// [`DashManifest::with_base_url`] for those manifests.
    ///
    /// A representation addressed only by `SegmentBase` keeps its segments in
    /// a `sidx` index inside the media file. It becomes streamable once that
    /// index is loaded, with [`load_segment_indexes`](Self::load_segment_indexes) on this
    /// type (the `sync` feature) or on
    /// [`nonblocking::DashManifest`](crate::nonblocking::DashManifest) (the `async`
    /// feature).
    /// Failure to load one representation's index does not prevent callers
    /// from using other representations.
    pub fn new(dash_xml: impl AsRef<str>) -> Result<Self, Error> {
        let manifest = Self {
            mpd: crate::vendor::dash_mpd_core::parse(dash_xml.as_ref())?,
            base_url: None,
            segment_index_errors: HashMap::new(),
        };

        if manifest.selections().next().is_none() {
            return Err(Error::DashManifestMissingRepresentations);
        }
        if manifest
            .representations()
            .any(|representation| representation.id.is_empty())
        {
            return Err(Error::DashManifestMissingRepresentationId);
        }
        Ok(manifest)
    }

    /// Parses a manifest embedded in a base64-encoded `data:` URL.
    ///
    /// This performs no network request. Relative media URLs still need a base URL set
    /// with [`Self::with_base_url`].
    pub fn new_from_data_url(data_url: &str) -> Result<Self, Error> {
        let dash_xml = decode_data_url(data_url)?;
        Self::new(dash_xml)
    }

    /// Sets the absolute source URL used to resolve relative media references.
    ///
    /// Use the manifest URL itself, not its containing directory; standard URL joining
    /// handles the directory. This replaces a previously set base URL.
    pub fn with_base_url(mut self, url: &str) -> Result<Self, Error> {
        self.base_url = Some(resolve_url(None, url)?);
        Ok(self)
    }

    /// Returns the absolute source URL used to resolve relative media references.
    /// Returns `None` when the manifest was parsed without one.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_ref().map(Url::as_str)
    }

    /// Lists the media choices advertised by the manifest.
    ///
    /// A representation is one encoded variant, such as 720p video or 128 kbit/s audio.
    /// Inspect these values, choose one, and pass its ID to `media_stream`.
    pub fn representations(&self) -> impl Iterator<Item = Representation<'_>> {
        self.selections().map(Selected::summary)
    }

    /// Finds one advertised media choice by representation ID.
    ///
    /// The first pass matches `representation_id` against the full
    /// representation ID. If that misses, a second pass matches the format
    /// token before the first comma. Both comparisons are case-insensitive. The
    /// first match in document order wins.
    pub fn representation(&self, representation_id: impl AsRef<str>) -> Option<Representation<'_>> {
        self.find(representation_id.as_ref()).map(Selected::summary)
    }

    /// Lists only the representations classified as `kind`, in manifest order.
    ///
    /// Representations carrying both video and audio are [`ContentKind::Muxed`], and are
    /// not listed for [`ContentKind::Video`] or [`ContentKind::Audio`].
    pub fn representations_of(
        &self,
        kind: ContentKind,
    ) -> impl Iterator<Item = Representation<'_>> {
        self.representations()
            .filter(move |representation| representation.kind == kind)
    }

    /// Chooses the `kind` representation with the highest advertised bitrate.
    ///
    /// Returns `None` if no representation of `kind` advertises a bandwidth.
    pub fn highest_bandwidth(&self, kind: ContentKind) -> Option<Representation<'_>> {
        self.representations_of(kind)
            .filter(|representation| representation.bandwidth.is_some())
            .reduce(|best, next| match next.bandwidth > best.bandwidth {
                true => next,
                false => best,
            })
    }

    /// Chooses the `kind` representation with the lowest advertised bitrate.
    ///
    /// Returns `None` if no representation of `kind` advertises a bandwidth.
    pub fn lowest_bandwidth(&self, kind: ContentKind) -> Option<Representation<'_>> {
        self.representations_of(kind)
            .filter(|representation| representation.bandwidth.is_some())
            .reduce(|best, next| match next.bandwidth < best.bandwidth {
                true => next,
                false => best,
            })
    }

    /// Resolves the initialization segment and the timed media segments of a
    /// representation.
    ///
    /// This is a lower-level query; use `media_stream` for decoder-ready playback.
    ///
    /// Segments come from the representation's `SegmentTemplate` or
    /// `SegmentList` (whose entries may carry byte ranges), with URLs resolved
    /// against the `BaseURL` chain and [`DashManifest::base_url`]. Their spans
    /// are presentation time, shared by every representation of the manifest.
    pub fn segments(&self, representation_id: impl AsRef<str>) -> Result<Segments, Error> {
        let (selected, times) = self.select_segment_times(representation_id.as_ref())?;
        selected.segments(self.base_url.as_ref(), &times)
    }

    /// Returns a representation's MIME type, inherited from its adaptation set.
    /// Returns `Ok(None)` when neither level declares one.
    pub fn mime_type(&self, representation_id: impl AsRef<str>) -> Result<Option<&str>, Error> {
        Ok(self.select(representation_id.as_ref())?.mime_type())
    }

    /// Returns the representation's Common Encryption scheme, such as `cenc` or `cbcs`.
    ///
    /// The value is inherited from its adaptation set. `Ok(None)` means no compatible
    /// protection declaration was found; it does not inspect downloaded media.
    pub fn protection_scheme(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<Option<String>, Error> {
        Ok(self.select(representation_id.as_ref())?.protection_scheme())
    }

    fn selections(&self) -> impl Iterator<Item = Selected<'_>> {
        let mpd = &self.mpd;
        let adaptations = mpd
            .periods
            .iter()
            .enumerate()
            .flat_map(move |(index, period)| {
                let next_period = mpd.periods.get(index + 1);
                period
                    .adaptations
                    .iter()
                    .map(move |adaptation| (period, next_period, adaptation))
            });
        adaptations.enumerate().flat_map(
            move |(adaptation_index, (period, next_period, adaptation))| {
                adaptation
                    .representations
                    .iter()
                    .map(move |representation| Selected {
                        mpd,
                        period,
                        next_period,
                        adaptation,
                        adaptation_index,
                        representation,
                    })
            },
        )
    }

    fn find(&self, id: &str) -> Option<Selected<'_>> {
        self.selections()
            .find(|selected| selected.id().eq_ignore_ascii_case(id))
            .or_else(|| {
                self.selections()
                    .find(|selected| representation_token(selected.id()).eq_ignore_ascii_case(id))
            })
    }

    /// A representation and its media segments' times, or the error that kept its
    /// `SegmentBase` index from loading.
    fn select_segment_times(&self, id: &str) -> Result<(Selected<'_>, SegmentTimes), Error> {
        let selected = self.select(id)?;
        if selected.needs_segment_index()
            && let Some(reason) = self.segment_index_errors.get(selected.id())
        {
            return Err(Error::DashManifestSegmentIndex {
                representation: selected.id().to_owned(),
                reason: reason.clone(),
            });
        }
        Ok((selected, selected.segment_times()?))
    }

    fn select(&self, id: &str) -> Result<Selected<'_>, Error> {
        self.find(id)
            .ok_or_else(|| Error::DashManifestMissingRepresentation(id.to_owned()))
    }
}

fn representation_token(id: &str) -> &str {
    id.split(',').next().unwrap_or(id)
}

fn common_encryption_scheme(protections: &[ContentProtection]) -> Option<String> {
    protections
        .iter()
        .find(|protection| {
            protection
                .schemeIdUri
                .eq_ignore_ascii_case("urn:mpeg:dash:mp4protection:2011")
        })
        .map(|protection| {
            protection
                .value
                .clone()
                .unwrap_or_else(|| "common encryption".to_owned())
        })
}
