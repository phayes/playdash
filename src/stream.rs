//! Progressive in-memory reader over MPEG-DASH fragments.

use crate::error::Error;
use log::warn;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Media-segment durations used to map playback time onto downloaded fragments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaTimeline {
    /// DASH timescale: ticks per second.
    pub timescale: u32,
    /// Duration of each media fragment, in timescale ticks, in playback order.
    pub media_durations: Vec<u64>,
}

/// Padding applied to advertised media bytes when guessing total length.
/// AAC `bandwidth` is near the real rate; FLAC's is often uncompressed PCM.
const SYMPHONIA_COMPAT_MEDIA_FACTOR: u128 = 4;

/// Extra bytes reserved per media fragment for `moof` / box overhead.
const SYMPHONIA_COMPAT_BYTES_PER_FRAGMENT: u128 = 64 * 1024;

impl MediaTimeline {
    fn ticks_to_duration(&self, ticks: u64) -> Option<Duration> {
        if self.timescale == 0 {
            return None;
        }
        let timescale = u64::from(self.timescale);
        let seconds = ticks / timescale;
        let nanos = (u128::from(ticks % timescale) * 1_000_000_000 / u128::from(timescale)) as u32;
        Some(Duration::new(seconds, nanos))
    }

    fn fragment_start(&self, fragment_index: usize) -> Option<Duration> {
        if fragment_index == 0 || fragment_index > self.media_durations.len() {
            return None;
        }
        let ticks: u64 = self.media_durations[..fragment_index - 1]
            .iter()
            .copied()
            .sum();
        self.ticks_to_duration(ticks)
    }

    fn total_duration(&self) -> Option<Duration> {
        self.ticks_to_duration(self.media_durations.iter().copied().sum())
    }

    /// Oversized guess at concatenated init + media bytes.
    ///
    /// Uses four times the advertised bitrate over the timeline, plus the
    /// known initialization segment and 64 KiB per media fragment. Intended
    /// for demuxers that need a length before every fragment size is known.
    pub fn estimate_len(&self, init_len: u64, bitrate_bps: u32) -> Option<u64> {
        if bitrate_bps == 0 || self.timescale == 0 || self.media_durations.is_empty() {
            return None;
        }
        let ticks: u128 = self.media_durations.iter().copied().map(u128::from).sum();
        let fragments = u128::from(self.media_durations.len() as u64);
        let denom = u128::from(self.timescale) * 8;
        let media = (u128::from(bitrate_bps) * ticks).div_ceil(denom);
        let padded = media
            .saturating_mul(SYMPHONIA_COMPAT_MEDIA_FACTOR)
            .saturating_add(fragments.saturating_mul(SYMPHONIA_COMPAT_BYTES_PER_FRAGMENT))
            .saturating_add(u128::from(init_len));
        Some(u64::try_from(padded).unwrap_or(u64::MAX))
    }
}

/// Byte and media-time information for a stream position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// Absolute offset in the concatenated init + media byte stream.
    ///
    /// This is `None` when preceding fragment sizes are not yet known.
    pub byte: Option<u64>,

    /// Media timestamp corresponding to the position, when a timeline applies.
    pub time: Option<TimePosition>,
}

/// Media timestamp and the accuracy with which it was resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimePosition {
    /// Actual media timestamp at which reading begins.
    pub timestamp: Duration,

    /// Whether `timestamp` is exact or the start of a containing fragment.
    pub accuracy: TimeAccuracy,
}

/// Accuracy of a reported media timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeAccuracy {
    /// The byte position is known to land exactly at this media timestamp.
    Exact,

    /// The timestamp is the start of the fragment containing the requested position.
    Coarse,
}

/// Parses an inclusive byte range in the DASH `@mediaRange` / `@range` and
/// HTTP `first-last` form, such as `"0-837"`.
pub(crate) fn parse_byte_range(value: &str) -> Result<RangeInclusive<u64>, Error> {
    value
        .trim()
        .split_once('-')
        .and_then(|(start, end)| {
            let (start, end) = (start.parse().ok()?, end.parse().ok()?);
            (start <= end).then_some(start..=end)
        })
        .ok_or_else(|| Error::InvalidByteRange(value.to_owned()))
}

/// One piece of the concatenated stream: a URL, optionally narrowed to a byte range.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fragment {
    /// Absolute URL of the resource.
    pub url: String,
    /// Inclusive byte range within the resource, as in an HTTP `Range`
    /// header, or `None` for the whole body. Must not be empty.
    pub range: Option<RangeInclusive<u64>>,
}

impl Fragment {
    /// The whole body at `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            range: None,
        }
    }

    /// The bytes of `url` covered by `range`.
    pub fn with_range(url: impl Into<String>, range: RangeInclusive<u64>) -> Self {
        Self {
            url: url.into(),
            range: Some(range),
        }
    }

    /// Byte length, when the range makes it known in advance.
    pub fn known_size(&self) -> Option<u64> {
        self.range
            .as_ref()
            .and_then(|range| range.end().checked_sub(*range.start())?.checked_add(1))
    }
}

impl From<String> for Fragment {
    fn from(url: String) -> Self {
        Self::new(url)
    }
}

impl From<&str> for Fragment {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

impl fmt::Display for Fragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.range {
            Some(range) => write!(f, "{} (bytes {}-{})", self.url, range.start(), range.end()),
            None => f.write_str(&self.url),
        }
    }
}

/// Fetches fragment bodies and sizes for a [`FragmentCache`].
///
/// [`ureq::Agent`] implements this with its connection pool.
pub trait Transport: Send + Sync {
    /// Body of `fragment`.
    ///
    /// Must honor [`Fragment::range`] and return exactly those bytes; a ranged
    /// body of any other length fails the stream. `size_hint` is the length
    /// when already known, for preallocating the body.
    fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Arc<[u8]>, Error>;

    /// Byte length of `fragment`, typically from a HEAD `Content-Length`.
    ///
    /// Never called for ranged fragments, whose sizes are already known. An
    /// error disables HEAD probing for the stream; the default always errors.
    fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
        Err(Error::StreamInitializationError(format!(
            "transport cannot HEAD {fragment}"
        )))
    }
}

impl Transport for ureq::Agent {
    /// Sends a `Range` header for ranged fragments; if a server ignores it and
    /// returns the whole body, the range is sliced out of that body.
    fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Arc<[u8]>, Error> {
        let mut request = ureq::Agent::get(self, &fragment.url);
        if let Some(range) = &fragment.range {
            request = request.header(
                ureq::http::header::RANGE,
                format!("bytes={}-{}", range.start(), range.end()),
            );
        }
        let response = request.call()?;
        let partial = response.status() == ureq::http::StatusCode::PARTIAL_CONTENT;
        let mut bytes = size_hint
            .and_then(|len| usize::try_from(len).ok())
            .map(Vec::with_capacity)
            .unwrap_or_default();
        response.into_body().into_reader().read_to_end(&mut bytes)?;
        match &fragment.range {
            Some(range) if !partial => {
                warn!("server ignored the Range request for {fragment}; slicing the full body");
                Ok(slice_range(&bytes, range).unwrap_or(&bytes).into())
            }
            _ => Ok(bytes.into()),
        }
    }

    fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
        let url = &fragment.url;
        let response = ureq::Agent::head(self, url).call()?;
        response
            .headers()
            .get(ureq::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                Error::StreamInitializationError(format!(
                    "HEAD for {url} did not return Content-Length"
                ))
            })
    }
}

/// What is known about one [`Fragment`]: its body once fetched, and its
/// length once a range, HEAD or GET establishes it. Shared by every reader of
/// a [`FragmentCache`]; each value is set at most once.
#[derive(Debug)]
struct FragmentData {
    fragment: Fragment,
    body: OnceLock<Arc<[u8]>>,
    /// Length from HEAD, before the body arrives.
    probed_size: OnceLock<u64>,
    /// Held across a GET so a second reader waits for that download instead
    /// of starting its own.
    get_lock: Mutex<()>,
    /// Held across a HEAD, as `get_lock` is for GETs.
    head_lock: Mutex<()>,
}

impl FragmentData {
    fn new(fragment: Fragment) -> Self {
        Self {
            fragment,
            body: OnceLock::new(),
            probed_size: OnceLock::new(),
            get_lock: Mutex::new(()),
            head_lock: Mutex::new(()),
        }
    }

    fn body(&self) -> Option<&Arc<[u8]>> {
        self.body.get()
    }

    /// Byte length from the range, else the body, else a probe.
    fn size(&self) -> Option<u64> {
        self.fragment
            .known_size()
            .or_else(|| self.body().map(|body| body.len() as u64))
            .or_else(|| self.probed_size.get().copied())
    }
}

/// Fragment bodies and lengths, keyed by [`Fragment`], and the [`Transport`]
/// that fetches them.
///
/// Use this to share caches across multiple readers that read the same stream,
/// such as a reader rebuilt after a seek.
pub struct FragmentCache {
    transport: Arc<dyn Transport>,
    fragments: Mutex<HashMap<Fragment, Arc<FragmentData>>>,
}

impl FragmentCache {
    /// An empty cache that fetches through `transport`.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            fragments: Mutex::default(),
        }
    }

    /// The body of `fragment`, if it has been fetched.
    pub fn cached(&self, fragment: &Fragment) -> Option<Arc<[u8]>> {
        self.lock().get(fragment)?.body().cloned()
    }

    /// The byte length of `fragment`, if its range, a HEAD or a GET has
    /// established it.
    pub fn known_size(&self, fragment: &Fragment) -> Option<u64> {
        fragment
            .known_size()
            .or_else(|| self.lock().get(fragment)?.size())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Fragment, Arc<FragmentData>>> {
        self.fragments
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The shared entry for `fragment`, created empty if new.
    fn data(&self, fragment: Fragment) -> Arc<FragmentData> {
        self.lock()
            .entry(fragment)
            .or_insert_with_key(|fragment| Arc::new(FragmentData::new(fragment.clone())))
            .clone()
    }

    /// The body of `data`, fetching it unless it is cached or another reader
    /// is already fetching it. A ranged body of the wrong length is an error
    /// and is not cached.
    fn get(&self, data: &FragmentData) -> Result<Arc<[u8]>, Error> {
        if let Some(body) = data.body() {
            return Ok(body.clone());
        }
        let _in_flight = data
            .get_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(body) = data.body() {
            return Ok(body.clone());
        }
        let fragment = &data.fragment;
        let body = self.transport.get(fragment, data.size())?;
        if let Some(expected) = fragment.known_size()
            && body.len() as u64 != expected
        {
            return Err(Error::FragmentLength {
                fragment: fragment.to_string(),
                expected,
                actual: body.len() as u64,
            });
        }
        Ok(data.body.get_or_init(|| body).clone())
    }

    /// HEADs `data` unless its size is already known or another reader is
    /// already probing it.
    fn probe(&self, data: &FragmentData) -> Result<(), Error> {
        if data.size().is_some() {
            return Ok(());
        }
        let _in_flight = data
            .head_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if data.size().is_none() {
            let size = self.transport.head(&data.fragment)?;
            data.probed_size.get_or_init(|| size);
        }
        Ok(())
    }
}

impl Default for FragmentCache {
    /// Fetches through a ureq agent with this crate's connection-pool settings.
    fn default() -> Self {
        Self::new(Arc::new(ureq_agent()))
    }
}

impl fmt::Debug for FragmentCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FragmentCache")
            .field("fragments", &self.lock().len())
            .finish_non_exhaustive()
    }
}

/// How long to keep an idle HTTP connection in the ureq pool.
///
/// Fastly allows idle client reuse for up to 10 minutes. Stay well under
/// that so we close first, and under typical NAT idle timeouts.
const POOL_IDLE_AGE: Duration = Duration::from_secs(90);

fn ureq_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_idle_age(POOL_IDLE_AGE)
            .build(),
    )
}

/// The `range` bytes of a full body, for servers that answer a Range request with 200.
fn slice_range<'a>(body: &'a [u8], range: &RangeInclusive<u64>) -> Option<&'a [u8]> {
    let start = usize::try_from(*range.start()).ok()?;
    let end = usize::try_from(*range.end()).ok()?;
    body.get(start..=end)
}

/// Progressive in-memory reader over MPEG-DASH fragments.
///
/// Bodies live in a [`FragmentCache`], which can be shared across readers. The
/// initialization segment is index `0`, followed by media segments in
/// timeline order.
/// Ranged fragments have known sizes up front and are fetched with HTTP
/// `Range` requests. [`Read`] blocks until the current fragment slot is filled.
///
/// Use [`Self::seek_bytes`] for exact offsets in the concatenated byte
/// stream. When HEAD probes are available, `seek_bytes` discovers fragment
/// lengths without downloading skipped bodies; if HEAD fails or disagrees
/// with a later GET, the reader falls back to waiting on downloads. After
/// the initialization GET, an optional HEAD worker fills remaining sizes
/// on a second HTTP connection while the GET worker downloads bodies; see
/// [`Self::start_eager_head`] and [`Self::stop_eager_head`].
/// Use [`Self::seek_time_coarse`] to jump to the beginning of the media
/// fragment containing a timestamp — the GET worker fills from the playhead
/// forward, then backfills earlier holes. Returned [`Position`] values include
/// both the known absolute byte offset and an exact or coarse media timestamp.
/// Enable [`Self::set_symphonia_compat`] so [`SeekFrom::End`]`(0)` can
/// return an oversized length estimate without waiting for every fragment.
#[derive(Debug)]
pub struct MpegStreamReader {
    shared: Arc<ReaderShared>,
    byte_position: Option<u64>,
    fragment_index: usize,
    offset_in_fragment: usize,
    timeline: Option<MediaTimeline>,
    /// When set, [`SeekFrom::End`]`(0)` may report [`Self::length_estimate`]
    /// instead of waiting for every fragment size.
    symphonia_compat: bool,
    length_estimate: Option<u64>,
}

/// A reader's state, shared with its GET and HEAD worker threads.
#[derive(Debug)]
struct ReaderShared {
    state: Mutex<ReaderState>,
    /// Notified after every change to `state`, or to a body or size in
    /// `state.fragments` written by this reader's threads.
    condvar: Condvar,
    cancelled: AtomicBool,
    cache: Arc<FragmentCache>,
}

/// Which fragments a reader wants and how its workers schedule them.
#[derive(Debug)]
struct ReaderState {
    /// Initialization fragment, then media fragments in timeline order.
    fragments: Vec<Arc<FragmentData>>,
    /// When false, byte seeks wait on downloaded bodies instead of HEAD.
    head_available: bool,
    /// Background HEAD prefetch is wanted. Does not gate on-demand seek HEADs.
    eager_head: bool,
    /// A HEAD worker thread is alive, possibly finishing an in-flight request.
    head_worker_running: bool,
    /// Read/seek playhead. The GET worker fills at or after this index first.
    cursor_fragment: usize,
    priority: Option<usize>,
    complete: bool,
    error: Option<String>,
}

impl ReaderState {
    fn all_sizes_known(&self) -> bool {
        self.total_len().is_some()
    }

    fn missing_body(&self, index: usize) -> bool {
        self.fragments
            .get(index)
            .is_some_and(|data| data.body().is_none())
    }

    fn disable_head(&mut self, reason: &str) {
        if self.head_available {
            self.head_available = false;
            warn!("MPEG stream disabling HEAD size probes: {reason}");
        }
    }

    fn next_get(&mut self) -> Option<(usize, Arc<FragmentData>)> {
        let cursor = self.cursor_fragment.min(self.fragments.len());
        let index = self
            .priority
            .take()
            .filter(|&priority| self.missing_body(priority))
            .or_else(|| {
                (cursor..self.fragments.len())
                    .chain(0..cursor)
                    .find(|&index| self.missing_body(index))
            })?;
        Some((index, self.fragments[index].clone()))
    }

    fn next_head(&self) -> Option<(usize, Arc<FragmentData>)> {
        if !self.head_available {
            return None;
        }
        let index = self
            .fragments
            .iter()
            .position(|data| data.size().is_none())?;
        Some((index, self.fragments[index].clone()))
    }

    /// Sum of the first `count` fragment sizes, if all are known.
    fn prefix_len(&self, count: usize) -> Option<u64> {
        self.fragments[..count].iter().map(|data| data.size()).sum()
    }

    fn total_len(&self) -> Option<u64> {
        self.prefix_len(self.fragments.len())
    }

    // TODO: Publish a media fragment's leading `moof` as soon as those bytes
    // have arrived, instead of waiting for the trailing `mdat`. IsoMp4 open
    // and the first packet only need the `moof`; `Read` currently blocks until
    // this method sees the whole GET. Stream the body, parse top-level box
    // sizes, and wake readers once the `moof` is complete.
    /// Stops trusting HEAD if fragment `index`'s body disagrees with its probe.
    fn check_probe(&mut self, index: usize) {
        let data = &self.fragments[index];
        if let (Some(&expected), Some(body)) = (data.probed_size.get(), data.body())
            && expected != body.len() as u64
        {
            let len = body.len();
            self.disable_head(&format!(
                "GET length {len} != HEAD length {expected} for fragment {index}"
            ));
        }
    }

    /// Maps an absolute byte offset onto a fragment cursor once contiguous
    /// sizes covering that offset are known.
    fn resolve_cursor(&self, target: u64) -> Option<(usize, usize)> {
        let mut acc = 0u64;
        for (index, data) in self.fragments.iter().enumerate() {
            let len = data.size()?;
            if target < acc + len {
                return Some((index, (target - acc) as usize));
            }
            acc += len;
        }

        let last = self.fragments.len().checked_sub(1)?;
        Some((last, self.fragments[last].size().unwrap_or(0) as usize))
    }
}

impl MpegStreamReader {
    /// Downloads the first fragment with a new ureq agent, then fills
    /// remaining slots on background threads.
    ///
    /// The initialization GET runs first so its TLS session is established
    /// before any other request. A GET worker then downloads bodies at or
    /// ahead of the read cursor, and only backfills earlier holes when the
    /// suffix is complete. When `eager_head` is set, a HEAD worker fills the
    /// size list on its own pooled connection; otherwise prefetch starts
    /// stopped (see [`Self::start_eager_head`]). If HEAD fails or later
    /// disagrees with GET, HEAD probing is disabled. See
    /// [`Self::new_with_transport`] for a custom [`Transport`], and
    /// [`Self::new_with_cache`] to share connections and downloaded bodies
    /// across readers.
    pub fn new(
        fragments: Vec<Fragment>,
        timeline: Option<MediaTimeline>,
        eager_head: bool,
    ) -> Result<Self, Error> {
        Self::new_with_cache(fragments, timeline, eager_head, Arc::default())
    }

    /// Like [`Self::new`], but fetches through `transport` with a new
    /// [`FragmentCache`] dedicated to this reader.
    pub fn new_with_transport(
        fragments: Vec<Fragment>,
        timeline: Option<MediaTimeline>,
        eager_head: bool,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, Error> {
        let cache = FragmentCache::new(transport);
        Self::new_with_cache(fragments, timeline, eager_head, Arc::new(cache))
    }

    /// Creates a reader that fetches through `cache`.
    ///
    /// The initialization fragment is fetched before returning. After that, a
    /// dedicated HEAD worker rips through unknown sizes if `eager_head` is
    /// set; otherwise no worker is spawned until [`Self::start_eager_head`].
    /// On-demand HEADs from byte seeks run either way. If HEAD fails or later
    /// disagrees with GET, HEAD probing is disabled for the rest of the stream.
    /// Sizes known from a fragment's range or from `cache` are never HEADed.
    /// GETs receive a fragment's length as `size_hint` when one of those or a
    /// prior HEAD established it.
    pub fn new_with_cache(
        fragments: Vec<Fragment>,
        timeline: Option<MediaTimeline>,
        eager_head: bool,
        cache: Arc<FragmentCache>,
    ) -> Result<Self, Error> {
        if fragments.is_empty() {
            return Err(Error::DashManifestMissingUrls);
        }

        let fragments: Vec<_> = fragments
            .into_iter()
            .map(|fragment| cache.data(fragment))
            .collect();
        cache.get(&fragments[0])?;
        let complete = fragments.iter().all(|data| data.body().is_some());

        let shared = Arc::new(ReaderShared {
            state: Mutex::new(ReaderState {
                fragments,
                head_available: true,
                eager_head: false,
                head_worker_running: false,
                cursor_fragment: 0,
                priority: None,
                complete,
                error: None,
            }),
            condvar: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cache,
        });

        if !complete {
            spawn_get_worker(shared.clone());
        }

        let reader = Self {
            shared,
            byte_position: Some(0),
            fragment_index: 0,
            offset_in_fragment: 0,
            timeline,
            symphonia_compat: false,
            length_estimate: None,
        };
        if eager_head {
            reader.start_eager_head();
        }
        Ok(reader)
    }

    /// Starts background HEAD prefetch of unknown fragment sizes.
    ///
    /// Spawns the HEAD worker if none is running. No-op once every size is
    /// known or HEAD probing has been disabled.
    pub fn start_eager_head(&self) {
        let mut inner = self.shared.lock();
        inner.eager_head = true;
        if inner.head_worker_running
            || inner.error.is_some()
            || inner.next_head().is_none()
            || self.shared.cancelled.load(Ordering::SeqCst)
        {
            // A running worker sees `eager_head` on its next check and continues.
            return;
        }
        inner.head_worker_running = true;
        drop(inner);
        spawn_head_worker(self.shared.clone());
    }

    /// Stops background HEAD prefetch.
    ///
    /// An in-flight HEAD still completes and records its size; no further
    /// prefetch HEADs are issued. On-demand HEADs from byte seeks are
    /// unaffected.
    pub fn stop_eager_head(&self) {
        self.shared.lock().eager_head = false;
    }

    /// Treat [`SeekFrom::End`]`(0)` as a length query for demuxers such as
    /// Symphonia's ISO-BMFF reader, which seek to EOF to learn the file size.
    ///
    /// While any fragment size is still unknown, `End(0)` returns an oversized
    /// estimate (`bitrate_bps` × timeline duration × 4, plus init and 64 KiB
    /// per media fragment) and does not move the download playhead. Once every
    /// size is known, `End(0)` reports the exact total. Non-zero
    /// [`SeekFrom::End`] offsets still wait for real sizes.
    ///
    /// Requires a [`MediaTimeline`] and a non-zero bitrate.
    pub fn set_symphonia_compat(&mut self, enabled: bool, bitrate_bps: u32) -> Result<(), Error> {
        if !enabled {
            self.symphonia_compat = false;
            self.length_estimate = None;
            return Ok(());
        }

        let timeline = self.timeline.as_ref().ok_or_else(|| {
            Error::StreamInitializationError(
                "symphonia_compat requires a SegmentTimeline".to_owned(),
            )
        })?;
        let init_len = {
            let inner = self.shared.lock();
            inner.fragments[0].size().unwrap_or(0)
        };
        let estimate = timeline
            .estimate_len(init_len, bitrate_bps)
            .ok_or_else(|| {
                Error::StreamInitializationError(
                    "symphonia_compat requires a non-zero bitrate and timescale".to_owned(),
                )
            })?;
        self.symphonia_compat = true;
        self.length_estimate = Some(estimate);
        Ok(())
    }

    /// Exact total when every size is known; otherwise the compat overestimate.
    fn reported_end_len(&self) -> Option<u64> {
        let inner = self.shared.lock();
        if let Some(exact) = inner.total_len() {
            return Some(exact);
        }
        if !self.symphonia_compat {
            return None;
        }
        let known: u64 = inner.fragments.iter().filter_map(|data| data.size()).sum();
        self.length_estimate.map(|estimate| estimate.max(known))
    }

    /// Seeks to an exact offset in the concatenated initialization + media bytes.
    ///
    /// When HEAD is available, discovers lengths for skipped fragments without
    /// downloading them, then prioritizes a GET of the target fragment.
    /// Otherwise backfills bodies until the offset is reachable.
    /// [`SeekFrom::Current`] requires a known absolute byte cursor.
    ///
    /// The returned byte offset is exact, except [`SeekFrom::End`]`(0)` under
    /// [`Self::set_symphonia_compat`], which may report an oversized estimate.
    /// When a timeline is available, the returned time is exact at a fragment
    /// boundary and otherwise reports the containing fragment's start as
    /// [`TimeAccuracy::Coarse`].
    pub fn seek_bytes(&mut self, from: SeekFrom) -> io::Result<Position> {
        if let SeekFrom::End(0) = from
            && let Some(len) = self.reported_end_len()
        {
            return self.park_at_reported_end(len);
        }

        let target = match from {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                let Some(position) = self.byte_position else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "MPEG stream absolute byte position is unknown; use seek_time_coarse or SeekFrom::Start",
                    ));
                };
                if offset >= 0 {
                    position.saturating_add(offset as u64)
                } else {
                    position.saturating_sub(offset.unsigned_abs())
                }
            }
            SeekFrom::End(offset) => {
                self.ensure_sizes(ReaderState::all_sizes_known)?;
                let size = {
                    let inner = self.shared.lock();
                    inner.total_len().unwrap_or(0)
                };
                if offset >= 0 {
                    size.saturating_add(offset as u64)
                } else {
                    size.saturating_sub(offset.unsigned_abs())
                }
            }
        };

        self.ensure_sizes(|inner| inner.resolve_cursor(target).is_some())?;

        let (fragment_index, offset_in_fragment) = {
            let inner = self.shared.lock();
            match inner.resolve_cursor(target) {
                Some(cursor) => cursor,
                None => {
                    return Err(io::Error::other(
                        "MPEG stream ended before the requested byte offset",
                    ));
                }
            }
        };

        self.fragment_index = fragment_index;
        self.offset_in_fragment = offset_in_fragment;
        self.publish_cursor();
        self.prioritize_fragment(fragment_index);
        self.byte_position = Some(target);
        Ok(Position {
            byte: Some(target),
            time: self.time_at_fragment(
                fragment_index,
                if offset_in_fragment == 0 {
                    TimeAccuracy::Exact
                } else {
                    TimeAccuracy::Coarse
                },
            ),
        })
    }

    /// Coarsely seeks to a media timestamp using the DASH `SegmentTimeline`.
    ///
    /// Time `0` is the start of the first media fragment, after the
    /// initialization segment. The seek lands at byte `0` of the fragment
    /// containing `time`, so an fMP4 consumer receives the fragment's `moof`
    /// rather than an arbitrary position inside `mdat`. The target fragment is
    /// prioritized and loaded before this method returns.
    ///
    /// This is segment-granular: playback may begin earlier than `time` by up
    /// to one media-fragment duration. The returned [`TimePosition`] contains
    /// the actual fragment-start timestamp and marks it [`TimeAccuracy::Exact`]
    /// only when the requested time is exactly that boundary. The byte field is
    /// `None` until every earlier fragment size is known.
    pub fn seek_time_coarse(&mut self, time: Duration) -> io::Result<Position> {
        let timeline = self.timeline.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "MPEG stream has no SegmentTimeline",
            )
        })?;
        if timeline.timescale == 0 || timeline.media_durations.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MPEG stream timeline is missing a timescale or media durations",
            ));
        }

        let total_ticks: u64 = timeline.media_durations.iter().sum();
        let target_ticks =
            ((time.as_secs_f64() * f64::from(timeline.timescale)).round() as u64).min(total_ticks);

        let mut tick = 0u64;
        let mut media_index = timeline.media_durations.len() - 1;

        for (index, duration) in timeline.media_durations.iter().enumerate() {
            let next = tick.saturating_add(*duration);
            if target_ticks < next
                || (target_ticks == total_ticks && index + 1 == timeline.media_durations.len())
            {
                media_index = index;
                break;
            }
            tick = next;
        }

        let target_fragment = media_index + 1;
        {
            let mut inner = self.shared.lock();
            if target_fragment >= inner.fragments.len() {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            }
            inner.cursor_fragment = target_fragment;
            if inner.missing_body(target_fragment) {
                inner.priority = Some(target_fragment);
                self.shared.condvar.notify_all();
            }
        }

        let byte_position = {
            let inner =
                self.wait_until(|inner| !inner.missing_body(target_fragment) || inner.complete)?;
            if inner.missing_body(target_fragment) {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            }
            inner.prefix_len(target_fragment)
        };

        self.fragment_index = target_fragment;
        self.offset_in_fragment = 0;
        self.byte_position = byte_position;
        Ok(Position {
            byte: byte_position,
            time: self.time_at_fragment(
                target_fragment,
                if target_ticks == tick {
                    TimeAccuracy::Exact
                } else {
                    TimeAccuracy::Coarse
                },
            ),
        })
    }

    /// Fills unknown sizes until `known` holds, via on-demand HEAD or GET
    /// fallback. Also returns once every body is downloaded.
    fn ensure_sizes(&self, known: impl Fn(&ReaderState) -> bool) -> io::Result<()> {
        loop {
            let next_head = {
                let inner = self.shared.lock();
                if known(&inner) {
                    return Ok(());
                }
                inner.next_head()
            };
            // A failed HEAD disables probing, so `next_head` ends the loop.
            let Some((index, data)) = next_head else {
                break;
            };
            self.shared.probe(index, &data);
        }

        {
            let mut inner = self.shared.lock();
            if known(&inner) {
                return Ok(());
            }
            if let Some(index) = (0..inner.fragments.len()).find(|&i| inner.missing_body(i)) {
                inner.priority = Some(index);
                self.shared.condvar.notify_all();
            }
        }

        self.wait_until(|inner| known(inner) || inner.complete)
            .map(drop)
    }

    fn wait_until<F>(&self, mut ready: F) -> io::Result<MutexGuard<'_, ReaderState>>
    where
        F: FnMut(&ReaderState) -> bool,
    {
        let mut inner = self.shared.lock();
        loop {
            if self.shared.cancelled.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "MPEG stream cancelled",
                ));
            }
            if let Some(error) = &inner.error {
                return Err(io::Error::other(error.clone()));
            }
            if ready(&inner) {
                return Ok(inner);
            }
            inner = self
                .shared
                .condvar
                .wait(inner)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn prioritize_fragment(&self, index: usize) {
        let mut inner = self.shared.lock();
        if inner.missing_body(index) {
            inner.priority = Some(index);
            self.shared.condvar.notify_all();
        }
    }

    fn publish_cursor(&self) {
        let mut inner = self.shared.lock();
        inner.cursor_fragment = self.fragment_index;
    }

    fn time_at_fragment(
        &self,
        fragment_index: usize,
        accuracy: TimeAccuracy,
    ) -> Option<TimePosition> {
        self.timeline
            .as_ref()?
            .fragment_start(fragment_index)
            .map(|timestamp| TimePosition {
                timestamp,
                accuracy,
            })
    }

    fn time_at_eof(&self) -> Option<TimePosition> {
        self.timeline
            .as_ref()?
            .total_duration()
            .map(|timestamp| TimePosition {
                timestamp,
                accuracy: TimeAccuracy::Exact,
            })
    }

    /// Report EOF for [`SeekFrom::End`]`(0)` without retargeting the GET worker.
    fn park_at_reported_end(&mut self, len: u64) -> io::Result<Position> {
        let fragment_count = {
            let inner = self.shared.lock();
            inner.fragments.len()
        };
        self.fragment_index = fragment_count;
        self.offset_in_fragment = 0;
        self.byte_position = Some(len);
        Ok(Position {
            byte: Some(len),
            time: self.time_at_eof(),
        })
    }
}

impl ReaderShared {
    fn lock(&self) -> MutexGuard<'_, ReaderState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// HEADs fragment `index` unless its size is known, or disables HEAD
    /// probing if the request fails.
    fn probe(&self, index: usize, data: &FragmentData) {
        let result = self.cache.probe(data);
        let mut state = self.lock();
        if let Err(error) = result {
            state.disable_head(&format!("HEAD failed for fragment {index}: {error}"));
        }
        self.condvar.notify_all();
    }
}

fn spawn_get_worker(shared: Arc<ReaderShared>) {
    std::thread::spawn(move || {
        while !shared.cancelled.load(Ordering::SeqCst) {
            let (index, data) = {
                let mut state = shared.lock();
                if state.error.is_some() {
                    return;
                }
                let Some(work) = state.next_get() else {
                    state.complete = true;
                    shared.condvar.notify_all();
                    return;
                };
                work
            };

            let result = shared.cache.get(&data);
            let mut state = shared.lock();
            match result {
                Ok(_) => state.check_probe(index),
                Err(error) => state.error = Some(error.to_string()),
            }
            shared.condvar.notify_all();
        }
    });
}

fn spawn_head_worker(shared: Arc<ReaderShared>) {
    std::thread::spawn(move || {
        loop {
            if shared.cancelled.load(Ordering::SeqCst) {
                return;
            }

            // Exit and spawn decisions share the lock so start/stop cannot
            // leave zero or two workers.
            let Some((index, data)) = ({
                let mut inner = shared.lock();
                let next = if inner.eager_head && inner.error.is_none() {
                    inner.next_head()
                } else {
                    None
                };
                if next.is_none() {
                    inner.head_worker_running = false;
                }
                next
            }) else {
                return;
            };

            // After a failure, `next_head` is `None` and the worker exits.
            shared.probe(index, &data);
        }
    });
}

impl Read for MpegStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let fragment_count = {
                let inner = self.shared.lock();
                inner.fragments.len()
            };
            if self.fragment_index >= fragment_count {
                let _inner = self.wait_until(|inner| inner.complete)?;
                return Ok(0);
            }

            self.prioritize_fragment(self.fragment_index);

            let index = self.fragment_index;
            let offset = self.offset_in_fragment;
            let (copied, fragment_len) = {
                let inner =
                    self.wait_until(|inner| !inner.missing_body(index) || inner.complete)?;
                let Some(fragment) = inner.fragments[index].body() else {
                    return Ok(0);
                };
                let fragment_len = fragment.len();
                if offset >= fragment_len {
                    (0, fragment_len)
                } else {
                    let copied = (fragment_len - offset).min(buf.len());
                    buf[..copied].copy_from_slice(&fragment[offset..offset + copied]);
                    (copied, fragment_len)
                }
            };

            if offset >= fragment_len {
                self.fragment_index += 1;
                self.offset_in_fragment = 0;
                self.publish_cursor();
                continue;
            }

            self.offset_in_fragment += copied;
            if self.offset_in_fragment >= fragment_len {
                self.fragment_index += 1;
                self.offset_in_fragment = 0;
                self.publish_cursor();
            }
            if let Some(position) = self.byte_position {
                self.byte_position = Some(position + copied as u64);
            }
            return Ok(copied);
        }
    }
}

impl Seek for MpegStreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.seek_bytes(from)?
            .byte
            .ok_or_else(|| io::Error::other("MPEG stream byte position is unknown"))
    }
}

impl Drop for MpegStreamReader {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.shared.condvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Duration;

    /// Adapts GET and HEAD closures to [`Transport`].
    struct FnTransport<G, H> {
        get: G,
        head: H,
    }

    impl<G, H> Transport for FnTransport<G, H>
    where
        G: Fn(&Fragment, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync,
        H: Fn(&Fragment) -> Result<u64, Error> + Send + Sync,
    {
        fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Arc<[u8]>, Error> {
            (self.get)(fragment, size_hint).map(Arc::from)
        }

        fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
            (self.head)(fragment)
        }
    }

    /// A reader fetching through GET and HEAD closures.
    fn closure_reader<G, H>(
        fragments: Vec<Fragment>,
        timeline: Option<MediaTimeline>,
        eager_head: bool,
        get: G,
        head: H,
    ) -> Result<MpegStreamReader, Error>
    where
        G: Fn(&Fragment, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync + 'static,
        H: Fn(&Fragment) -> Result<u64, Error> + Send + Sync + 'static,
    {
        let transport = Arc::new(FnTransport { get, head });
        MpegStreamReader::new_with_transport(fragments, timeline, eager_head, transport)
    }

    fn position(byte: Option<u64>, time: Option<(Duration, TimeAccuracy)>) -> Position {
        Position {
            byte,
            time: time.map(|(timestamp, accuracy)| TimePosition {
                timestamp,
                accuracy,
            }),
        }
    }

    fn instant_get(
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> impl Fn(&Fragment, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |fragment: &Fragment, _expected_len: Option<u64>| {
            let url = fragment.url.clone();
            payloads
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, bytes)| bytes.to_vec())
                .ok_or_else(|| Error::StreamInitializationError(format!("unknown url {url}")))
        }
    }

    fn instant_head(
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> impl Fn(&Fragment) -> Result<u64, Error> + Send + Sync {
        move |fragment: &Fragment| {
            let url = fragment.url.clone();
            payloads
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, bytes)| bytes.len() as u64)
                .ok_or_else(|| Error::StreamInitializationError(format!("unknown url {url}")))
        }
    }

    fn failing_head() -> impl Fn(&Fragment) -> Result<u64, Error> + Send + Sync {
        move |_fragment: &Fragment| Err(Error::StreamInitializationError("HEAD unavailable".into()))
    }

    fn start_instant(
        urls: Vec<Fragment>,
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> Result<MpegStreamReader, Error> {
        closure_reader(
            urls,
            None,
            true,
            instant_get(payloads),
            instant_head(payloads),
        )
    }

    fn gated_get(
        allow_rest: Arc<AtomicBool>,
        payload: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    ) -> impl Fn(&Fragment, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |fragment: &Fragment, _expected_len: Option<u64>| {
            let url = fragment.url.clone();
            if url != "init" {
                while !allow_rest.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(payload(&url))
        }
    }

    #[test]
    fn reads_init_then_media_fragments() {
        let mut reader = start_instant(
            vec!["init".into(), "a".into(), "b".into()],
            &[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BB")],
        )
        .unwrap();

        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"INITAAAABB");
    }

    #[test]
    fn blocks_until_later_fragments_arrive() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            vec!["init".into(), "more".into()],
            None,
            true,
            gated_get(allow_rest.clone(), |url| url.as_bytes().to_vec()),
            failing_head(),
        )
        .unwrap();

        let mut init = [0u8; 4];
        assert_eq!(reader.read(&mut init).unwrap(), 4);
        assert_eq!(&init, b"init");

        let handle = thread::spawn(move || {
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            rest
        });

        thread::sleep(Duration::from_millis(30));
        assert!(!handle.is_finished());

        allow_rest.store(true, Ordering::SeqCst);
        assert_eq!(handle.join().unwrap(), b"more");
    }

    #[test]
    fn seek_bytes_within_downloaded_data() {
        let mut reader = start_instant(
            vec!["init".into(), "body".into()],
            &[("init", b"0123"), ("body", b"4567")],
        )
        .unwrap();

        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"01234567");

        reader.seek_bytes(SeekFrom::Start(2)).unwrap();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"2345");

        reader.seek_bytes(SeekFrom::Current(-2)).unwrap();
        reader.read_exact(&mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], b"45");

        reader.seek_bytes(SeekFrom::End(-3)).unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, b"567");
    }

    #[test]
    fn seek_bytes_from_end_falls_back_without_head() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            vec!["init".into(), "tail".into()],
            None,
            true,
            gated_get(allow_rest.clone(), |url| vec![url.as_bytes()[0]; 4]),
            failing_head(),
        )
        .unwrap();

        let handle = thread::spawn(move || {
            reader.seek_bytes(SeekFrom::End(-2)).unwrap();
            let mut buf = [0u8; 2];
            reader.read_exact(&mut buf).unwrap();
            buf
        });

        thread::sleep(Duration::from_millis(30));
        assert!(!handle.is_finished());
        allow_rest.store(true, Ordering::SeqCst);

        assert_eq!(&handle.join().unwrap(), b"tt");
    }

    #[test]
    fn seek_bytes_uses_head_without_downloading_prefix() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let get_order = Arc::new(Mutex::new(Vec::new()));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            true,
            {
                let allow_get = allow_get.clone();
                let get_order = get_order.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    get_order.lock().unwrap().push(url.clone());
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            {
                let head_calls = head_calls.clone();
                move |fragment: &Fragment| {
                    let url = fragment.url.clone();
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        // Byte 6 is inside fragment "b" (init=4, a=4, b starts at 8... wait)
        // init=4, a=4, so offset 6 is in fragment a (bytes 4..8), offset 2 within a.
        // Use offset 9 to land in b (4+4+1).
        assert_eq!(
            reader.seek_bytes(SeekFrom::Start(9)).unwrap(),
            position(Some(9), None)
        );
        assert!(
            get_order.lock().unwrap().iter().all(|url| url == "init"),
            "seek via HEAD must not GET intervening bodies yet: {:?}",
            get_order.lock().unwrap()
        );
        assert_eq!(head_calls.lock().unwrap().as_slice(), &["a", "b"]);

        allow_get.store(true, Ordering::SeqCst);
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"bb");
    }

    #[test]
    fn seek_bytes_disables_head_on_length_mismatch() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    match url.as_str() {
                        "init" => Ok(b"INIT".to_vec()),
                        "a" => Ok(b"AAAA".to_vec()),
                        "b" => Ok(b"BBBB".to_vec()),
                        other => Err(Error::StreamInitializationError(format!(
                            "unknown url {other}"
                        ))),
                    }
                }
            },
            move |fragment: &Fragment| {
                let url = fragment.url.clone();
                // Lie about "a" so GET will disagree after HEAD was used.
                match url.as_str() {
                    "init" => Ok(4),
                    "a" => Ok(99),
                    "b" => Ok(4),
                    other => Err(Error::StreamInitializationError(format!(
                        "unknown url {other}"
                    ))),
                }
            },
        )
        .unwrap();

        // HEAD reports a=99 before any media GET runs.
        reader.seek_bytes(SeekFrom::Start(5)).unwrap();
        allow_get.store(true, Ordering::SeqCst);
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"A");
        assert!(
            !reader.shared.lock().head_available,
            "HEAD should be disabled after GET/HEAD length mismatch"
        );
    }

    #[test]
    fn seek_bytes_current_requires_known_position() {
        let allow_a = Arc::new(AtomicBool::new(false));
        let allow_b = Arc::new(AtomicBool::new(false));
        let allow_c = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            Some(MediaTimeline {
                timescale: 1,
                media_durations: vec![1, 1, 1],
            }),
            true,
            {
                let allow_a = allow_a.clone();
                let allow_b = allow_b.clone();
                let allow_c = allow_c.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| match fragment.url.as_str() {
                    "init" => Ok(b"INIT".to_vec()),
                    "a" => {
                        while !allow_a.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(b"AAAA".to_vec())
                    }
                    "b" => {
                        while !allow_b.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(b"BBBB".to_vec())
                    }
                    "c" => {
                        while !allow_c.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(b"CCCC".to_vec())
                    }
                    other => Err(Error::StreamInitializationError(format!(
                        "unknown url {other}"
                    ))),
                }
            },
            failing_head(),
        )
        .unwrap();

        let handle = thread::spawn(move || {
            let position = reader.seek_time_coarse(Duration::from_secs(2)).unwrap();
            let error = reader.seek_bytes(SeekFrom::Current(1)).unwrap_err();
            (position, error.kind())
        });

        thread::sleep(Duration::from_millis(20));
        allow_a.store(true, Ordering::SeqCst);
        allow_c.store(true, Ordering::SeqCst);

        let (position, kind) = handle.join().unwrap();
        assert_eq!(
            position,
            Position {
                byte: None,
                time: Some(TimePosition {
                    timestamp: Duration::from_secs(2),
                    accuracy: TimeAccuracy::Exact,
                }),
            }
        );
        assert_eq!(kind, io::ErrorKind::InvalidInput);

        allow_b.store(true, Ordering::SeqCst);
    }

    #[test]
    fn seek_time_coarse_maps_timeline_to_fragment_starts() {
        let timeline = MediaTimeline {
            timescale: 100,
            media_durations: vec![100, 100],
        };
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            Some(timeline),
            true,
            instant_get(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
            instant_head(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
        )
        .unwrap();

        reader.read_to_end(&mut Vec::new()).unwrap();

        assert_eq!(
            reader.seek_time_coarse(Duration::from_secs(0)).unwrap(),
            position(Some(4), Some((Duration::ZERO, TimeAccuracy::Exact))),
            "time 0 is the first media fragment, after init"
        );
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"AAAA");

        assert_eq!(
            reader.seek_time_coarse(Duration::from_secs(1)).unwrap(),
            position(Some(8), Some((Duration::from_secs(1), TimeAccuracy::Exact)))
        );
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"BBBB");

        assert_eq!(
            reader.seek_time_coarse(Duration::from_millis(500)).unwrap(),
            position(Some(4), Some((Duration::ZERO, TimeAccuracy::Coarse))),
            "coarse seeks land at the containing fragment's moof"
        );
        reader.read_exact(&mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], b"AA");
    }

    #[test]
    fn seek_time_coarse_prioritizes_target_fragment() {
        let allow_a = Arc::new(AtomicBool::new(false));
        let allow_b = Arc::new(AtomicBool::new(false));
        let allow_c = Arc::new(AtomicBool::new(false));
        let a_started = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            Some(MediaTimeline {
                timescale: 1,
                media_durations: vec![1, 1, 1],
            }),
            true,
            {
                let allow_a = allow_a.clone();
                let allow_b = allow_b.clone();
                let allow_c = allow_c.clone();
                let a_started = a_started.clone();
                let order = order.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| match fragment.url.as_str() {
                    "init" => Ok(b"INIT".to_vec()),
                    "a" => {
                        a_started.store(true, Ordering::SeqCst);
                        while !allow_a.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        order.lock().unwrap().push("a");
                        Ok(b"AAAA".to_vec())
                    }
                    "b" => {
                        while !allow_b.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        order.lock().unwrap().push("b");
                        Ok(b"BBBB".to_vec())
                    }
                    "c" => {
                        while !allow_c.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        order.lock().unwrap().push("c");
                        Ok(b"CCCC".to_vec())
                    }
                    other => Err(Error::StreamInitializationError(format!(
                        "unknown url {other}"
                    ))),
                }
            },
            failing_head(),
        )
        .unwrap();
        // The worker must be busy with "a" before the seek, or it would take "c" first.
        while !a_started.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }

        let handle = thread::spawn(move || {
            let position = reader.seek_time_coarse(Duration::from_secs(2)).unwrap();
            let mut buf = [0u8; 4];
            reader.read_exact(&mut buf).unwrap();
            (position, buf)
        });

        thread::sleep(Duration::from_millis(30));
        assert!(!handle.is_finished());
        allow_a.store(true, Ordering::SeqCst);
        allow_c.store(true, Ordering::SeqCst);

        let (position, buf) = handle.join().unwrap();
        assert_eq!(
            position,
            Position {
                byte: None,
                time: Some(TimePosition {
                    timestamp: Duration::from_secs(2),
                    accuracy: TimeAccuracy::Exact,
                }),
            }
        );
        assert_eq!(&buf, b"CCCC");
        assert_eq!(order.lock().unwrap().as_slice(), &["a", "c"]);

        allow_b.store(true, Ordering::SeqCst);
    }

    #[test]
    fn get_worker_fills_ahead_of_cursor_before_backfill() {
        let release_media = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            vec![
                "init".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
            ],
            Some(MediaTimeline {
                timescale: 1,
                media_durations: vec![1, 1, 1, 1],
            }),
            true,
            {
                let release_media = release_media.clone();
                let order = order.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !release_media.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    order.lock().unwrap().push(url.clone());
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            failing_head(),
        )
        .unwrap();

        let cache = reader.shared.clone();
        let release_media_when_seeked = release_media.clone();
        thread::spawn(move || {
            loop {
                if cache.lock().cursor_fragment >= 3 {
                    release_media_when_seeked.store(true, Ordering::SeqCst);
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        reader.seek_time_coarse(Duration::from_secs(2)).unwrap();
        let _inner = reader.wait_until(|inner| inner.complete).unwrap();

        let got = order.lock().unwrap().clone();
        let b = got.iter().position(|url| url == "b").unwrap();
        let c = got.iter().position(|url| url == "c").unwrap();
        let d = got.iter().position(|url| url == "d").unwrap();
        assert!(
            c < b && d < b,
            "suffix ahead of the playhead must download before backfill: {got:?}"
        );
    }

    #[test]
    fn seek_time_coarse_returns_known_when_prefix_filled() {
        let timeline = MediaTimeline {
            timescale: 1,
            media_durations: vec![1, 1],
        };
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            Some(timeline),
            true,
            instant_get(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
            instant_head(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
        )
        .unwrap();

        reader.read_to_end(&mut Vec::new()).unwrap();
        assert_eq!(
            reader.seek_time_coarse(Duration::from_secs(1)).unwrap(),
            position(Some(8), Some((Duration::from_secs(1), TimeAccuracy::Exact)))
        );
    }

    #[test]
    fn seek_bytes_backfills_leading_fragments_without_head() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            vec!["init".into(), "aaaa".into(), "bbbb".into()],
            None,
            true,
            gated_get(allow_rest.clone(), |url| vec![url.as_bytes()[0]; 4]),
            failing_head(),
        )
        .unwrap();

        let handle = thread::spawn(move || {
            reader.seek_bytes(SeekFrom::Start(6)).unwrap();
            let mut buf = [0u8; 2];
            reader.read_exact(&mut buf).unwrap();
            buf
        });

        thread::sleep(Duration::from_millis(30));
        assert!(!handle.is_finished());
        allow_rest.store(true, Ordering::SeqCst);

        assert_eq!(&handle.join().unwrap(), b"aa");
    }

    #[test]
    fn seek_time_coarse_requires_timeline() {
        let mut reader = start_instant(vec!["init".into()], &[("init", b"xyz")]).unwrap();
        let error = reader.seek_time_coarse(Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn eof_after_complete_download() {
        let mut reader = start_instant(vec!["init".into()], &[("init", b"xyz")]).unwrap();

        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn surfaces_download_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let get = {
            let calls = calls.clone();
            move |fragment: &Fragment, _expected_len: Option<u64>| {
                let url = fragment.url.clone();
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(b"INIT".to_vec())
                } else {
                    Err(Error::StreamInitializationError(format!(
                        "failed to fetch {url}"
                    )))
                }
            }
        };

        let mut reader = closure_reader(
            vec!["init".into(), "bad".into()],
            None,
            true,
            get,
            failing_head(),
        )
        .unwrap();

        let mut buf = Vec::new();
        let error = loop {
            match reader.read_to_end(&mut buf) {
                Ok(_) => {
                    buf.clear();
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("failed to fetch bad"));
    }

    #[test]
    fn empty_url_list_is_rejected() {
        let result = closure_reader(Vec::new(), None, true, instant_get(&[]), failing_head());
        assert!(matches!(result, Err(Error::DashManifestMissingUrls)));
    }

    #[test]
    fn head_worker_fills_sizes_without_bodies() {
        let allow_late = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let get_expected = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            vec![
                "init".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
            ],
            None,
            true,
            {
                let allow_late = allow_late.clone();
                let get_expected = get_expected.clone();
                move |fragment: &Fragment, expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    get_expected
                        .lock()
                        .unwrap()
                        .push((url.clone(), expected_len));
                    if url == "d" || url == "e" {
                        while !allow_late.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            {
                let head_calls = head_calls.clone();
                move |fragment: &Fragment| {
                    let url = fragment.url.clone();
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        let inner = reader
            .wait_until(|inner| {
                inner.fragments[4].size().is_some() && inner.fragments[5].size().is_some()
            })
            .unwrap();
        assert!(
            inner.missing_body(4) && inner.missing_body(5),
            "HEAD worker should learn sizes before those bodies are fetched"
        );
        drop(inner);

        let heads = head_calls.lock().unwrap().clone();
        assert!(heads.contains(&"d".to_string()), "{heads:?}");
        assert!(heads.contains(&"e".to_string()), "{heads:?}");

        allow_late.store(true, Ordering::SeqCst);
        let _inner = reader.wait_until(|inner| inner.complete).unwrap();
        let expected = get_expected.lock().unwrap().clone();
        assert!(
            expected
                .iter()
                .any(|(url, len)| url == "e" && *len == Some(4)),
            "GET started after HEAD should receive the known length: {expected:?}"
        );
    }

    #[test]
    fn head_worker_heads_immediately_without_body_buffer() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            {
                let head_calls = head_calls.clone();
                move |fragment: &Fragment| {
                    let url = fragment.url.clone();
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        let inner = reader.wait_until(|inner| inner.all_sizes_known()).unwrap();
        assert!(
            (1..=3).all(|index| inner.missing_body(index)),
            "HEAD worker should fill sizes without waiting for a body buffer"
        );
        drop(inner);
        let heads = head_calls.lock().unwrap().clone();
        assert!(heads.contains(&"a".to_string()), "{heads:?}");
        assert!(heads.contains(&"b".to_string()), "{heads:?}");
        assert!(heads.contains(&"c".to_string()), "{heads:?}");

        allow_get.store(true, Ordering::SeqCst);
    }

    #[test]
    fn get_worker_proceeds_while_head_is_blocked() {
        let allow_heads = Arc::new(AtomicBool::new(false));
        let reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            None,
            true,
            instant_get(&[
                ("init", b"INIT"),
                ("a", b"AAAA"),
                ("b", b"BBBB"),
                ("c", b"CCCC"),
            ]),
            {
                let allow_heads = allow_heads.clone();
                move |_fragment: &Fragment| {
                    while !allow_heads.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Ok(4)
                }
            },
        )
        .unwrap();

        let _inner = reader.wait_until(|inner| inner.complete).unwrap();
        allow_heads.store(true, Ordering::SeqCst);
    }

    #[test]
    fn head_worker_failure_disables_head() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(AtomicUsize::new(0));
        let reader = closure_reader(
            vec![
                "init".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
            ],
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            {
                let head_calls = head_calls.clone();
                move |_fragment: &Fragment| {
                    head_calls.fetch_add(1, Ordering::SeqCst);
                    Err(Error::StreamInitializationError("HEAD unavailable".into()))
                }
            },
        )
        .unwrap();

        let inner = reader.wait_until(|inner| !inner.head_available).unwrap();
        assert!(
            head_calls.load(Ordering::SeqCst) >= 1,
            "HEAD worker should attempt probes after the initialization GET"
        );
        drop(inner);
        allow_get.store(true, Ordering::SeqCst);
        let _inner = reader.wait_until(|inner| inner.complete).unwrap();
    }

    fn recording_head(
        calls: Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(&Fragment) -> Result<u64, Error> + Send + Sync {
        move |fragment: &Fragment| {
            let url = fragment.url.clone();
            calls.lock().unwrap().push(url);
            Ok(4)
        }
    }

    #[test]
    fn stopped_eager_head_spawns_no_worker() {
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            false,
            instant_get(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
            recording_head(head_calls.clone()),
        )
        .unwrap();

        let inner = reader.wait_until(|inner| inner.complete).unwrap();
        assert!(!inner.eager_head && !inner.head_worker_running);
        drop(inner);
        assert!(head_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn start_eager_head_fills_sizes_after_stopped_construction() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            None,
            false,
            {
                let allow_get = allow_get.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            recording_head(head_calls.clone()),
        )
        .unwrap();

        thread::sleep(Duration::from_millis(20));
        assert!(head_calls.lock().unwrap().is_empty());

        reader.start_eager_head();
        let inner = reader.wait_until(|inner| inner.all_sizes_known()).unwrap();
        assert!(
            inner.fragments[1..]
                .iter()
                .all(|data| data.body().is_none())
        );
        drop(inner);
        let _inner = reader
            .wait_until(|inner| !inner.head_worker_running)
            .unwrap();
        assert_eq!(head_calls.lock().unwrap().as_slice(), &["a", "b", "c"]);

        allow_get.store(true, Ordering::SeqCst);
    }

    #[test]
    fn stop_eager_head_halts_after_in_flight_request() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let head_entered = Arc::new(AtomicUsize::new(0));
        let release_head = Arc::new(AtomicBool::new(false));
        let reader = closure_reader(
            vec![
                "init".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
            ],
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            {
                let head_entered = head_entered.clone();
                let release_head = release_head.clone();
                move |_fragment: &Fragment| {
                    head_entered.fetch_add(1, Ordering::SeqCst);
                    while !release_head.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Ok(4)
                }
            },
        )
        .unwrap();

        while head_entered.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(5));
        }
        reader.stop_eager_head();
        release_head.store(true, Ordering::SeqCst);

        let inner = reader
            .wait_until(|inner| !inner.head_worker_running)
            .unwrap();
        assert_eq!(head_entered.load(Ordering::SeqCst), 1);
        assert_eq!(
            inner.fragments[1].size(),
            Some(4),
            "in-flight HEAD should be kept"
        );
        assert!(
            inner.fragments[2..]
                .iter()
                .all(|data| data.size().is_none())
        );
        drop(inner);

        allow_get.store(true, Ordering::SeqCst);
    }

    #[test]
    fn restart_during_in_flight_head_reuses_worker() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let release_head = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |fragment: &Fragment, _expected_len: Option<u64>| {
                    let url = fragment.url.clone();
                    if url != "init" {
                        while !allow_get.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(vec![url.as_bytes()[0]; 4])
                }
            },
            {
                let release_head = release_head.clone();
                let active = active.clone();
                let max_active = max_active.clone();
                move |_fragment: &Fragment| {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    while !release_head.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(4)
                }
            },
        )
        .unwrap();

        while active.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..5 {
            reader.stop_eager_head();
            reader.start_eager_head();
        }
        release_head.store(true, Ordering::SeqCst);

        let _inner = reader.wait_until(|inner| inner.all_sizes_known()).unwrap();
        assert_eq!(max_active.load(Ordering::SeqCst), 1);

        allow_get.store(true, Ordering::SeqCst);
    }

    #[test]
    fn start_eager_head_is_noop_when_head_disabled() {
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            vec!["init".into(), "a".into()],
            None,
            false,
            gated_get(Arc::new(AtomicBool::new(false)), |url| {
                url.as_bytes().to_vec()
            }),
            recording_head(head_calls.clone()),
        )
        .unwrap();

        reader.shared.lock().disable_head("test");
        reader.start_eager_head();
        assert!(!reader.shared.lock().head_worker_running);
        assert!(head_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn stopped_eager_head_still_heads_on_demand_for_seeks() {
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            false,
            gated_get(Arc::new(AtomicBool::new(false)), |url| {
                url.as_bytes().to_vec()
            }),
            recording_head(head_calls.clone()),
        )
        .unwrap();

        let position = reader.seek_bytes(SeekFrom::End(0)).unwrap();
        assert_eq!(position.byte, Some(4 + 4 + 4));
        assert_eq!(head_calls.lock().unwrap().as_slice(), &["a", "b"]);
        assert!(!reader.shared.lock().head_worker_running);
    }

    fn two_second_timeline() -> MediaTimeline {
        MediaTimeline {
            timescale: 44_100,
            media_durations: vec![44_100, 44_100],
        }
    }

    #[test]
    fn estimate_len_errs_larger_than_advertised_media() {
        let timeline = two_second_timeline();
        let init_len = 4;
        let bitrate = 128_000;
        let raw_media = u64::from(bitrate) * 2 / 8;
        let estimate = timeline.estimate_len(init_len, bitrate).unwrap();
        assert!(
            estimate >= init_len + raw_media * 4 + 2 * 64 * 1024,
            "estimate {estimate} should pad raw media {raw_media}"
        );
        assert!(timeline.estimate_len(init_len, 0).is_none());
    }

    #[test]
    fn symphonia_compat_end_zero_does_not_wait_for_bodies() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            vec!["init".into(), "a".into(), "b".into()],
            Some(two_second_timeline()),
            true,
            gated_get(allow_rest.clone(), |url| vec![url.as_bytes()[0]; 4]),
            failing_head(),
        )
        .unwrap();
        reader.set_symphonia_compat(true, 128_000).unwrap();

        let end = reader.seek_bytes(SeekFrom::End(0)).unwrap();
        let estimate = two_second_timeline().estimate_len(4, 128_000).unwrap();
        assert_eq!(end.byte, Some(estimate));
        assert_eq!(
            end.time,
            Some(TimePosition {
                timestamp: Duration::from_secs(2),
                accuracy: TimeAccuracy::Exact,
            })
        );

        reader.seek_bytes(SeekFrom::Start(0)).unwrap();
        let mut init = [0u8; 4];
        assert_eq!(reader.read(&mut init).unwrap(), 4);
        assert_eq!(&init, b"iiii");

        assert!(
            !allow_rest.load(Ordering::SeqCst),
            "End(0) must not wait on media bodies"
        );
    }

    #[test]
    fn symphonia_compat_end_zero_uses_exact_len_once_known() {
        let mut reader = closure_reader(
            vec!["init".into(), "a".into()],
            Some(two_second_timeline()),
            true,
            instant_get(&[("init", b"INIT"), ("a", b"AAAA")]),
            instant_head(&[("init", b"INIT"), ("a", b"AAAA")]),
        )
        .unwrap();
        reader.set_symphonia_compat(true, 1_411_200).unwrap();
        drop(reader.wait_until(|inner| inner.all_sizes_known()).unwrap());

        let end = reader.seek_bytes(SeekFrom::End(0)).unwrap();
        assert_eq!(end.byte, Some(8));
        assert!(
            end.byte.unwrap() < reader.length_estimate.unwrap(),
            "exact total should replace the padded estimate"
        );
    }

    #[test]
    fn symphonia_compat_requires_timeline() {
        let mut reader = start_instant(
            vec!["init".into(), "a".into()],
            &[("init", b"INIT"), ("a", b"AAAA")],
        )
        .unwrap();
        let error = reader.set_symphonia_compat(true, 128_000).unwrap_err();
        assert!(
            matches!(error, Error::StreamInitializationError(message) if message.contains("SegmentTimeline"))
        );
    }

    #[test]
    fn byte_range_parses_dash_form() {
        let range = parse_byte_range(" 100-199 ").unwrap();
        assert_eq!(range, 100..=199);
        assert_eq!(
            Fragment::with_range("a", range.clone()).known_size(),
            Some(100)
        );
        assert_eq!(Fragment::with_range("a", 5..=5).known_size(), Some(1));
        for invalid in ["", "5", "5-", "-5", "9-3", "a-b"] {
            assert!(
                matches!(parse_byte_range(invalid), Err(Error::InvalidByteRange(value)) if value == invalid),
                "{invalid:?} should be rejected"
            );
        }
        assert_eq!(
            Fragment::with_range("media.mp4", range).to_string(),
            "media.mp4 (bytes 100-199)"
        );
    }

    /// Serves byte ranges of one in-memory file, keyed by fragment range.
    fn ranged_get(
        file: &'static [u8],
    ) -> impl Fn(&Fragment, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |fragment: &Fragment, _expected_len: Option<u64>| {
            let range = fragment.range.as_ref().expect("test fragments are ranged");
            Ok(file[*range.start() as usize..=*range.end() as usize].to_vec())
        }
    }

    fn ranged(url: &str, start: u64, end: u64) -> Fragment {
        Fragment::with_range(url, start..=end)
    }

    #[test]
    fn ranged_fragments_know_sizes_without_head() {
        const FILE: &[u8] = b"INITaaaaBBBBBBcc";
        let allow_media = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            vec![
                ranged("file", 0, 3),
                ranged("file", 4, 7),
                ranged("file", 8, 13),
                ranged("file", 14, 15),
            ],
            None,
            true,
            {
                let allow_media = allow_media.clone();
                let get = ranged_get(FILE);
                move |fragment: &Fragment, expected_len: Option<u64>| {
                    assert_eq!(expected_len, fragment.known_size());
                    if *fragment.range.as_ref().unwrap().start() > 0 {
                        while !allow_media.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    get(fragment, expected_len)
                }
            },
            recording_head(head_calls.clone()),
        )
        .unwrap();

        // Every size comes from a range, so byte seeks resolve before any media body arrives.
        assert_eq!(reader.seek_bytes(SeekFrom::End(0)).unwrap().byte, Some(16));
        assert_eq!(reader.seek_bytes(SeekFrom::Start(9)).unwrap().byte, Some(9));
        assert!(reader.shared.lock().missing_body(2));

        allow_media.store(true, Ordering::SeqCst);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"BBBBBcc");
        reader.seek_bytes(SeekFrom::Start(0)).unwrap();
        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        assert_eq!(all, FILE);
        assert!(head_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn ranged_fragment_with_wrong_length_fails() {
        let result = closure_reader(
            vec![ranged("file", 0, 7)],
            None,
            false,
            |_fragment: &Fragment, _expected_len: Option<u64>| Ok(b"INIT".to_vec()),
            failing_head(),
        );
        assert!(matches!(
            result,
            Err(Error::FragmentLength { fragment, expected: 8, actual: 4 })
                if fragment == "file (bytes 0-7)"
        ));

        let mut reader = closure_reader(
            vec![ranged("file", 0, 3), ranged("file", 4, 7)],
            None,
            false,
            |fragment: &Fragment, _expected_len: Option<u64>| match *fragment
                .range
                .as_ref()
                .unwrap()
                .start()
            {
                0 => Ok(b"INIT".to_vec()),
                _ => Ok(b"short".to_vec()),
            },
            failing_head(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        let error = reader.read_to_end(&mut bytes).unwrap_err();
        assert!(error.to_string().contains("returned 5 bytes, expected 4"));
    }

    /// Serves `FILE` over HTTP, answering `Range` requests with 206 on `/ranged`
    /// and ignoring them (200, full body) on `/plain`. Returns the base URL.
    fn spawn_range_server(connections: usize) -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        const FILE: &[u8] = b"INITaaaaBBBBBBcc";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for _ in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut range = None;
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = Some(parse_byte_range(value).unwrap());
                    }
                    header.clear();
                }
                let (status, body) = match range {
                    Some(range) if request_line.starts_with("GET /ranged ") => (
                        "206 Partial Content",
                        &FILE[*range.start() as usize..=*range.end() as usize],
                    ),
                    _ => ("200 OK", FILE),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let stream = reader.get_mut();
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn default_transport_sends_range_and_slices_ignored_ranges() {
        let base = spawn_range_server(3);
        let mut reader = MpegStreamReader::new(
            vec![
                ranged(&format!("{base}/ranged"), 0, 3),
                ranged(&format!("{base}/plain"), 8, 13),
                ranged(&format!("{base}/ranged"), 14, 15),
            ],
            None,
            false,
        )
        .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"INITBBBBBBcc");
    }

    /// Records GETs and HEADs (bodies are the URL bytes) and blocks GETs of
    /// `gated` URLs until released.
    struct CountingTransport {
        gets: Mutex<Vec<String>>,
        heads: Mutex<Vec<String>>,
        gated: &'static [&'static str],
        released: AtomicBool,
    }

    impl CountingTransport {
        fn new(gated: &'static [&'static str]) -> Self {
            Self {
                gets: Mutex::new(Vec::new()),
                heads: Mutex::new(Vec::new()),
                gated,
                released: AtomicBool::new(false),
            }
        }

        fn gets(&self) -> Vec<String> {
            self.gets.lock().unwrap().clone()
        }

        fn heads(&self) -> Vec<String> {
            self.heads.lock().unwrap().clone()
        }
    }

    impl Transport for CountingTransport {
        fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
            self.heads.lock().unwrap().push(fragment.url.clone());
            Ok(fragment.url.len() as u64)
        }

        fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<Arc<[u8]>, Error> {
            self.gets.lock().unwrap().push(fragment.url.clone());
            if self.gated.contains(&fragment.url.as_str()) {
                while !self.released.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(fragment.url.as_bytes().into())
        }
    }

    fn wait_for(mut ready: impl FnMut() -> bool) {
        for _ in 0..400 {
            if ready() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("condition not reached within 2s");
    }

    /// A cache over a fresh [`CountingTransport`], and that transport.
    fn counting_cache(
        gated: &'static [&'static str],
    ) -> (Arc<FragmentCache>, Arc<CountingTransport>) {
        let transport = Arc::new(CountingTransport::new(gated));
        (Arc::new(FragmentCache::new(transport.clone())), transport)
    }

    #[test]
    fn fragment_cache_reuses_bodies_across_readers() {
        let (cache, transport) = counting_cache(&[]);
        let mut first = MpegStreamReader::new_with_cache(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            false,
            cache.clone(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        first.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initab");

        // A rebuilt reader, as after a seek, gets everything from memory.
        let mut second = MpegStreamReader::new_with_cache(
            vec!["init".into(), "b".into()],
            None,
            true,
            cache.clone(),
        )
        .unwrap();
        bytes.clear();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
        assert_eq!(cache.known_size(&"a".into()), Some(1));
        assert!(transport.heads().is_empty());
    }

    #[test]
    fn fragment_cache_keeps_in_flight_bodies_after_reader_drop() {
        let (cache, transport) = counting_cache(&["b"]);
        let first = MpegStreamReader::new_with_cache(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            false,
            cache.clone(),
        )
        .unwrap();
        wait_for(|| transport.gets().contains(&"b".to_owned()));
        drop(first);

        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| cache.cached(&"b".into()).is_some());

        let mut second = MpegStreamReader::new_with_cache(
            vec!["init".into(), "b".into()],
            None,
            false,
            cache.clone(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
    }

    #[test]
    fn fragment_cache_joins_a_get_already_in_flight() {
        let (cache, transport) = counting_cache(&["b"]);
        let first = MpegStreamReader::new_with_cache(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            false,
            cache.clone(),
        )
        .unwrap();
        wait_for(|| transport.gets().contains(&"b".to_owned()));
        drop(first);

        // The rebuilt reader waits on the dropped reader's GET of "b".
        let mut second = MpegStreamReader::new_with_cache(
            vec!["init".into(), "b".into()],
            None,
            false,
            cache.clone(),
        )
        .unwrap();
        transport.released.store(true, Ordering::SeqCst);
        let mut bytes = Vec::new();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
    }

    #[test]
    fn fragment_cache_skips_wrong_length_ranged_bodies() {
        let cache = FragmentCache::new(Arc::new(FnTransport {
            get: |_fragment: &Fragment, _size_hint: Option<u64>| Ok(b"short".to_vec()),
            head: |_fragment: &Fragment| Ok(0),
        }));
        let fragment = ranged("file", 0, 7);
        assert!(matches!(
            cache.get(&cache.data(fragment.clone())),
            Err(Error::FragmentLength {
                expected: 8,
                actual: 5,
                ..
            })
        ));
        assert_eq!(cache.cached(&fragment), None);

        let whole = Fragment::new("file");
        cache.get(&cache.data(whole.clone())).unwrap();
        assert_eq!(cache.cached(&whole).as_deref(), Some(&b"short"[..]));
    }

    /// Serves each path's name as its body with HTTP keep-alive. Returns the
    /// base URL and a count of accepted TCP connections.
    fn spawn_keep_alive_server() -> (String, Arc<AtomicUsize>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = connections.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                accepted.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.unwrap());
                    loop {
                        let mut request_line = String::new();
                        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut header = String::new();
                        while reader.read_line(&mut header).unwrap() > 2 {
                            header.clear();
                        }
                        let path = request_line.split(' ').nth(1).unwrap();
                        let body = path.trim_start_matches('/');
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        reader.get_mut().write_all(response.as_bytes()).unwrap();
                    }
                });
            }
        });
        (base, connections)
    }

    #[test]
    fn fragment_cache_reuses_pooled_connections_across_readers() {
        let (base, connections) = spawn_keep_alive_server();
        let fragments = |paths: &[&str]| -> Vec<Fragment> {
            paths
                .iter()
                .map(|path| format!("{base}/{path}").into())
                .collect()
        };
        let read_all = |mut reader: MpegStreamReader| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        };

        let cache = Arc::new(FragmentCache::default());
        let first =
            MpegStreamReader::new_with_cache(fragments(&["init", "a"]), None, false, cache.clone())
                .unwrap();
        assert_eq!(read_all(first), b"inita");
        let second =
            MpegStreamReader::new_with_cache(fragments(&["init", "b"]), None, false, cache.clone())
                .unwrap();
        assert_eq!(read_all(second), b"initb");
        assert_eq!(connections.load(Ordering::SeqCst), 1);

        // Separate agents cannot share the connection.
        let third = MpegStreamReader::new(fragments(&["init", "c"]), None, false).unwrap();
        assert_eq!(read_all(third), b"initc");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fragment_cache_remembers_head_lengths_across_readers() {
        let (cache, transport) = counting_cache(&["a", "bb"]);
        let fragments = || vec!["init".into(), "a".into(), "bb".into()];
        let first =
            MpegStreamReader::new_with_cache(fragments(), None, true, cache.clone()).unwrap();
        wait_for(|| cache.known_size(&"bb".into()).is_some());
        drop(first);

        // The rebuilt reader starts with every size and sends no HEAD at all.
        let mut second =
            MpegStreamReader::new_with_cache(fragments(), None, true, cache.clone()).unwrap();
        assert!(second.shared.lock().all_sizes_known());
        assert_eq!(second.seek_bytes(SeekFrom::End(0)).unwrap().byte, Some(7));
        assert_eq!(transport.heads(), ["a", "bb"]);
        assert_eq!(cache.cached(&"bb".into()), None);

        // A later GET's body takes precedence over the HEAD length.
        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| cache.cached(&"bb".into()).is_some());
        assert_eq!(cache.known_size(&"bb".into()), Some(2));
    }
}
