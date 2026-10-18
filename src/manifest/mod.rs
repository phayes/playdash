//! MPEG-DASH manifests: loading a stream and choosing its audio or video.

#[cfg(feature = "sync")]
mod fetch;
pub(crate) mod live;
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
use crate::segment::index::{LiveWindow, MediaIndex};
use crate::segment::{MediaTiming, SegmentLocation, SegmentNumber, Segments};
use crate::vendor::dash_mpd_core::{
    AdaptationSet, BaseURL, ContentProtection, MPD, Period, Representation as CoreRepresentation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
pub use live::LiveDelay;
use live::{Live, LivePresentation};
pub use representation::{ContentKind, FrameRate, Representation};
use segment_list::ListChain;
use segments::SegmentClock;
pub(crate) use segments::{FixedSegments, TimelineSegments};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
pub(crate) use template::MediaUrls;
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
#[derive(Clone, Debug)]
pub struct DashManifest {
    mpd: MPD,
    /// Shared refresh state for a live manifest; `None` for a static manifest.
    live: Option<Arc<Live>>,
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

    /// Returns how long before its end a live segment may be fetched.
    /// `@availabilityTimeOffset` from the most specific `SegmentTemplate` that
    /// sets it, else from the most specific `BaseURL`, else zero.
    ///
    /// Returns [`Error::DashManifestUnsupportedLive`] for negative or infinite
    /// offsets, which require unsupported low-latency behavior.
    fn availability_time_offset(self) -> Result<Duration, Error> {
        let base_urls = [
            self.representation.BaseURL.first(),
            self.adaptation.BaseURL.first(),
            self.period.BaseURL.first(),
            self.mpd.base_url.first(),
        ];
        let offset = self
            .templates()
            .get(|template| template.availabilityTimeOffset)
            .or_else(|| {
                base_urls
                    .into_iter()
                    .flatten()
                    .find_map(|base| base.availability_time_offset)
            });
        match offset {
            None => Ok(Duration::ZERO),
            Some(offset) => Duration::try_from_secs_f64(offset).map_err(|_| {
                Error::DashManifestUnsupportedLive(format!(
                    "@availabilityTimeOffset {offset} is not a finite, non-negative number of seconds"
                ))
            }),
        }
    }

    /// Builds a static representation's segments, resolving its `BaseURL` chain.
    fn segments(self, manifest_url: Option<&Url>) -> Result<Segments, Error> {
        if self.needs_segment_index() {
            return Err(Error::DashManifestInvalidSegments(
                "SegmentBase index not loaded; load it with load_segment_indexes".to_owned(),
            ));
        }
        if self.uses_segment_list() {
            return self.list_segments(manifest_url);
        }
        let (init, index) = self.template_index(manifest_url, None)?;
        Ok(Segments::indexed(init, index))
    }

    /// The segments of the representation's `SegmentList`.
    fn list_segments(self, manifest_url: Option<&Url>) -> Result<Segments, Error> {
        let mut list = self.lists().segments(self.period_duration())?;

        // Resolve every URL against the BaseURL chain.
        let base = self.base_url(manifest_url)?;
        let resolve = |location: &mut SegmentLocation| -> Result<(), Error> {
            location.url = resolve_url(base.as_ref(), &location.url)?.into();
            Ok(())
        };
        resolve(&mut list.init)?;
        for segment in &mut list.media {
            resolve(&mut segment.location)?;
        }

        let timing = self.timing(list.timescale, list.presentation_time_offset);
        Segments::timed(list.init, timing, list.media)
    }

    /// Builds the initialization segment and media index from `SegmentTemplate`.
    /// If `live` is present, the index also includes wall-clock availability.
    ///
    /// Fails with [`Error::DashManifestUnsupportedLive`] when the
    /// representation is addressed some other way.
    fn template_index(
        self,
        manifest_url: Option<&Url>,
        live: Option<&LivePresentation>,
    ) -> Result<(SegmentLocation, MediaIndex), Error> {
        let other = match (self.needs_segment_index(), self.uses_segment_list()) {
            (true, _) => Some("SegmentBase"),
            (false, true) => Some("SegmentList"),
            (false, false) => None,
        };
        if let Some(addressing) = other {
            return Err(Error::DashManifestUnsupportedLive(format!(
                "representation {} uses {addressing} addressing; only SegmentTemplate plays live",
                self.id()
            )));
        }
        let addressed = self
            .templates()
            .addressed(self.period_duration(), live.is_some())?;
        let values = TemplateValues {
            representation_id: self.id(),
            bandwidth: self.representation.bandwidth,
            ..TemplateValues::default()
        };
        let init = self.templates().init_location(values)?;
        let media = self.templates().media_template(values)?;

        // Resolve the URLs against the BaseURL chain.
        let base = self.base_url(manifest_url)?;
        let init = SegmentLocation {
            url: resolve_url(base.as_ref(), &init.url)?.into(),
            ..init
        };
        let urls = MediaUrls::new(media, base);

        // Every media segment's URL must resolve, and each segment follow the one ahead.
        urls.check()?;
        addressed
            .segments
            .check_order(|number, time| urls.location(number, time))?;

        // Preserve MPD segment numbers for live refresh alignment; static indexes use zero.
        let window = live.map(|live| self.live_window(live)).transpose()?;
        let first = |number: Option<u64>| match window {
            Some(_) => SegmentNumber(number.unwrap_or(0)),
            None => SegmentNumber(0),
        };
        let timing = self.timing(addressed.timescale, addressed.presentation_time_offset);
        let index = match addressed.segments {
            SegmentClock::Fixed(segments) => MediaIndex::Template {
                timing,
                first: first(Some(segments.start_number())),
                segments,
                urls,
                live: window,
            },
            SegmentClock::Timeline(segments) => MediaIndex::Timeline {
                timing,
                first: first(segments.number(0)),
                segments,
                urls,
                live: window,
            },
        };
        Ok((init, index))
    }

    /// Builds the wall-clock availability window for this representation.
    fn live_window(self, live: &LivePresentation) -> Result<LiveWindow, Error> {
        Ok(LiveWindow {
            availability_start: live.availability_start,
            time_shift_buffer_depth: live.time_shift_buffer_depth,
            availability_time_offset: self.availability_time_offset()?,
        })
    }

    /// The media clock of segments at `timescale` whose media time
    /// `presentation_time_offset` presents at the start of the Period.
    fn timing(self, timescale: NonZeroU32, presentation_time_offset: u64) -> MediaTiming {
        MediaTiming {
            timescale,
            presentation_time_offset,
            period_start: self.period.start.unwrap_or_default(),
        }
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
    ///
    /// A live manifest (`MPD@type="dynamic"`) must have an
    /// `MPD@availabilityStartTime`, or this fails with
    /// [`Error::DashManifestMissingAvailabilityStart`].
    pub fn new(dash_xml: impl AsRef<str>) -> Result<Self, Error> {
        let mpd = crate::vendor::dash_mpd_core::parse(dash_xml.as_ref())?;
        let live = LivePresentation::of(&mpd)?
            .map(|presentation| Arc::new(Live::new(mpd.clone(), presentation, None)));
        let manifest = Self {
            live,
            mpd,
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
        let url = resolve_url(None, url)?;
        if let Some(live) = &self.live {
            live.set_base_url(url.clone());
        }
        self.base_url = Some(url);
        Ok(self)
    }

    /// Returns the absolute source URL used to resolve relative media references.
    /// Returns `None` when the manifest was parsed without one.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_ref().map(Url::as_str)
    }

    /// Returns `true` for a live manifest (`MPD@type="dynamic"`).
    pub fn is_live(&self) -> bool {
        self.live.is_some()
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
    ///
    /// For a live manifest, this returns the representation described by the
    /// latest refresh. See [`Segments::is_live`].
    pub fn segments(&self, representation_id: impl AsRef<str>) -> Result<Segments, Error> {
        let selected = self.select_indexed(representation_id.as_ref())?;
        match &self.live {
            Some(live) => Ok(live.segments(selected.id())?.0),
            None => selected.segments(self.base_url.as_ref()),
        }
    }

    /// Returns the shared live-refresh state, if this manifest is live.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn live(&self) -> Option<&Arc<Live>> {
        self.live.as_ref()
    }

    /// Replaces the clock used by live timing calculations in tests.
    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: Arc<dyn crate::segment::clock::Clock>) -> Self {
        self.live = LivePresentation::of(&self.mpd)
            .unwrap()
            .map(|presentation| {
                Arc::new(Live::with_clock(
                    self.mpd.clone(),
                    presentation,
                    self.base_url.clone(),
                    clock,
                ))
            });
        self
    }

    /// Builds a fresh media index for representation `id` in tests.
    #[cfg(test)]
    pub(crate) fn index_for_test(&self, id: &str) -> MediaIndex {
        let presentation = LivePresentation::of(&self.mpd).unwrap();
        let selected = self.select(id).unwrap();
        let (_, index) = selected
            .template_index(self.base_url.as_ref(), presentation.as_ref())
            .unwrap();
        index
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
        selections(&self.mpd)
    }

    fn find(&self, id: &str) -> Option<Selected<'_>> {
        find(&self.mpd, id)
    }

    /// A representation, or the error that kept its `SegmentBase` index from
    /// loading.
    fn select_indexed(&self, id: &str) -> Result<Selected<'_>, Error> {
        let selected = self.select(id)?;
        if selected.needs_segment_index()
            && let Some(reason) = self.segment_index_errors.get(selected.id())
        {
            return Err(Error::DashManifestSegmentIndex {
                representation: selected.id().to_owned(),
                reason: reason.clone(),
            });
        }
        Ok(selected)
    }

    fn select(&self, id: &str) -> Result<Selected<'_>, Error> {
        self.find(id)
            .ok_or_else(|| Error::DashManifestMissingRepresentation(id.to_owned()))
    }
}

/// Iterates over every representation in MPD document order.
fn selections(mpd: &MPD) -> impl Iterator<Item = Selected<'_>> {
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

/// Finds the representation named by `id` using [`DashManifest::representation`] matching rules.
fn find<'a>(mpd: &'a MPD, id: &str) -> Option<Selected<'a>> {
    selections(mpd)
        .find(|selected| selected.id().eq_ignore_ascii_case(id))
        .or_else(|| {
            selections(mpd)
                .find(|selected| representation_token(selected.id()).eq_ignore_ascii_case(id))
        })
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
