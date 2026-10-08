//! Parsed TIDAL MPEG-DASH manifests.

use std::time::Duration;

use crate::error::Error;
use crate::manifest_segment_base::BaseChain;
use crate::manifest_segment_list::ListChain;
use crate::manifest_template::{Segments, TemplateChain, TemplateValues};
use crate::stream::{Fragment, FragmentCache, MediaTimeline, MpegStreamReader, Transport};
use base64::{Engine, engine::general_purpose::STANDARD};
use dash_mpd_core::{
    AdaptationSet, BaseURL, ContentProtection, MPD, Period, Representation, SegmentList,
};
use std::collections::HashMap;
use std::sync::Arc;
use ureq::ResponseExt;
use url::Url;

/// A parsed MPEG-DASH manifest.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct DashManifest {
    /// The full manifest as parsed by [`dash_mpd_core`].
    pub mpd: MPD,
    /// Where the manifest was fetched from; the root of `BaseURL` resolution.
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
    representation: &'a Representation,
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

    /// Segment bases from the most to the least specific level.
    fn bases(self) -> BaseChain<'a> {
        BaseChain::new([
            self.representation.SegmentBase.as_ref(),
            self.adaptation.SegmentBase.as_ref(),
            self.period.SegmentBase.as_ref(),
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
        !explicit && self.bases().is_present()
    }

    /// The `SegmentList` equivalent of this representation's `SegmentBase`
    /// index, or `None` if it does not need one or its URL is relative and
    /// there is no manifest URL yet.
    fn segment_index(
        self,
        manifest_url: Option<&Url>,
        transport: &dyn Transport,
    ) -> Result<Option<SegmentList>, Error> {
        if !self.needs_segment_index() {
            return Ok(None);
        }
        let url = match self.base_url(manifest_url) {
            Ok(Some(url)) => url,
            Ok(None) | Err(Error::DashManifestUrl(_, url::ParseError::RelativeUrlWithoutBase))
                if manifest_url.is_none() =>
            {
                return Ok(None);
            }
            Ok(None) => return Err(Error::DashManifestMissingUrls),
            Err(error) => return Err(error),
        };
        self.bases().segment_list(url.as_str(), transport).map(Some)
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
                "SegmentBase index not loaded: its URL is relative and the manifest has no base URL"
                    .to_owned(),
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
    ) -> Result<Vec<Fragment>, Error> {
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
        let base = self.base_url(manifest_url)?;
        fragments
            .into_iter()
            .map(|fragment| {
                Ok(Fragment {
                    url: resolve_url(base.as_ref(), &fragment.url)?.into(),
                    ..fragment
                })
            })
            .collect()
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
    /// Parses an MPEG-DASH manifest.
    ///
    /// Relative segment and `BaseURL` references cannot be resolved without
    /// knowing where the manifest came from; use [`DashManifest::new_from_url`]
    /// or [`DashManifest::with_base_url`] for those manifests.
    ///
    /// Representations addressed only by `SegmentBase` have their `sidx`
    /// index fetched over HTTP and rewritten as an equivalent `SegmentList`.
    /// Those with relative URLs are fetched by [`DashManifest::with_base_url`]
    /// instead. An index that fails to load is logged and does not fail the
    /// manifest; streaming that representation returns
    /// [`Error::DashManifestSegmentIndex`].
    pub fn new(dash_xml: impl AsRef<str>) -> Result<Self, Error> {
        let mut manifest = Self {
            mpd: dash_mpd_core::parse(dash_xml.as_ref())?,
            base_url: None,
            segment_index_errors: HashMap::new(),
        };

        if manifest.selections().next().is_none() {
            return Err(Error::DashManifestMissingRepresentations);
        }
        if manifest
            .representations()
            .any(|representation| representation.id.as_deref().is_none_or(str::is_empty))
        {
            return Err(Error::DashManifestMissingRepresentationId);
        }

        manifest.load_segment_indexes();
        Ok(manifest)
    }

    /// Fetches the `sidx` of every `SegmentBase` representation whose URL
    /// resolves, through a default [`ureq::Agent`].
    fn load_segment_indexes(&mut self) {
        if self.selections().any(Selected::needs_segment_index) {
            self.load_segment_indexes_with(&ureq::Agent::new_with_defaults());
        }
    }

    /// Rewrites each `SegmentBase` index fetched through `transport` as a
    /// `SegmentList` on its representation.
    ///
    /// A representation whose index fails to load is left as it was, and the
    /// error is kept to report if that representation is streamed, so one bad
    /// index does not fail the whole manifest.
    fn load_segment_indexes_with(&mut self, transport: &dyn Transport) {
        let mut failures = Vec::new();
        let lists: Vec<_> = self
            .selections()
            .map(|selected| {
                selected
                    .segment_index(self.base_url.as_ref(), transport)
                    .unwrap_or_else(|error| {
                        #[cfg(feature = "log")]
                        log::warn!(
                            "SegmentBase index for representation {} failed to load: {error}",
                            selected.id()
                        );
                        failures.push((selected.id().to_owned(), error.to_string()));
                        None
                    })
            })
            .collect();
        let representations = self
            .mpd
            .periods
            .iter_mut()
            .flat_map(|period| &mut period.adaptations)
            .flat_map(|adaptation| &mut adaptation.representations);
        // `selections` yields representations in this same document order.
        for (representation, list) in representations.zip(lists) {
            if list.is_some() {
                if let Some(id) = &representation.id {
                    self.segment_index_errors.remove(id);
                }
                representation.SegmentList = list;
            }
        }
        self.segment_index_errors.extend(failures);
    }

    /// Decodes a base64 `data:` URL and parses the embedded MPEG-DASH XML.
    pub fn new_from_data_url(data_url: &str) -> Result<Self, Error> {
        let dash_xml = decode_data_url(data_url)?;
        Self::new(dash_xml)
    }

    /// Fetches and parses an MPEG-DASH manifest.
    ///
    /// The final URL, after redirects, becomes the [`DashManifest::base_url`]
    /// that relative `BaseURL` and segment references resolve against. `data:`
    /// URLs are decoded as with [`DashManifest::new_from_data_url`].
    pub fn new_from_url(url: &str) -> Result<Self, Error> {
        if url.trim_start().starts_with("data:") {
            return Self::new_from_data_url(url.trim());
        }
        let response = ureq::get(url).call().map_err(Error::DashManifestFetch)?;
        let final_url = response.get_uri().to_string();
        let dash_xml = response
            .into_body()
            .read_to_string()
            .map_err(Error::DashManifestFetch)?;
        Self::new(dash_xml)?.with_base_url(&final_url)
    }

    /// Sets the absolute URL the manifest was loaded from.
    ///
    /// Relative `BaseURL` and segment references resolve against it, and any
    /// `SegmentBase` index that needed it is fetched now.
    pub fn with_base_url(mut self, url: &str) -> Result<Self, Error> {
        self.base_url = Some(resolve_url(None, url)?);
        self.load_segment_indexes();
        Ok(self)
    }

    /// The URL the manifest was loaded from, if known.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_ref().map(Url::as_str)
    }

    /// Starts a progressive in-memory reader over fragmented MPEG-DASH bytes.
    ///
    /// `id` uses the same full-ID or format-token matching as
    /// [`DashManifest::representation`].
    ///
    /// The initialization fragment is fetched before returning (and establishes
    /// the GET connection). Remaining media fragments download on a GET worker
    /// (from the playhead forward, then earlier holes). When `eager_head` is set,
    /// a second connection HEADs remaining URLs to fill fragment sizes; toggle it
    /// later with [`MpegStreamReader::start_eager_head`] and
    /// [`MpegStreamReader::stop_eager_head`]. [`MpegStreamReader::seek_bytes`]
    /// may also HEAD to map offsets without downloading skipped bodies; if HEAD
    /// is unusable, it falls back to waiting on GETs. Call
    /// [`MpegStreamReader::set_symphonia_compat`] before handing the reader to
    /// a demuxer that seeks to EOF for the file length. Media timestamps use
    /// [`MpegStreamReader::seek_time_coarse`], which lands at the beginning of
    /// the containing fMP4 fragment and returns a
    /// [`crate::stream::Position`].
    pub fn stream(&self, id: impl AsRef<str>, eager_head: bool) -> Result<MpegStreamReader, Error> {
        let (fragments, timeline) = self.stream_parts(id.as_ref())?;
        MpegStreamReader::new(fragments, Some(timeline), eager_head)
    }

    /// Like [`DashManifest::stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`FragmentCache`] to share pooled connections and
    /// downloaded fragments between readers, such as a reader rebuilt after a
    /// seek.
    pub fn stream_with_cache(
        &self,
        id: impl AsRef<str>,
        eager_head: bool,
        cache: Arc<FragmentCache>,
    ) -> Result<MpegStreamReader, Error> {
        let (fragments, timeline) = self.stream_parts(id.as_ref())?;
        MpegStreamReader::new_with_cache(fragments, Some(timeline), eager_head, cache)
    }

    fn stream_parts(&self, id: &str) -> Result<(Vec<Fragment>, MediaTimeline), Error> {
        let (selected, segments) = self.select_segments(id)?;
        let fragments = selected.fragments(self.base_url.as_ref(), &segments)?;
        Ok((fragments, segments.media_timeline()))
    }

    /// Representations in document order, across all periods and adaptation sets.
    pub fn representations(&self) -> impl Iterator<Item = &Representation> {
        self.selections().map(|selected| selected.representation)
    }

    /// Finds a representation by ID.
    ///
    /// The first pass matches `id` against the full representation ID. If that
    /// misses, a second pass matches the format token before the first comma.
    /// Both comparisons are case-insensitive. The first match in document order
    /// wins.
    pub fn representation(&self, id: impl AsRef<str>) -> Option<&Representation> {
        self.find(id.as_ref())
            .map(|selected| selected.representation)
    }

    /// Initialization fragment followed by media segment fragments in timeline order.
    ///
    /// Fragments come from the representation's `SegmentTemplate` or
    /// `SegmentList` (whose entries may carry byte ranges), with URLs resolved
    /// against the `BaseURL` chain and [`DashManifest::base_url`].
    pub fn fragments(&self, id: impl AsRef<str>) -> Result<Vec<Fragment>, Error> {
        let (selected, segments) = self.select_segments(id.as_ref())?;
        selected.fragments(self.base_url.as_ref(), &segments)
    }

    /// Media-segment durations in timeline order, with the DASH timescale.
    pub fn media_timeline(&self, id: impl AsRef<str>) -> Result<MediaTimeline, Error> {
        Ok(self.select_segments(id.as_ref())?.1.media_timeline())
    }

    /// MIME type of a representation, inherited from its adaptation set if absent.
    pub fn mime_type(&self, id: impl AsRef<str>) -> Result<Option<&str>, Error> {
        Ok(self.select(id.as_ref())?.mime_type())
    }

    /// Common-encryption scheme protecting a representation, such as `cenc` or
    /// `cbcs`, inherited from its adaptation set if absent.
    pub fn protection_scheme(&self, id: impl AsRef<str>) -> Result<Option<String>, Error> {
        Ok(self.select(id.as_ref())?.protection_scheme())
    }

    fn selections(&self) -> impl Iterator<Item = Selected<'_>> {
        let mpd = &self.mpd;
        mpd.periods
            .iter()
            .enumerate()
            .flat_map(move |(index, period)| {
                let next_period = mpd.periods.get(index + 1);
                period.adaptations.iter().flat_map(move |adaptation| {
                    adaptation
                        .representations
                        .iter()
                        .map(move |representation| Selected {
                            mpd,
                            period,
                            next_period,
                            adaptation,
                            representation,
                        })
                })
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(manifest: &DashManifest, id: &str) -> Vec<String> {
        manifest
            .fragments(id)
            .unwrap()
            .into_iter()
            .map(|fragment| {
                assert_eq!(fragment.range, None);
                fragment.url
            })
            .collect()
    }

    #[test]
    fn parses_dash_manifest_fields() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC,44100,16" codecs="flac" bandwidth="1411200">
                            <BaseURL>https://media.example/audio/</BaseURL>
                            <SegmentTemplate
                                initialization="init.mp4"
                                media="segment_$Number$.m4s"
                                timescale="48000"
                                duration="192000"
                                startNumber="1"
                            />
                        </Representation>
                        <Representation id="AACLC" codecs="mp4a.40.2" bandwidth="320000">
                            <SegmentTemplate
                                initialization="aac-init.mp4"
                                media="aac-segment_$Number$.m4s"
                                timescale="44100"
                                startNumber="1"
                            />
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let manifest = DashManifest::new(xml).unwrap();
        assert_eq!(manifest.representations().count(), 2);

        let flac = manifest.representation("FLAC").unwrap();
        assert_eq!(flac.id.as_deref(), Some("FLAC,44100,16"));
        assert_eq!(flac.codecs.as_deref(), Some("flac"));
        assert_eq!(flac.bandwidth, Some(1_411_200));
        assert_eq!(flac.BaseURL[0].base, "https://media.example/audio/");
        assert_eq!(manifest.mime_type("FLAC").unwrap(), Some("audio/mp4"));

        let template = flac.SegmentTemplate.as_ref().unwrap();
        assert_eq!(template.initialization.as_deref(), Some("init.mp4"));
        assert_eq!(template.media.as_deref(), Some("segment_$Number$.m4s"));
        assert_eq!(template.timescale, Some(48_000));
        assert_eq!(template.duration, Some(192_000.0));
        assert_eq!(template.startNumber, Some(1));

        let aac = manifest.representation("AACLC").unwrap();
        assert_eq!(aac.codecs.as_deref(), Some("mp4a.40.2"));
        assert_eq!(aac.bandwidth, Some(320_000));
        assert!(matches!(
            manifest.fragments("FLAC"),
            Err(Error::DashManifestMissingDuration)
        ));
        assert!(matches!(
            manifest.fragments("AACLC"),
            Err(Error::DashManifestMissingTimeline)
        ));
        assert!(matches!(
            manifest.media_timeline("AACLC"),
            Err(Error::DashManifestMissingTimeline)
        ));
    }

    #[test]
    fn parses_tidal_segment_timeline_and_unescapes_urls() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="AACLC" codecs="mp4a.40.2" bandwidth="321744">
                            <SegmentTemplate
                                timescale="44100"
                                initialization="https://cdn.example/0.mp4?token=abc&amp;info=init"
                                media="https://cdn.example/$Number$.mp4?token=abc&amp;info=media"
                                startNumber="1"
                            >
                                <SegmentTimeline>
                                    <S d="176128" r="2"/>
                                    <S d="108735"/>
                                </SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let manifest = DashManifest::new(xml).unwrap();
        assert_eq!(
            urls(&manifest, "AACLC"),
            [
                "https://cdn.example/0.mp4?token=abc&info=init",
                "https://cdn.example/1.mp4?token=abc&info=media",
                "https://cdn.example/2.mp4?token=abc&info=media",
                "https://cdn.example/3.mp4?token=abc&info=media",
                "https://cdn.example/4.mp4?token=abc&info=media",
            ]
        );
        assert_eq!(
            manifest.media_timeline("AACLC").unwrap(),
            MediaTimeline {
                timescale: 44_100,
                media_durations: vec![176_128, 176_128, 176_128, 108_735],
            }
        );
    }

    #[test]
    fn inherits_segment_template_from_adaptation_set() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <SegmentTemplate timescale="1000" initialization="init.mp4" media="$Number$.mp4">
                            <SegmentTimeline><S d="2000" r="1"/></SegmentTimeline>
                        </SegmentTemplate>
                        <Representation id="FLAC" codecs="flac">
                            <SegmentTemplate startNumber="5"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_base_url("https://cdn.example/track/manifest.mpd")
            .unwrap();
        assert_eq!(
            urls(&manifest, "FLAC"),
            [
                "https://cdn.example/track/init.mp4",
                "https://cdn.example/track/5.mp4",
                "https://cdn.example/track/6.mp4",
            ]
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap(),
            MediaTimeline {
                timescale: 1000,
                media_durations: vec![2000, 2000],
            }
        );
    }

    #[test]
    fn inherits_common_encryption_scheme_from_adaptation_set() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <ContentProtection
                            schemeIdUri="urn:mpeg:dash:mp4protection:2011"
                            value="cbcs"
                        />
                        <Representation id="FLAC" codecs="flac">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.mp4">
                                <SegmentTimeline><S d="1000"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let manifest = DashManifest::new(xml).unwrap();
        assert_eq!(
            manifest.protection_scheme("FLAC").unwrap().as_deref(),
            Some("cbcs")
        );
    }

    #[test]
    fn stream_rejects_unknown_format() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="AACLC" codecs="mp4a.40.2">
                            <SegmentTemplate
                                initialization="init.mp4"
                                media="segment_$Number$.m4s"
                                startNumber="1"
                            >
                                <SegmentTimeline>
                                    <S d="1000"/>
                                </SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml).unwrap();
        let error = manifest.stream("FLAC", true).unwrap_err();
        assert!(matches!(
            error,
            Error::DashManifestMissingRepresentation(id) if id == "FLAC"
        ));
    }

    #[test]
    fn representation_prefers_full_id_then_format_token() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC_HIRES,48000,24" codecs="flac" bandwidth="2304000">
                            <SegmentTemplate initialization="hires-init.mp4" media="hires_$Number$.m4s"/>
                        </Representation>
                        <Representation id="FLAC,44100,16" codecs="flac" bandwidth="1411200">
                            <SegmentTemplate initialization="init.mp4" media="segment_$Number$.m4s"/>
                        </Representation>
                        <Representation id="AACLC" codecs="mp4a.40.2" bandwidth="320000">
                            <SegmentTemplate initialization="aac-init.mp4" media="aac_$Number$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml).unwrap();
        let id_of = |id: &str| manifest.representation(id).unwrap().id.as_deref().unwrap();

        assert_eq!(id_of("FLAC_HIRES,48000,24"), "FLAC_HIRES,48000,24");
        assert_eq!(id_of("flac,44100,16"), "FLAC,44100,16");
        assert_eq!(id_of("FLAC"), "FLAC,44100,16");
        assert_eq!(id_of("flac_hires"), "FLAC_HIRES,48000,24");
        assert_eq!(id_of("aaclc"), "AACLC");
        assert!(manifest.representation("MP3").is_none());
    }

    #[test]
    fn rejects_dash_manifest_without_representations() {
        assert!(matches!(
            DashManifest::new("<MPD/>"),
            Err(Error::DashManifestMissingRepresentations)
        ));
    }

    #[test]
    fn rejects_representation_without_id() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation codecs="flac">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        assert!(matches!(
            DashManifest::new(xml),
            Err(Error::DashManifestMissingRepresentationId)
        ));
    }

    #[test]
    fn rejects_malformed_xml() {
        assert!(matches!(
            DashManifest::new("<MPD><Period>"),
            Err(Error::DashManifestParse(_))
        ));
    }

    #[test]
    fn parses_sample_tidal_manifest() {
        let manifest = DashManifest::new(include_str!("../test_files/manifest.xml")).unwrap();
        assert_eq!(urls(&manifest, "FLAC").len(), 86);
        assert_eq!(manifest.mime_type("FLAC").unwrap(), Some("audio/mp4"));
        assert_eq!(manifest.protection_scheme("FLAC").unwrap(), None);
    }

    #[test]
    fn new_from_data_url_decodes_and_parses() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="AACLC" codecs="mp4a.40.2">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let encoded = STANDARD.encode(xml);
        let manifest =
            DashManifest::new_from_data_url(&format!("data:application/dash+xml;base64,{encoded}"))
                .unwrap();
        assert!(manifest.representation("AACLC").is_some());
    }

    #[test]
    fn new_from_data_url_rejects_non_data_url() {
        assert!(matches!(
            DashManifest::new_from_data_url("https://example.com/manifest.mpd"),
            Err(Error::InvalidDataUrl)
        ));
    }

    #[test]
    fn resolves_base_url_chain_and_duration_template() {
        let xml = r#"
            <MPD mediaPresentationDuration="PT9S">
                <BaseURL>../media/</BaseURL>
                <Period>
                    <BaseURL>audio/</BaseURL>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac" bandwidth="1411200">
                            <BaseURL>flac/</BaseURL>
                            <SegmentTemplate
                                timescale="1000"
                                duration="4000"
                                initialization="$RepresentationID$-init.mp4"
                                media="$Bandwidth$/$Number%03d$.m4s"
                            />
                        </Representation>
                        <Representation id="AACLC" codecs="mp4a.40.2">
                            <BaseURL>https://other.example/aac/</BaseURL>
                            <SegmentTemplate timescale="1000" duration="9000" initialization="i.mp4" media="$Time$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_base_url("https://cdn.example/manifests/track.mpd?sig=1")
            .unwrap();
        assert_eq!(
            manifest.base_url(),
            Some("https://cdn.example/manifests/track.mpd?sig=1")
        );
        assert_eq!(
            urls(&manifest, "FLAC"),
            [
                "https://cdn.example/media/audio/flac/FLAC-init.mp4",
                "https://cdn.example/media/audio/flac/1411200/001.m4s",
                "https://cdn.example/media/audio/flac/1411200/002.m4s",
                "https://cdn.example/media/audio/flac/1411200/003.m4s",
            ]
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap(),
            MediaTimeline {
                timescale: 1000,
                media_durations: vec![4000, 4000, 1000],
            }
        );
        assert_eq!(
            urls(&manifest, "AACLC"),
            [
                "https://other.example/aac/i.mp4",
                "https://other.example/aac/0.m4s"
            ]
        );
    }

    #[test]
    fn relative_urls_need_a_base_url() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.mp4">
                                <SegmentTimeline><S d="1000"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml).unwrap();
        assert!(matches!(
            manifest.fragments("FLAC"),
            Err(Error::DashManifestUrl(url, url::ParseError::RelativeUrlWithoutBase)) if url == "init.mp4"
        ));
        assert!(matches!(
            manifest.with_base_url("manifest.mpd"),
            Err(Error::DashManifestUrl(
                _,
                url::ParseError::RelativeUrlWithoutBase
            ))
        ));
    }

    #[test]
    fn period_duration_runs_to_next_period_start() {
        let xml = r#"
            <MPD mediaPresentationDuration="PT20S">
                <Period start="PT2S">
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FIRST" codecs="flac">
                            <SegmentTemplate timescale="1" initialization="https://cdn.example/i.mp4"
                                media="https://cdn.example/$Time$.mp4">
                                <SegmentTimeline><S d="2" r="-1"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
                <Period start="PT7S">
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="SECOND" codecs="flac">
                            <SegmentTemplate timescale="1" duration="10" initialization="https://cdn.example/i.mp4"
                                media="https://cdn.example/$Number$.mp4"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml).unwrap();
        assert_eq!(
            manifest.media_timeline("FIRST").unwrap().media_durations,
            [2, 2, 2]
        );
        assert_eq!(
            manifest.media_timeline("SECOND").unwrap().media_durations,
            [10, 3]
        );
    }

    #[test]
    fn new_from_url_resolves_against_redirected_url() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let xml = r#"<MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac">
                        <SegmentTemplate initialization="init.mp4" media="$Number$.mp4">
                            <SegmentTimeline><S d="1000"/></SegmentTimeline>
                        </SegmentTemplate>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>"#;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    header.clear();
                }
                let response = if request_line.starts_with("GET /start ") {
                    "HTTP/1.1 302 Found\r\nLocation: /tracks/7/manifest.mpd\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/dash+xml\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{xml}",
                        xml.len()
                    )
                };
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });

        let manifest = DashManifest::new_from_url(&format!("http://{address}/start")).unwrap();
        server.join().unwrap();
        assert_eq!(
            manifest.base_url(),
            Some(format!("http://{address}/tracks/7/manifest.mpd").as_str())
        );
        assert_eq!(
            urls(&manifest, "FLAC"),
            [
                format!("http://{address}/tracks/7/init.mp4"),
                format!("http://{address}/tracks/7/1.mp4"),
            ]
        );
    }

    #[test]
    fn new_from_url_accepts_data_urls() {
        let xml = r#"<MPD><Period><AdaptationSet><Representation id="AACLC"/></AdaptationSet></Period></MPD>"#;
        let encoded = STANDARD.encode(xml);
        let manifest =
            DashManifest::new_from_url(&format!("data:application/dash+xml;base64,{encoded}"))
                .unwrap();
        assert!(manifest.representation("AACLC").is_some());
        assert_eq!(manifest.base_url(), None);
    }

    fn ranged(url: &str, start: u64, end: u64) -> Fragment {
        Fragment::with_range(url, start..=end)
    }

    #[test]
    fn segment_list_media_ranges_address_one_file() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac">
                            <BaseURL>track.mp4</BaseURL>
                            <SegmentList timescale="1000" duration="4000">
                                <Initialization range="0-837"/>
                                <SegmentURL mediaRange="838-50000"/>
                                <SegmentURL mediaRange="50001-90000"/>
                            </SegmentList>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_base_url("https://cdn.example/audio/manifest.mpd")
            .unwrap();
        assert_eq!(
            manifest.fragments("FLAC").unwrap(),
            [
                ranged("https://cdn.example/audio/track.mp4", 0, 837),
                ranged("https://cdn.example/audio/track.mp4", 838, 50_000),
                ranged("https://cdn.example/audio/track.mp4", 50_001, 90_000),
            ]
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap(),
            MediaTimeline {
                timescale: 1000,
                media_durations: vec![4000, 4000],
            }
        );
    }

    #[test]
    fn segment_list_inherits_initialization_and_timeline() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <SegmentList timescale="100">
                            <Initialization sourceURL="init.mp4"/>
                            <SegmentTimeline><S d="200" r="1"/><S d="50"/></SegmentTimeline>
                        </SegmentList>
                        <Representation id="AACLC" codecs="mp4a.40.2">
                            <SegmentList>
                                <SegmentURL media="a.m4s"/>
                                <SegmentURL media="b.m4s"/>
                                <SegmentURL media="c.m4s"/>
                            </SegmentList>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_base_url("https://cdn.example/aac/")
            .unwrap();
        assert_eq!(
            urls(&manifest, "AACLC"),
            [
                "https://cdn.example/aac/init.mp4",
                "https://cdn.example/aac/a.m4s",
                "https://cdn.example/aac/b.m4s",
                "https://cdn.example/aac/c.m4s",
            ]
        );
        assert_eq!(
            manifest.media_timeline("AACLC").unwrap().media_durations,
            [200, 200, 50]
        );
    }

    #[test]
    fn segment_list_drops_urls_past_the_period() {
        let xml = r#"
            <MPD mediaPresentationDuration="PT5S">
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac">
                            <SegmentList timescale="1" duration="2">
                                <Initialization sourceURL="https://cdn.example/init.mp4"/>
                                <SegmentURL media="https://cdn.example/1.m4s"/>
                                <SegmentURL media="https://cdn.example/2.m4s"/>
                                <SegmentURL media="https://cdn.example/3.m4s"/>
                                <SegmentURL media="https://cdn.example/4.m4s"/>
                            </SegmentList>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml).unwrap();
        assert_eq!(
            urls(&manifest, "FLAC"),
            [
                "https://cdn.example/init.mp4",
                "https://cdn.example/1.m4s",
                "https://cdn.example/2.m4s",
                "https://cdn.example/3.m4s",
            ]
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap().media_durations,
            [2, 2, 1]
        );
    }

    #[test]
    fn rejects_invalid_segment_list() {
        let manifest = |list: &str| {
            DashManifest::new(format!(
                r#"<MPD><Period><AdaptationSet><Representation id="FLAC">{list}</Representation></AdaptationSet></Period></MPD>"#
            ))
            .unwrap()
            .with_base_url("https://cdn.example/")
            .unwrap()
        };
        assert!(matches!(
            manifest(r#"<SegmentList duration="1"><Initialization/></SegmentList>"#)
                .fragments("FLAC"),
            Err(Error::DashManifestInvalidSegments(_))
        ));
        assert!(matches!(
            manifest(r#"<SegmentList duration="1"><SegmentURL media="a.m4s"/></SegmentList>"#)
                .fragments("FLAC"),
            Err(Error::DashManifestMissingUrls)
        ));
        assert!(matches!(
            manifest(
                r#"<SegmentList duration="1"><Initialization/><SegmentURL mediaRange="9-1"/></SegmentList>"#
            )
            .fragments("FLAC"),
            Err(Error::InvalidByteRange(range)) if range == "9-1"
        ));
    }

    #[test]
    fn segment_template_falls_back_to_initialization_element() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac">
                            <BaseURL>https://cdn.example/track.mp4</BaseURL>
                            <SegmentTemplate media="$Number$.m4s">
                                <Initialization range="0-99"/>
                                <SegmentTimeline><S d="1"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml).unwrap();
        assert_eq!(
            manifest.fragments("FLAC").unwrap(),
            [
                ranged("https://cdn.example/track.mp4", 0, 99),
                Fragment::new("https://cdn.example/1.m4s"),
            ]
        );
    }

    /// Serves byte ranges of one in-memory file and records each request.
    struct OneFile {
        body: Vec<u8>,
        requests: std::sync::Mutex<Vec<Fragment>>,
    }

    impl crate::Transport for OneFile {
        fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<Arc<[u8]>, Error> {
            self.requests.lock().unwrap().push(fragment.clone());
            let range = fragment
                .range
                .clone()
                .unwrap_or(0..=self.body.len() as u64 - 1);
            Ok(self.body[*range.start() as usize..=*range.end() as usize].into())
        }
    }

    /// An 838-byte initialization, a `sidx` for two subsegments, then their media.
    fn segment_base_file() -> (Vec<u8>, String) {
        let sidx =
            crate::manifest_segment_base::tests::sidx(0, 44_100, 0, &[(100, 4096), (50, 1024)]);
        let index_range = format!("838-{}", 838 + sidx.len() - 1);
        let mut body = vec![b'i'; 838];
        body.extend(sidx);
        body.extend([b'a'; 100]);
        body.extend([b'b'; 50]);
        (body, index_range)
    }

    fn segment_base_manifest(segment_base: &str) -> DashManifest {
        let manifest = DashManifest::new(format!(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation id="FLAC">
                <BaseURL>track.mp4</BaseURL>{segment_base}
            </Representation></AdaptationSet></Period></MPD>"#
        ))
        .unwrap();
        // The relative BaseURL defers the index fetch until a base URL is known.
        assert!(matches!(
            manifest.fragments("FLAC"),
            Err(Error::DashManifestInvalidSegments(reason)) if reason.contains("not loaded")
        ));
        manifest
    }

    #[test]
    fn segment_base_index_becomes_ranged_fragments() {
        use std::io::Read;

        let (body, index_range) = segment_base_file();
        let index_start = *parse_range(&index_range).start();
        let index_end = *parse_range(&index_range).end();
        let mut manifest =
            segment_base_manifest(&format!(r#"<SegmentBase indexRange="{index_range}"/>"#));
        manifest.base_url = Some(Url::parse("https://cdn.example/audio/").unwrap());
        let transport = Arc::new(OneFile {
            body: body.clone(),
            requests: Default::default(),
        });
        manifest.load_segment_indexes_with(transport.as_ref());

        let url = "https://cdn.example/audio/track.mp4";
        assert_eq!(
            *transport.requests.lock().unwrap(),
            [ranged(url, index_start, index_end)]
        );
        // Without an Initialization element, everything before the index is the init.
        assert_eq!(
            manifest.fragments("FLAC").unwrap(),
            [
                ranged(url, 0, 837),
                ranged(url, index_end + 1, index_end + 100),
                ranged(url, index_end + 101, index_end + 150),
            ]
        );
        assert_eq!(
            manifest.media_timeline("FLAC").unwrap(),
            MediaTimeline {
                timescale: 44_100,
                media_durations: vec![4096, 1024],
            }
        );

        let mut reader = manifest
            .stream_with_cache("FLAC", false, Arc::new(FragmentCache::new(transport)))
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        let mut expected = body[..838].to_vec();
        expected.extend(&body[index_end as usize + 1..]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn segment_base_keeps_its_initialization_element() {
        let (body, index_range) = segment_base_file();
        let mut manifest = segment_base_manifest(&format!(
            r#"<SegmentBase indexRange="{index_range}"><Initialization range="0-99"/></SegmentBase>"#
        ));
        manifest.base_url = Some(Url::parse("https://cdn.example/").unwrap());
        manifest.load_segment_indexes_with(&OneFile {
            body,
            requests: Default::default(),
        });
        assert_eq!(
            manifest.fragments("FLAC").unwrap()[0],
            ranged("https://cdn.example/track.mp4", 0, 99)
        );
    }

    #[test]
    fn failed_segment_base_index_only_fails_its_representation() {
        let mut manifest = DashManifest::new(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
                <Representation id="FLAC">
                    <BaseURL>track.mp4</BaseURL><SegmentBase indexRange="0-9"/>
                </Representation>
                <Representation id="AACLC">
                    <SegmentTemplate initialization="init" media="$Number$">
                        <SegmentTimeline><S d="1"/></SegmentTimeline>
                    </SegmentTemplate>
                </Representation>
            </AdaptationSet></Period></MPD>"#,
        )
        .unwrap();
        manifest.base_url = Some(Url::parse("https://cdn.example/").unwrap());
        let transport = OneFile {
            body: vec![0; 10],
            requests: Default::default(),
        };
        manifest.load_segment_indexes_with(&transport);

        let error = manifest.fragments("FLAC").unwrap_err();
        assert!(matches!(
            &error,
            Error::DashManifestSegmentIndex { representation, reason }
                if representation == "FLAC" && reason.contains("no sidx box")
        ));
        assert!(matches!(
            manifest.media_timeline("FLAC"),
            Err(Error::DashManifestSegmentIndex { .. })
        ));
        assert_eq!(manifest.mime_type("FLAC").unwrap(), Some("audio/mp4"));
        assert_eq!(manifest.fragments("AACLC").unwrap().len(), 2);

        // A later successful load clears the failure.
        let (body, index_range) = segment_base_file();
        manifest.mpd.periods[0].adaptations[0].representations[0]
            .SegmentBase
            .as_mut()
            .unwrap()
            .indexRange = Some(index_range);
        manifest.load_segment_indexes_with(&OneFile {
            body,
            requests: Default::default(),
        });
        assert_eq!(manifest.fragments("FLAC").unwrap().len(), 3);
    }

    fn parse_range(value: &str) -> std::ops::RangeInclusive<u64> {
        crate::stream::parse_byte_range(value).unwrap()
    }

    #[test]
    fn stream_with_cache_shares_cached_fragments() {
        use crate::Transport;
        use std::io::Read;
        use std::sync::Mutex;

        /// Serves each URL's last path segment as its body and records requests.
        #[derive(Default)]
        struct Recording(Mutex<Vec<String>>);

        impl Transport for Recording {
            fn get(
                &self,
                fragment: &Fragment,
                _size_hint: Option<u64>,
            ) -> Result<Arc<[u8]>, Error> {
                self.0.lock().unwrap().push(fragment.url.clone());
                Ok(fragment.url.rsplit('/').next().unwrap().as_bytes().into())
            }
        }

        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac">
                            <SegmentTemplate initialization="init" media="$Number$">
                                <SegmentTimeline><S d="1" r="1"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_base_url("https://cdn.example/")
            .unwrap();
        let transport = Arc::new(Recording::default());
        let cache = Arc::new(FragmentCache::new(transport.clone()));
        for _ in 0..2 {
            let mut reader = manifest
                .stream_with_cache("FLAC", false, cache.clone())
                .unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"init12");
        }
        assert_eq!(
            *transport.0.lock().unwrap(),
            [
                "https://cdn.example/init",
                "https://cdn.example/1",
                "https://cdn.example/2",
            ]
        );
    }
}
