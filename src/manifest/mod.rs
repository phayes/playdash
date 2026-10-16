//! Parsed MPEG-DASH manifests.

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
use crate::fragment::{Fragment, Fragments, MediaTimeline};
use crate::vendor::dash_mpd_core::{
    AdaptationSet, BaseURL, ContentProtection, MPD, Period, Representation as CoreRepresentation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
pub use representation::{ContentKind, FrameRate, Representation};
use segment_list::ListChain;
use segments::Segments;
use std::collections::HashMap;
use template::{TemplateChain, TemplateValues};
use url::Url;

/// A parsed MPEG-DASH manifest and the entry point for selecting and streaming
/// its representations.
///
/// Load remote manifests with `DashManifest::new_from_url` (the `sync`
/// feature). When parsing XML obtained another way, use [`DashManifest::new`]
/// and set its source URL with [`DashManifest::with_base_url`] if it contains
/// relative URLs. Parsing never touches the network. With the `async`
/// feature, `nonblocking::DashManifest` mirrors this type and streams
/// asynchronously.
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
/// assert_eq!(manifest.fragments("audio")?.media.len(), 2);
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

    fn segments(self) -> Result<Segments, Error> {
        if self.needs_segment_index() {
            return Err(Error::DashManifestInvalidSegments(
                "SegmentBase index not loaded; load it with load_segment_indexes".to_owned(),
            ));
        }
        if self.uses_segment_list() {
            self.lists().segments(self.period_duration())
        } else {
            self.templates().segments(self.period_duration())
        }
    }

    fn fragments(
        self,
        manifest_url: Option<&Url>,
        segments: &Segments,
    ) -> Result<Fragments, Error> {
        let fragments = if self.uses_segment_list() {
            self.lists().fragments(segments)?
        } else {
            let values = TemplateValues {
                representation_id: self.id(),
                bandwidth: self.representation.bandwidth,
                ..TemplateValues::default()
            };
            self.templates().fragments(values, segments)?
        };

        // Resolve every URL against the BaseURL chain.
        let base = self.base_url(manifest_url)?;
        let resolve = |fragment: Fragment| -> Result<Fragment, Error> {
            Ok(Fragment {
                url: resolve_url(base.as_ref(), &fragment.url)?.into(),
                ..fragment
            })
        };
        Ok(Fragments {
            init: resolve(fragments.init)?,
            media: fragments
                .media
                .into_iter()
                .map(resolve)
                .collect::<Result<_, _>>()?,
        })
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
    /// Parses MPEG-DASH XML. Parsing never touches the network.
    ///
    /// Relative segment and `BaseURL` references cannot be resolved without
    /// knowing where the manifest came from; set it with
    /// [`DashManifest::with_base_url`] for those manifests.
    ///
    /// A representation addressed only by `SegmentBase` keeps its segments in
    /// a `sidx` index inside the media file. It becomes streamable once that
    /// index is loaded, with `load_segment_indexes` on this type (the `sync`
    /// feature) or on `nonblocking::DashManifest` (the `async` feature).
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

    /// Decodes a base64 `data:` URL and parses the embedded MPEG-DASH XML.
    pub fn new_from_data_url(data_url: &str) -> Result<Self, Error> {
        let dash_xml = decode_data_url(data_url)?;
        Self::new(dash_xml)
    }

    /// Sets the absolute URL the manifest was loaded from. Relative `BaseURL`
    /// and segment references resolve against it.
    pub fn with_base_url(mut self, url: &str) -> Result<Self, Error> {
        self.base_url = Some(resolve_url(None, url)?);
        Ok(self)
    }

    /// The URL the manifest was loaded from, if known.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_ref().map(Url::as_str)
    }

    /// Lists representations in document order, across all periods and
    /// adaptation sets.
    ///
    /// Use this to inspect the available IDs, codecs, and bitrates before
    /// selecting one with [`DashManifest::representation`] or
    /// [`DashManifest::fragments`].
    pub fn representations(&self) -> impl Iterator<Item = Representation<'_>> {
        self.selections().map(Selected::summary)
    }

    /// Finds a representation by ID.
    ///
    /// The first pass matches `representation_id` against the full
    /// representation ID. If that misses, a second pass matches the format
    /// token before the first comma. Both comparisons are case-insensitive. The
    /// first match in document order wins.
    pub fn representation(&self, representation_id: impl AsRef<str>) -> Option<Representation<'_>> {
        self.find(representation_id.as_ref()).map(Selected::summary)
    }

    /// Lists a representation's initialization fragment and its media
    /// fragments in timeline order.
    ///
    /// Fragments come from the representation's `SegmentTemplate` or
    /// `SegmentList` (whose entries may carry byte ranges), with URLs resolved
    /// against the `BaseURL` chain and [`DashManifest::base_url`]. The media
    /// fragments line up with [`DashManifest::media_timeline`].
    pub fn fragments(&self, representation_id: impl AsRef<str>) -> Result<Fragments, Error> {
        let (selected, segments) = self.select_segments(representation_id.as_ref())?;
        selected.fragments(self.base_url.as_ref(), &segments)
    }

    /// Media-segment durations in timeline order, with the DASH timescale.
    pub fn media_timeline(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<MediaTimeline, Error> {
        Ok(self
            .select_segments(representation_id.as_ref())?
            .1
            .media_timeline())
    }

    /// MIME type of a representation, inherited from its adaptation set if absent.
    pub fn mime_type(&self, representation_id: impl AsRef<str>) -> Result<Option<&str>, Error> {
        Ok(self.select(representation_id.as_ref())?.mime_type())
    }

    /// Common-encryption scheme protecting a representation, such as `cenc` or
    /// `cbcs`, inherited from its adaptation set if absent.
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

    /// A representation and its media segments, or the error that kept its
    /// `SegmentBase` index from loading.
    fn select_segments(&self, id: &str) -> Result<(Selected<'_>, Segments), Error> {
        let selected = self.select(id)?;
        if selected.needs_segment_index()
            && let Some(reason) = self.segment_index_errors.get(selected.id())
        {
            return Err(Error::DashManifestSegmentIndex {
                representation: selected.id().to_owned(),
                reason: reason.clone(),
            });
        }
        Ok((selected, selected.segments()?))
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
