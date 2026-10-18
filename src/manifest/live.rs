//! Support for live MPEG-DASH presentations (`MPD@type="dynamic"`).
//!
//! This module maps presentation time to wall-clock time, determines which
//! segments can be fetched, and shares manifest refreshes between streams
//! opened from the same [`DashManifest`].

// Suppress unused-code warnings when neither networking implementation is enabled.
#![cfg_attr(not(any(feature = "sync", feature = "async")), allow(dead_code))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use url::Url;

#[cfg(any(feature = "sync", feature = "async"))]
use super::DashManifest;
use super::{find, resolve_url};
use crate::error::Error;
#[cfg(any(feature = "sync", feature = "async"))]
use crate::segment::FetchedManifest;
#[cfg(any(feature = "sync", feature = "async"))]
use crate::segment::SegmentNumber;
use crate::segment::Segments;
use crate::segment::clock::{Clock, SystemClock};
use crate::segment::index::MediaIndex;
#[cfg(any(feature = "sync", feature = "async"))]
use crate::segment::schedule::{Idle, start_at};
use crate::vendor::dash_mpd_core::MPD;

/// Refresh interval used when `MPD@minimumUpdatePeriod` is zero.
const ZERO_UPDATE_PERIOD: Duration = Duration::from_secs(5);

/// Minimum delay before an out-of-segments stream may request an early refresh.
const RUN_OUT_REFRESH: Duration = Duration::from_secs(1);

/// Number of consecutive stale refreshes tolerated before playback fails.
const STALE_RETRIES: u32 = 3;

/// Increase in delay after each stale refresh, capped by [`STALE_RETRY_CAP`].
const STALE_RETRY_STEP: Duration = Duration::from_secs(1);

/// Maximum delay before retrying a stale manifest refresh.
const STALE_RETRY_CAP: Duration = Duration::from_secs(5);

/// Selects how far behind the live edge a stream begins playback.
///
/// The live edge is the end time of the newest available media segment.
///
/// Set it with `MediaStreamBuilder::live_delay`. Without one, a stream
/// uses `MPD@suggestedPresentationDelay`, falling back to four segments.
/// A delay is clamped to the available time-shift window and has no effect
/// on a static manifest.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use playdash::LiveDelay;
///
/// let low_latency = LiveDelay::LiveEdge;
/// let safe = LiveDelay::Time(Duration::from_secs(30));
/// assert_ne!(low_latency, safe);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveDelay {
    /// Starts in the newest available media segment.
    LiveEdge,
    /// Starts by the given amount of presentation time behind the live edge.
    Time(Duration),
    /// Starts approximately this many segments behind the live edge.
    ///
    /// The delay is calculated using the representation's longest segment.
    Segments(u32),
}

impl LiveDelay {
    /// Default used when neither the caller nor the manifest specifies a delay.
    const FALLBACK: Self = Self::Segments(4);
}

/// Live timing and refresh settings parsed from the MPD element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LivePresentation {
    /// Wall-clock instant corresponding to presentation time zero
    /// (`MPD@availabilityStartTime`).
    pub(crate) availability_start: SystemTime,
    /// When this copy of the manifest was generated: `MPD@publishTime`.
    pub(crate) publish_time: Option<SystemTime>,
    /// Recommended interval between manifest fetches
    /// (`MPD@minimumUpdatePeriod`). `None` means no periodic refresh.
    pub(crate) minimum_update_period: Option<Duration>,
    /// Duration of the seekable window behind the live edge
    /// (`MPD@timeShiftBufferDepth`). `None` retains the full presentation.
    pub(crate) time_shift_buffer_depth: Option<Duration>,
    /// How far behind the live edge the publisher suggests playing:
    /// `MPD@suggestedPresentationDelay`.
    pub(crate) suggested_presentation_delay: Option<Duration>,
    /// The longest media segment of any representation: `MPD@maxSegmentDuration`.
    pub(crate) max_segment_duration: Option<Duration>,
    /// Unresolved URL in the first `Location` element, if present.
    pub(crate) location: Option<String>,
}

impl LivePresentation {
    /// Parses live timing from `mpd`, returning `None` for a static manifest.
    ///
    /// Fails with [`Error::DashManifestMissingAvailabilityStart`] for a live
    /// manifest without `MPD@availabilityStartTime`.
    pub(crate) fn of(mpd: &MPD) -> Result<Option<Self>, Error> {
        let dynamic = mpd
            .mpdtype
            .as_deref()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("dynamic"));
        if !dynamic {
            return Ok(None);
        }
        let availability_start = mpd
            .availabilityStartTime
            .ok_or(Error::DashManifestMissingAvailabilityStart)?;
        Ok(Some(Self {
            availability_start: availability_start.into(),
            publish_time: mpd.publishTime.map(Into::into),
            minimum_update_period: mpd.minimumUpdatePeriod,
            time_shift_buffer_depth: mpd.timeShiftBufferDepth,
            suggested_presentation_delay: mpd.suggestedPresentationDelay,
            max_segment_duration: mpd.maxSegmentDuration,
            location: mpd
                .locations
                .first()
                .map(|location| location.url.trim().to_owned())
                .filter(|url| !url.is_empty()),
        }))
    }
}

/// Shared state for a live presentation and its current manifest.
///
/// All streams opened from one [`DashManifest`] use this state, so at most one
/// refresh is fetched at a time. Segment numbers remain stable across refreshes.
#[derive(Debug)]
pub(crate) struct Live {
    clock: Arc<dyn Clock>,
    state: Mutex<LiveState>,
    /// Increments whenever a newer manifest is installed.
    generation: AtomicU64,
    /// Serializes blocking refreshes.
    #[cfg(feature = "sync")]
    blocking_refresh: Mutex<()>,
    /// Serializes asynchronous refreshes.
    #[cfg(feature = "async")]
    async_refresh: futures_util::lock::Mutex<()>,
}

/// Current manifest data and refresh status guarded by [`Live::state`].
#[derive(Debug)]
struct LiveState {
    mpd: MPD,
    /// The live timing; `None` once the manifest has turned static.
    presentation: Option<LivePresentation>,
    /// The URL the manifest came from, which relative URLs resolve against.
    base_url: Option<Url>,
    /// The media index of each representation opened so far, by ID, or why
    /// it could not be refreshed.
    indexes: HashMap<String, Result<Arc<MediaIndex>, String>>,
    /// When the manifest was fetched.
    fetched_at: SystemTime,
    /// Refreshes in a row that returned an older manifest.
    stale_attempts: u32,
    /// When to retry after a stale refresh.
    retry_at: Option<SystemTime>,
    /// Permanent refresh failure reported to all streams.
    failure: Option<String>,
}

impl Live {
    /// Creates shared live state for an MPD fetched now.
    ///
    /// Relative manifest and segment URLs are resolved against `base_url`.
    pub(crate) fn new(mpd: MPD, presentation: LivePresentation, base_url: Option<Url>) -> Self {
        Self::with_clock(mpd, presentation, base_url, Arc::new(SystemClock))
    }

    /// Creates live state using `clock` instead of the system clock.
    pub(crate) fn with_clock(
        mpd: MPD,
        presentation: LivePresentation,
        base_url: Option<Url>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let state = LiveState {
            mpd,
            presentation: Some(presentation),
            base_url,
            indexes: HashMap::new(),
            fetched_at: clock.now(),
            stale_attempts: 0,
            retry_at: None,
            failure: None,
        };
        Self {
            clock,
            state: Mutex::new(state),
            generation: AtomicU64::new(0),
            #[cfg(feature = "sync")]
            blocking_refresh: Mutex::new(()),
            #[cfg(feature = "async")]
            async_refresh: futures_util::lock::Mutex::new(()),
        }
    }

    /// Returns the clock used for availability and refresh calculations.
    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Returns the generation of the currently installed manifest.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn lock(&self) -> MutexGuard<'_, LiveState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Sets the URL used to resolve relative URLs and, absent `Location`, to refresh the MPD.
    pub(crate) fn set_base_url(&self, url: Url) {
        self.lock().base_url = Some(url);
    }

    /// Opens representation `id` from the current manifest.
    ///
    /// Returns its segments and manifest generation. Later refreshes rebuild
    /// the representation's index so streams can adopt it.
    ///
    /// Fails as [`DashManifest::segments`](crate::DashManifest::segments)
    /// does, and once the presentation has stopped refreshing.
    pub(crate) fn segments(&self, id: &str) -> Result<(Segments, u64), Error> {
        let mut state = self.lock();
        if let Some(failure) = &state.failure {
            return Err(Error::Initialization(failure.clone()));
        }
        let generation = self.generation();
        let LiveState {
            mpd,
            presentation,
            base_url,
            indexes,
            ..
        } = &mut *state;
        let selected =
            find(mpd, id).ok_or_else(|| Error::DashManifestMissingRepresentation(id.to_owned()))?;
        let (init, index) = selected.template_index(base_url.as_ref(), presentation.as_ref())?;
        let index = match indexes.get(selected.id()) {
            Some(Ok(index)) => index.clone(),
            Some(Err(reason)) => return Err(Error::Initialization(reason.clone())),
            None => {
                let index = Arc::new(index);
                indexes.insert(selected.id().to_owned(), Ok(index.clone()));
                index
            }
        };
        Ok((Segments::from_index(init, index), generation))
    }

    /// Returns the latest index and generation for an opened representation.
    ///
    /// Fails if the representation was not opened or could not be preserved
    /// across a refresh.
    pub(crate) fn index(&self, id: &str) -> Result<(Arc<MediaIndex>, u64), String> {
        let state = self.lock();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let index = match state.indexes.get(id) {
            Some(index) => index.clone()?,
            None => return Err(format!("live representation {id} was never opened")),
        };
        Ok((index, self.generation()))
    }

    /// Resolves the effective start delay for `index`; see [`LiveDelay`].
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn delay(&self, choice: Option<LiveDelay>, index: &MediaIndex) -> Duration {
        let state = self.lock();
        let presentation = state.presentation.as_ref();
        let suggested = presentation
            .and_then(|live| live.suggested_presentation_delay)
            .map(LiveDelay::Time);
        let delay = match choice.or(suggested).unwrap_or(LiveDelay::FALLBACK) {
            LiveDelay::LiveEdge => Duration::ZERO,
            LiveDelay::Time(delay) => delay,
            LiveDelay::Segments(count) => {
                let longest = index
                    .max_segment_duration()
                    .or(presentation.and_then(|live| live.max_segment_duration))
                    .unwrap_or_default();
                longest.saturating_mul(count)
            }
        };
        match presentation.and_then(|live| live.time_shift_buffer_depth) {
            Some(depth) => delay.min(depth),
            None => delay,
        }
    }

    /// Chooses the segment at `delay` behind the current live edge.
    ///
    /// The result is clamped to the available window. If the window is empty,
    /// it is the next segment expected to become available.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn start(&self, index: &MediaIndex, delay: Duration) -> SegmentNumber {
        let now = self.clock.now();
        let available = index.available(now);
        let (Some(edge), Some(last)) = (index.live_edge(now), available.end.checked_prev()) else {
            return available.end;
        };
        index
            .number_at(edge.saturating_sub(delay))
            .unwrap_or(last)
            .clamp(available.start, last)
    }

    /// Returns when the next manifest refresh is due.
    ///
    /// Returns `None` if refresh is disabled, impossible, or no longer needed.
    /// A stream that has `ran_out` of listed segments may refresh earlier.
    ///
    /// Once the presentation has failed, a refresh is always due, and
    /// reports the failure.
    pub(crate) fn due_at(&self, ran_out: bool) -> Option<SystemTime> {
        let state = self.lock();
        if state.failure.is_some() {
            return Some(SystemTime::UNIX_EPOCH);
        }
        if let Some(retry) = state.retry_at {
            return Some(retry);
        }
        let presentation = state.presentation.as_ref()?;
        refresh_url(&state)?;
        let period = match presentation.minimum_update_period? {
            period if period.is_zero() => ZERO_UPDATE_PERIOD,
            period => period,
        };
        let regular = state.fetched_at.checked_add(period)?;
        let soon = state.fetched_at.checked_add(RUN_OUT_REFRESH)?;
        Some(match ran_out {
            true => regular.min(soon),
            false => regular,
        })
    }

    /// Returns the refresh URL when generation `seen` is still current and a
    /// refresh is due. Returns `None` if another stream already refreshed it.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn refresh_from(&self, seen: u64, ran_out: bool) -> Result<Option<Url>, Error> {
        if self.generation() != seen {
            return Ok(None);
        }
        let due = self.due_at(ran_out);
        let state = self.lock();
        if let Some(failure) = &state.failure {
            return Err(Error::Initialization(failure.clone()));
        }
        match due {
            Some(due) if due <= self.clock.now() => Ok(refresh_url(&state)),
            _ => Ok(None),
        }
    }

    /// Refreshes and installs the manifest through `transport` when needed.
    ///
    /// Concurrent blocking callers are serialized; a caller skips the fetch
    /// if another stream has already advanced generation `seen`.
    ///
    /// Fails if the fetch or the new manifest does, or once the
    /// presentation has failed.
    #[cfg(feature = "sync")]
    pub(crate) fn refresh_blocking(
        &self,
        transport: &dyn crate::segment::Transport,
        seen: u64,
        ran_out: bool,
    ) -> Result<(), Error> {
        let _refreshing = self
            .blocking_refresh
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match self.refresh_from(seen, ran_out)? {
            Some(url) => self.install(transport.fetch_manifest(url.as_str())?),
            None => Ok(()),
        }
    }

    /// Asynchronous counterpart of [`Self::refresh_blocking`].
    #[cfg(feature = "async")]
    pub(crate) async fn refresh(
        &self,
        transport: &dyn crate::nonblocking::segment::Transport,
        seen: u64,
        ran_out: bool,
    ) -> Result<(), Error> {
        let _refreshing = self.async_refresh.lock().await;
        match self.refresh_from(seen, ran_out)? {
            Some(url) => self.install(transport.fetch_manifest(url.as_str()).await?),
            None => Ok(()),
        }
    }

    /// Installs `fetched` and rebuilds every opened representation's index.
    ///
    /// A manifest older than the current one, by `MPD@publishTime`, is set
    /// aside to retry later; after [`STALE_RETRIES`] of them in a row this
    /// fails with [`Error::DashManifestStale`], and so does every later
    /// refresh. A representation whose index cannot be rebuilt or numbered
    /// as before fails on its own. Fails if `fetched` does not parse.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn install(&self, fetched: FetchedManifest) -> Result<(), Error> {
        let now = self.clock.now();
        let mpd = crate::vendor::dash_mpd_core::parse(&fetched.body)?;
        let presentation = LivePresentation::of(&mpd)?;
        let base_url = resolve_url(None, &fetched.url)?;
        let mut state = self.lock();

        // Delay and retry an older MPD instead of moving playback backward.
        let published = |mpd: &MPD| mpd.publishTime.map(SystemTime::from);
        if let (Some(new), Some(current)) = (published(&mpd), published(&state.mpd))
            && new < current
        {
            state.stale_attempts += 1;
            let attempts = state.stale_attempts;
            if attempts > STALE_RETRIES {
                let error = Error::DashManifestStale { attempts };
                state.failure = Some(error.to_string());
                return Err(error);
            }
            let wait = STALE_RETRY_STEP
                .saturating_mul(attempts)
                .min(STALE_RETRY_CAP);
            state.retry_at = now.checked_add(wait);
            return Ok(());
        }

        // Preserve segment numbers while rebuilding each opened representation.
        let rebuild = |id: &str, old: &Result<Arc<MediaIndex>, String>| {
            let old = old.as_ref().map_err(Clone::clone)?;
            let selected = find(&mpd, id).ok_or_else(|| {
                format!("the refreshed manifest no longer lists representation {id}")
            })?;
            let index = selected
                .template_index(Some(&base_url), presentation.as_ref())
                .and_then(|(_, index)| index.aligned_to(old))
                .map_err(|error| error.to_string())?;
            Ok(Arc::new(index))
        };
        let indexes = state
            .indexes
            .iter()
            .map(|(id, old)| (id.clone(), rebuild(id, old)))
            .collect();
        *state = LiveState {
            mpd,
            presentation,
            base_url: Some(base_url),
            indexes,
            fetched_at: now,
            stale_attempts: 0,
            retry_at: None,
            failure: None,
        };
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Starting point requested for a live stream.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Clone, Copy, Debug)]
pub(crate) enum LiveStart {
    /// Use the caller's delay, or the manifest/default delay when absent.
    Delay(Option<LiveDelay>),
    /// Use the segment containing this presentation time.
    At(Duration),
}

/// Segments, refresh state, and starting position used to open a live stream.
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) struct LiveSource {
    pub(crate) segments: Segments,
    pub(crate) link: LiveLink,
    /// Number of the first segment the stream reads.
    pub(crate) first: SegmentNumber,
    /// Presentation time at which the first segment starts.
    pub(crate) start: Duration,
}

#[cfg(any(feature = "sync", feature = "async"))]
impl DashManifest {
    /// What a stream of the representation `representation_id` opens with
    /// to start as `start` says, while the manifest is live: its segments,
    /// the link that keeps them refreshed, and where it starts. `None` for a
    /// static manifest.
    ///
    /// Fails as [`DashManifest::segments`] does, and with
    /// [`Error::Initialization`] for a start time when the segments are
    /// untimed.
    pub(crate) fn live_source(
        &self,
        representation_id: &str,
        start: LiveStart,
    ) -> Result<Option<LiveSource>, Error> {
        let Some(live) = self.live() else {
            return Ok(None);
        };
        let representation = self.select_indexed(representation_id)?.id().to_owned();
        let (segments, generation) = live.segments(&representation)?;
        let index = segments.index();
        let (first, start) = match start {
            LiveStart::At(time) => start_at(index, time)?,
            LiveStart::Delay(choice) => {
                let first = live.start(index, live.delay(choice, index));
                let start = index.span(first).map_or(Duration::ZERO, |span| span.start);
                (first, start)
            }
        };
        let link = LiveLink::new(live.clone(), representation, generation);
        Ok(Some(LiveSource {
            segments,
            link,
            first,
            start,
        }))
    }
}

/// Connects a stream to refreshes for one live representation.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Debug)]
pub(crate) struct LiveLink {
    pub(crate) live: Arc<Live>,
    representation: String,
    seen: AtomicU64,
}

/// Next action for a live stream whose current download queue is empty.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Refresh the manifest, noting whether the current MPD ran out of segments.
    Refresh { ran_out: bool },
    /// Wait until this time or until another event wakes the worker.
    Wait(Option<SystemTime>),
    /// No more segments can appear; finish after the current last segment.
    Finish,
}

#[cfg(any(feature = "sync", feature = "async"))]
impl LiveLink {
    /// A link to `live` for a stream of the representation whose ID is
    /// `representation`, holding its index of [`Live::generation`] `seen`.
    pub(crate) fn new(live: Arc<Live>, representation: String, seen: u64) -> Self {
        Self {
            live,
            representation,
            seen: AtomicU64::new(seen),
        }
    }

    /// Returns the generation of the index currently held by the stream.
    pub(crate) fn seen(&self) -> u64 {
        self.seen.load(Ordering::SeqCst)
    }

    /// Takes a refreshed index if one is newer than the stream's current index.
    ///
    /// Returns `None` if already current, or an error if the representation
    /// could not be carried across the refresh.
    pub(crate) fn refreshed(&self) -> Result<Option<Arc<MediaIndex>>, String> {
        if self.live.generation() == self.seen() {
            return Ok(None);
        }
        let (index, generation) = self.live.index(&self.representation)?;
        self.seen.store(generation, Ordering::SeqCst);
        Ok(Some(index))
    }

    /// Chooses the worker's next action at `now` for the given idle state.
    pub(crate) fn plan(&self, idle: Idle, now: SystemTime) -> Plan {
        let ran_out = idle == Idle::Refresh;
        let due = self.live.due_at(ran_out);
        if ran_out && due.is_none() {
            return Plan::Finish;
        }
        if due.is_some_and(|due| due <= now) {
            return Plan::Refresh { ran_out };
        }
        let until = match idle {
            Idle::Wait(until) => until,
            Idle::Refresh | Idle::Ended => None,
        };
        let earliest = [until, due].into_iter().flatten().min();
        Plan::Wait(earliest)
    }
}

/// Resolves the next refresh URL from `Location`, falling back to the MPD URL.
fn refresh_url(state: &LiveState) -> Option<Url> {
    let location = state
        .presentation
        .as_ref()
        .and_then(|live| live.location.as_deref());
    match location {
        Some(location) => resolve_url(state.base_url.as_ref(), location).ok(),
        None => state.base_url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::dash_mpd_core::parse;

    fn presentation(attributes: &str, children: &str) -> Result<Option<LivePresentation>, Error> {
        let xml = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" {attributes}>{children}<Period/></MPD>"#
        );
        LivePresentation::of(&parse(&xml)?)
    }

    #[test]
    fn a_dynamic_manifest_reads_its_live_timing() {
        let live = presentation(
            r#"type="dynamic" availabilityStartTime="1970-01-01T00:01:40Z"
               publishTime="1970-01-01T00:03:20Z" minimumUpdatePeriod="PT2S"
               timeShiftBufferDepth="PT30S" suggestedPresentationDelay="PT6S"
               maxSegmentDuration="PT2S""#,
            "<Location> https://cdn.example/next.mpd </Location>",
        )
        .unwrap()
        .unwrap();
        let at = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(
            live,
            LivePresentation {
                availability_start: at(100),
                publish_time: Some(at(200)),
                minimum_update_period: Some(Duration::from_secs(2)),
                time_shift_buffer_depth: Some(Duration::from_secs(30)),
                suggested_presentation_delay: Some(Duration::from_secs(6)),
                max_segment_duration: Some(Duration::from_secs(2)),
                location: Some("https://cdn.example/next.mpd".to_owned()),
            }
        );
    }

    #[test]
    fn static_manifests_are_not_live() {
        assert_eq!(presentation("", "").unwrap(), None);
        assert_eq!(presentation(r#"type="static""#, "").unwrap(), None);
    }

    #[test]
    fn a_live_manifest_needs_an_availability_start() {
        assert!(matches!(
            presentation(r#"type="dynamic""#, ""),
            Err(Error::DashManifestMissingAvailabilityStart)
        ));
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    mod refresh {
        use super::*;
        use crate::segment::clock::ManualClock;

        const MPD_URL: &str = "https://cdn.test/live.mpd";

        /// A live manifest with the attributes `attributes` and the
        /// children `children`, published at `published` seconds, of one
        /// representation of two-second templated segments.
        fn xml(attributes: &str, children: &str, published: u64) -> String {
            format!(
                r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                        availabilityStartTime="1970-01-01T00:00:00Z"
                        publishTime="1970-01-01T00:{:02}:{:02}Z" {attributes}>
                     {children}
                     <Period start="PT0S"><AdaptationSet mimeType="audio/mp4">
                       <Representation id="audio" bandwidth="1">
                         <SegmentTemplate initialization="i.mp4" media="$Number$.m4s" duration="2"/>
                       </Representation>
                     </AdaptationSet></Period>
                   </MPD>"#,
                published / 60,
                published % 60
            )
        }

        /// The live presentation of `xml`, fetched from [`MPD_URL`] at
        /// `secs` seconds on `clock`.
        fn live(xml: &str, clock: &Arc<ManualClock>, secs: u64) -> Live {
            clock.set(secs);
            let mpd = parse(xml).unwrap();
            let presentation = LivePresentation::of(&mpd).unwrap().unwrap();
            let base = Url::parse(MPD_URL).ok();
            Live::with_clock(mpd, presentation, base, clock.clone())
        }

        fn fetched(xml: &str) -> FetchedManifest {
            FetchedManifest {
                body: xml.to_owned(),
                url: MPD_URL.to_owned(),
            }
        }

        fn at(secs: u64) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
        }

        #[test]
        fn refreshes_fall_due_after_the_update_period() {
            let clock = ManualClock::at(0);
            let due = |attributes: &str, ran_out| {
                live(&xml(attributes, "", 0), &clock, 100).due_at(ran_out)
            };
            assert_eq!(due(r#"minimumUpdatePeriod="PT10S""#, false), Some(at(110)));
            assert_eq!(due(r#"minimumUpdatePeriod="PT10S""#, true), Some(at(101)));
            assert_eq!(due(r#"minimumUpdatePeriod="PT0S""#, false), Some(at(105)));
            assert_eq!(due("", false), None, "never changes");
            assert_eq!(due("", true), None);

            // Without a URL there is nothing to refresh from.
            let mpd = parse(&xml(r#"minimumUpdatePeriod="PT10S""#, "", 0)).unwrap();
            let presentation = LivePresentation::of(&mpd).unwrap().unwrap();
            let nowhere = Live::with_clock(mpd, presentation, None, clock.clone());
            assert_eq!(nowhere.due_at(false), None);
        }

        #[test]
        fn a_refresh_fetches_from_the_location() {
            let clock = ManualClock::at(0);
            let mpd = xml(
                r#"minimumUpdatePeriod="PT10S""#,
                "<Location>next/live.mpd</Location>",
                0,
            );
            let live = live(&mpd, &clock, 100);
            assert_eq!(live.refresh_from(0, false).unwrap(), None, "not yet due");
            clock.set(110);
            assert_eq!(
                live.refresh_from(0, false).unwrap(),
                Url::parse("https://cdn.test/next/live.mpd").ok()
            );
            assert_eq!(
                live.refresh_from(1, false).unwrap(),
                None,
                "already refreshed"
            );
        }

        #[test]
        fn stale_manifests_are_retried_then_fail() {
            let clock = ManualClock::at(0);
            let live = live(&xml(r#"minimumUpdatePeriod="PT10S""#, "", 50), &clock, 100);
            live.segments("audio").unwrap();
            let stale = xml(r#"minimumUpdatePeriod="PT10S""#, "", 40);

            // Each stale copy waits a second longer before the next try.
            for (attempt, retry) in [(1, 101), (2, 102), (3, 103)] {
                live.install(fetched(&stale)).unwrap();
                assert_eq!(live.due_at(false), Some(at(retry)), "attempt {attempt}");
                assert_eq!(live.generation(), 0);
            }
            assert!(matches!(
                live.install(fetched(&stale)),
                Err(Error::DashManifestStale { attempts: 4 })
            ));
            assert!(live.index("audio").is_err());
            assert!(live.refresh_from(0, false).is_err());

            // A newer copy is taken, and resets the count.
            let live = self::live(&xml(r#"minimumUpdatePeriod="PT10S""#, "", 50), &clock, 100);
            live.install(fetched(&stale)).unwrap();
            live.install(fetched(&xml(r#"minimumUpdatePeriod="PT10S""#, "", 60)))
                .unwrap();
            assert_eq!(live.generation(), 1);
            assert_eq!(live.due_at(false), Some(at(110)));
        }

        #[test]
        fn a_manifest_turning_static_stops_refreshing() {
            let clock = ManualClock::at(0);
            let mpd = xml(r#"minimumUpdatePeriod="PT10S""#, "", 50);
            let live = live(&mpd, &clock, 100);
            let (segments, _) = live.segments("audio").unwrap();
            assert!(segments.is_live());

            let ended = mpd
                .replace(
                    r#"type="dynamic""#,
                    r#"type="static" mediaPresentationDuration="PT120S""#,
                )
                .replace("00:00:50Z", "00:01:00Z");
            live.install(fetched(&ended)).unwrap();
            assert_eq!(live.due_at(true), None);
            let (index, generation) = live.index("audio").unwrap();
            assert_eq!(generation, 1);
            assert!(!index.is_live());
            assert_eq!(index.numbers(), SegmentNumber(1)..SegmentNumber(61));
        }

        #[cfg(feature = "sync")]
        #[test]
        fn streams_refreshing_together_fetch_once() {
            use crate::segment::{SegmentLocation, Transport};
            use std::sync::atomic::AtomicUsize;

            /// Serves a newer manifest, slowly.
            struct Slow(AtomicUsize);
            impl Transport for Slow {
                fn get(&self, _: &SegmentLocation, _: Option<u64>) -> Result<bytes::Bytes, Error> {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    let newer = xml(r#"minimumUpdatePeriod="PT10S""#, "", 60);
                    Ok(bytes::Bytes::from(newer))
                }
            }

            let clock = ManualClock::at(0);
            let live = live(&xml(r#"minimumUpdatePeriod="PT10S""#, "", 50), &clock, 100);
            clock.set(110);
            let transport = Slow(AtomicUsize::new(0));
            std::thread::scope(|scope| {
                for _ in 0..2 {
                    scope.spawn(|| live.refresh_blocking(&transport, 0, false).unwrap());
                }
            });
            assert_eq!(transport.0.load(Ordering::SeqCst), 1);
            assert_eq!(live.generation(), 1);
        }

        #[test]
        fn the_start_delay_is_the_callers_else_the_manifests() {
            let clock = ManualClock::at(0);
            let delay = |attributes: &str, choice| {
                let live = live(&xml(attributes, "", 0), &clock, 100);
                let (segments, _) = live.segments("audio").unwrap();
                live.delay(choice, segments.index())
            };
            let secs = Duration::from_secs;
            let suggested = r#"suggestedPresentationDelay="PT7S""#;
            assert_eq!(delay(suggested, None), secs(7));
            assert_eq!(delay(suggested, Some(LiveDelay::LiveEdge)), secs(0));
            assert_eq!(delay(suggested, Some(LiveDelay::Segments(3))), secs(6));
            assert_eq!(delay("", None), secs(8), "four two-second segments");
            assert_eq!(delay("", Some(LiveDelay::Time(secs(5)))), secs(5));
            assert_eq!(
                delay(
                    r#"timeShiftBufferDepth="PT4S""#,
                    Some(LiveDelay::Time(secs(9)))
                ),
                secs(4),
                "no further back than the time-shift window"
            );
        }
    }
}
