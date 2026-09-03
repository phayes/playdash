//! Progressive in-memory reader over MPEG-DASH fragments.

use crate::error::Error;
use log::warn;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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
        let media = (u128::from(bitrate_bps) * ticks + (denom - 1)) / denom;
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

type GetFn = Arc<dyn Fn(String, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync>;
type HeadFn = Arc<dyn Fn(String) -> Result<u64, Error> + Send + Sync>;

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

/// Progressive in-memory reader over MPEG-DASH fragments.
///
/// Fragments are stored sparsely (`Option` per URL). The initialization
/// segment is index `0`, followed by media segments in `SegmentTimeline`
/// order. [`Read`] blocks until the current fragment slot is filled.
///
/// Use [`Self::seek_bytes`] for exact offsets in the concatenated byte
/// stream. When HEAD probes are available, `seek_bytes` discovers fragment
/// lengths without downloading skipped bodies; if HEAD fails or disagrees
/// with a later GET, the reader falls back to waiting on downloads. After
/// the initialization GET, a dedicated HEAD worker fills remaining sizes
/// on a second HTTP connection while the GET worker downloads bodies.
/// Use [`Self::seek_time_coarse`] to jump to the beginning of the media
/// fragment containing a timestamp — the GET worker fills from the playhead
/// forward, then backfills earlier holes. Returned [`Position`] values include
/// both the known absolute byte offset and an exact or coarse media timestamp.
/// Enable [`Self::set_symphonia_compat`] so [`SeekFrom::End`]`(0)` can
/// return an oversized length estimate without waiting for every fragment.
#[derive(Debug)]
pub struct MpegStreamReader {
    cache: Arc<MpegCache>,
    byte_position: Option<u64>,
    fragment_index: usize,
    offset_in_fragment: usize,
    timeline: Option<MediaTimeline>,
    /// When set, [`SeekFrom::End`]`(0)` may report [`Self::length_estimate`]
    /// instead of waiting for every fragment size.
    symphonia_compat: bool,
    length_estimate: Option<u64>,
}

struct MpegCache {
    inner: Mutex<MpegCacheInner>,
    condvar: Condvar,
    cancelled: AtomicBool,
    head: HeadFn,
}

impl std::fmt::Debug for MpegCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpegCache")
            .field("cancelled", &self.cancelled)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct MpegCacheInner {
    urls: Vec<String>,
    fragments: Vec<Option<Vec<u8>>>,
    /// Known byte length per fragment (from HEAD and/or GET).
    sizes: Vec<Option<u64>>,
    /// When false, byte seeks wait on downloaded bodies instead of HEAD.
    head_available: bool,
    /// Read/seek playhead. The GET worker fills at or after this index first.
    cursor_fragment: usize,
    priority: Option<usize>,
    complete: bool,
    error: Option<String>,
}

impl MpegCacheInner {
    fn all_filled(&self) -> bool {
        self.fragments.iter().all(|fragment| fragment.is_some())
    }

    fn all_sizes_known(&self) -> bool {
        self.sizes.iter().all(|size| size.is_some())
    }

    fn disable_head(&mut self, reason: &str) {
        if self.head_available {
            self.head_available = false;
            warn!("MPEG stream disabling HEAD size probes: {reason}");
        }
    }

    fn next_get(&mut self) -> Option<(usize, String, Option<u64>)> {
        if let Some(priority) = self.priority.take()
            && self
                .fragments
                .get(priority)
                .is_some_and(|fragment| fragment.is_none())
        {
            return Some(self.get_work(priority));
        }
        let cursor = self.cursor_fragment.min(self.fragments.len());
        if let Some(offset) = self.fragments[cursor..]
            .iter()
            .position(|fragment| fragment.is_none())
        {
            return Some(self.get_work(cursor + offset));
        }
        self.fragments[..cursor]
            .iter()
            .position(|fragment| fragment.is_none())
            .map(|index| self.get_work(index))
    }

    fn get_work(&self, index: usize) -> (usize, String, Option<u64>) {
        (index, self.urls[index].clone(), self.sizes[index])
    }

    fn next_head(&self) -> Option<(usize, String)> {
        if !self.head_available {
            return None;
        }
        self.sizes
            .iter()
            .position(|size| size.is_none())
            .map(|index| (index, self.urls[index].clone()))
    }

    fn total_len(&self) -> Option<u64> {
        if !self.all_sizes_known() {
            return None;
        }
        Some(self.sizes.iter().map(|size| size.unwrap_or(0)).sum())
    }

    // TODO: Publish a media fragment's leading `moof` as soon as those bytes
    // have arrived, instead of waiting for the trailing `mdat`. IsoMp4 open
    // and the first packet only need the `moof`; `Read` currently blocks until
    // this method sees the whole GET. Stream the body, parse top-level box
    // sizes, and wake readers once the `moof` is complete.
    fn store_fragment(&mut self, index: usize, chunk: Vec<u8>) {
        let len = chunk.len() as u64;
        if let Some(expected) = self.sizes[index]
            && expected != len
        {
            self.disable_head(&format!(
                "GET length {len} != HEAD length {expected} for fragment {index}"
            ));
        }
        self.sizes[index] = Some(len);
        if self.fragments[index].is_none() {
            self.fragments[index] = Some(chunk);
        }
    }

    /// Maps an absolute byte offset onto a fragment cursor once contiguous
    /// sizes covering that offset are known.
    fn resolve_cursor(&self, target: u64) -> Option<(usize, usize)> {
        let mut acc = 0u64;
        for (index, size) in self.sizes.iter().enumerate() {
            let Some(len) = *size else {
                return None;
            };
            if target < acc + len {
                return Some((index, (target - acc) as usize));
            }
            acc += len;
        }

        if self.sizes.is_empty() {
            return Some((0, 0));
        }
        let last = self.sizes.len() - 1;
        let len = self.sizes[last].unwrap_or(0) as usize;
        Some((last, len))
    }
}

impl MpegStreamReader {
    /// Downloads the first fragment with the default ureq transport, then fills
    /// remaining slots on background threads.
    ///
    /// The initialization GET runs first so its TLS session is established
    /// before any other request. A GET worker then downloads bodies at or
    /// ahead of the read cursor, and only backfills earlier holes when the
    /// suffix is complete. A second ureq agent HEADs remaining URLs on its
    /// own connection to fill the size list. If HEAD fails or later
    /// disagrees with GET, HEAD probing is disabled.
    pub fn new(urls: Vec<String>, timeline: Option<MediaTimeline>) -> Result<Self, Error> {
        let get_agent = ureq_agent();
        let head_agent = ureq_agent();
        Self::new_with_get_head(
            urls,
            timeline,
            move |url, expected_len| {
                let mut bytes = expected_len
                    .and_then(|len| usize::try_from(len).ok())
                    .map(Vec::with_capacity)
                    .unwrap_or_default();
                get_agent
                    .get(&url)
                    .call()?
                    .into_body()
                    .into_reader()
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            },
            move |url| {
                let response = head_agent.head(&url).call()?;
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
            },
        )
    }

    /// Creates a reader with custom GET and HEAD implementations.
    ///
    /// `head` should return the fragment byte length (typically from
    /// `Content-Length`). After the initialization GET, a dedicated HEAD
    /// worker rips through unknown sizes. If HEAD fails or later disagrees
    /// with GET, HEAD probing is disabled for the rest of the stream.
    ///
    /// `get` receives that known length as `Some` when a prior HEAD already
    /// filled `sizes`; use it to preallocate the body buffer.
    pub fn new_with_get_head<G, H>(
        urls: Vec<String>,
        timeline: Option<MediaTimeline>,
        get: G,
        head: H,
    ) -> Result<Self, Error>
    where
        G: Fn(String, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync + 'static,
        H: Fn(String) -> Result<u64, Error> + Send + Sync + 'static,
    {
        if urls.is_empty() {
            return Err(Error::DashManifestMissingUrls);
        }

        let get: GetFn = Arc::new(get);
        let head: HeadFn = Arc::new(head);
        let first = get(urls[0].clone(), None)?;
        let first_len = first.len() as u64;
        let mut fragments = vec![None; urls.len()];
        let mut sizes = vec![None; urls.len()];
        fragments[0] = Some(first);
        sizes[0] = Some(first_len);
        let complete = fragments.iter().all(|fragment| fragment.is_some());

        let cache = Arc::new(MpegCache {
            inner: Mutex::new(MpegCacheInner {
                urls,
                fragments,
                sizes,
                head_available: true,
                cursor_fragment: 0,
                priority: None,
                complete,
                error: None,
            }),
            condvar: Condvar::new(),
            cancelled: AtomicBool::new(false),
            head,
        });

        if !complete {
            spawn_get_worker(cache.clone(), get);
            spawn_head_worker(cache.clone());
        }

        Ok(Self {
            cache,
            byte_position: Some(0),
            fragment_index: 0,
            offset_in_fragment: 0,
            timeline,
            symphonia_compat: false,
            length_estimate: None,
        })
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
            let inner = self.cache.lock();
            inner.sizes.first().copied().flatten().unwrap_or(0)
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
        let inner = self.cache.lock();
        if let Some(exact) = inner.total_len() {
            return Some(exact);
        }
        if !self.symphonia_compat {
            return None;
        }
        let known: u64 = inner.sizes.iter().copied().flatten().sum();
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
                self.ensure_all_sizes()?;
                let size = {
                    let inner = self.cache.lock();
                    inner.total_len().unwrap_or(0)
                };
                if offset >= 0 {
                    size.saturating_add(offset as u64)
                } else {
                    size.saturating_sub(offset.unsigned_abs())
                }
            }
        };

        self.ensure_sizes_covering(target)?;

        let (fragment_index, offset_in_fragment) = {
            let inner = self.cache.lock();
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
            let mut inner = self.cache.lock();
            if target_fragment >= inner.fragments.len() {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            }
            inner.cursor_fragment = target_fragment;
            if inner.fragments[target_fragment].is_none() {
                inner.priority = Some(target_fragment);
                self.cache.condvar.notify_all();
            }
        }

        let byte_position = {
            let inner = self.wait_until(|inner| {
                inner
                    .fragments
                    .get(target_fragment)
                    .is_some_and(|fragment| fragment.is_some())
                    || inner.complete
            })?;

            let Some(_fragment) = inner
                .fragments
                .get(target_fragment)
                .and_then(|f| f.as_ref())
            else {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            };

            let prefix_ready = inner.sizes[..target_fragment]
                .iter()
                .all(|size| size.is_some());
            if prefix_ready {
                let prefix: u64 = inner.sizes[..target_fragment]
                    .iter()
                    .map(|size| size.unwrap_or(0))
                    .sum();
                Some(prefix)
            } else {
                None
            }
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

    /// Fills unknown sizes needed to map `target`, via on-demand HEAD or GET fallback.
    fn ensure_sizes_covering(&self, target: u64) -> io::Result<()> {
        loop {
            let next_head = {
                let inner = self.cache.lock();
                if inner.resolve_cursor(target).is_some() {
                    return Ok(());
                }
                if !inner.head_available {
                    None
                } else {
                    inner
                        .sizes
                        .iter()
                        .position(|size| size.is_none())
                        .map(|index| (index, inner.urls[index].clone()))
                }
            };

            let Some((index, url)) = next_head else {
                break;
            };

            match (self.cache.head)(url) {
                Ok(len) => {
                    let mut inner = self.cache.lock();
                    if inner.sizes[index].is_none() {
                        inner.sizes[index] = Some(len);
                    }
                    self.cache.condvar.notify_all();
                }
                Err(error) => {
                    let mut inner = self.cache.lock();
                    inner.disable_head(&format!("HEAD failed for fragment {index}: {error}"));
                    break;
                }
            }
        }

        {
            let mut inner = self.cache.lock();
            if inner.resolve_cursor(target).is_some() {
                return Ok(());
            }
            if let Some(index) = inner
                .fragments
                .iter()
                .position(|fragment| fragment.is_none())
            {
                inner.priority = Some(index);
                self.cache.condvar.notify_all();
            }
        }

        let inner =
            self.wait_until(|inner| inner.resolve_cursor(target).is_some() || inner.complete)?;
        if inner.resolve_cursor(target).is_none() && !inner.complete {
            return Err(io::Error::other(
                "MPEG stream ended before the requested byte offset",
            ));
        }
        Ok(())
    }

    fn ensure_all_sizes(&self) -> io::Result<()> {
        loop {
            let next_head = {
                let inner = self.cache.lock();
                if inner.all_sizes_known() {
                    return Ok(());
                }
                if !inner.head_available {
                    None
                } else {
                    inner
                        .sizes
                        .iter()
                        .position(|size| size.is_none())
                        .map(|index| (index, inner.urls[index].clone()))
                }
            };

            let Some((index, url)) = next_head else {
                break;
            };

            match (self.cache.head)(url) {
                Ok(len) => {
                    let mut inner = self.cache.lock();
                    if inner.sizes[index].is_none() {
                        inner.sizes[index] = Some(len);
                    }
                    self.cache.condvar.notify_all();
                }
                Err(error) => {
                    let mut inner = self.cache.lock();
                    inner.disable_head(&format!("HEAD failed for fragment {index}: {error}"));
                    break;
                }
            }
        }

        {
            let mut inner = self.cache.lock();
            if inner.all_sizes_known() {
                return Ok(());
            }
            if let Some(index) = inner
                .fragments
                .iter()
                .position(|fragment| fragment.is_none())
            {
                inner.priority = Some(index);
                self.cache.condvar.notify_all();
            }
        }

        let _inner = self.wait_until(|inner| inner.all_sizes_known() || inner.complete)?;
        Ok(())
    }

    fn wait_until<F>(&self, mut ready: F) -> io::Result<MutexGuard<'_, MpegCacheInner>>
    where
        F: FnMut(&MpegCacheInner) -> bool,
    {
        let mut inner = self.cache.lock();
        loop {
            if self.cache.cancelled.load(Ordering::SeqCst) {
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
                .cache
                .condvar
                .wait(inner)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn prioritize_fragment(&self, index: usize) {
        let mut inner = self.cache.lock();
        if inner
            .fragments
            .get(index)
            .is_some_and(|fragment| fragment.is_none())
        {
            inner.priority = Some(index);
            self.cache.condvar.notify_all();
        }
    }

    fn publish_cursor(&self) {
        let mut inner = self.cache.lock();
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
            let inner = self.cache.lock();
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

impl MpegCache {
    fn lock(&self) -> MutexGuard<'_, MpegCacheInner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

fn spawn_get_worker(cache: Arc<MpegCache>, get: GetFn) {
    std::thread::spawn(move || {
        loop {
            if cache.cancelled.load(Ordering::SeqCst) {
                return;
            }

            let next = {
                let mut inner = cache.lock();
                if inner.error.is_some() {
                    return;
                }
                if inner.all_filled() {
                    inner.complete = true;
                    cache.condvar.notify_all();
                    return;
                }
                match inner.next_get() {
                    Some(work) => work,
                    None => {
                        inner.complete = true;
                        cache.condvar.notify_all();
                        return;
                    }
                }
            };

            match get(next.1, next.2) {
                Ok(chunk) => {
                    let mut inner = cache.lock();
                    inner.store_fragment(next.0, chunk);
                    if inner.all_filled() {
                        inner.complete = true;
                    }
                    cache.condvar.notify_all();
                }
                Err(error) => {
                    let mut inner = cache.lock();
                    inner.error = Some(error.to_string());
                    cache.condvar.notify_all();
                    return;
                }
            }
        }
    });
}

fn spawn_head_worker(cache: Arc<MpegCache>) {
    std::thread::spawn(move || {
        loop {
            if cache.cancelled.load(Ordering::SeqCst) {
                return;
            }

            let Some((index, url)) = ({
                let inner = cache.lock();
                if inner.error.is_some() {
                    return;
                }
                inner.next_head()
            }) else {
                return;
            };

            match (cache.head)(url) {
                Ok(len) => {
                    let mut inner = cache.lock();
                    if inner.sizes[index].is_none() {
                        inner.sizes[index] = Some(len);
                    }
                    cache.condvar.notify_all();
                }
                Err(error) => {
                    let mut inner = cache.lock();
                    inner.disable_head(&format!("HEAD failed for fragment {index}: {error}"));
                    cache.condvar.notify_all();
                    return;
                }
            }
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
                let inner = self.cache.lock();
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
                let inner = self.wait_until(|inner| {
                    inner
                        .fragments
                        .get(index)
                        .is_some_and(|fragment| fragment.is_some())
                        || inner.complete
                })?;

                let Some(fragment) = inner.fragments.get(index).and_then(|f| f.as_ref()) else {
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
        self.cache.cancelled.store(true, Ordering::SeqCst);
        self.cache.condvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Duration;

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
    ) -> impl Fn(String, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |url: String, _expected_len: Option<u64>| {
            payloads
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, bytes)| bytes.to_vec())
                .ok_or_else(|| Error::StreamInitializationError(format!("unknown url {url}")))
        }
    }

    fn instant_head(
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> impl Fn(String) -> Result<u64, Error> + Send + Sync {
        move |url: String| {
            payloads
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, bytes)| bytes.len() as u64)
                .ok_or_else(|| Error::StreamInitializationError(format!("unknown url {url}")))
        }
    }

    fn failing_head() -> impl Fn(String) -> Result<u64, Error> + Send + Sync {
        move |_url: String| Err(Error::StreamInitializationError("HEAD unavailable".into()))
    }

    fn start_instant(
        urls: Vec<String>,
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> Result<MpegStreamReader, Error> {
        MpegStreamReader::new_with_get_head(
            urls,
            None,
            instant_get(payloads),
            instant_head(payloads),
        )
    }

    fn gated_get(
        allow_rest: Arc<AtomicBool>,
        payload: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    ) -> impl Fn(String, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |url: String, _expected_len: Option<u64>| {
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "more".into()],
            None,
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "tail".into()],
            None,
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            {
                let allow_get = allow_get.clone();
                let get_order = get_order.clone();
                move |url: String, _expected_len: Option<u64>| {
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
                move |url: String| {
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into()],
            None,
            {
                let allow_get = allow_get.clone();
                move |url: String, _expected_len: Option<u64>| {
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
            move |url: String| {
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
            !reader.cache.lock().head_available,
            "HEAD should be disabled after GET/HEAD length mismatch"
        );
    }

    #[test]
    fn seek_bytes_current_requires_known_position() {
        let allow_a = Arc::new(AtomicBool::new(false));
        let allow_b = Arc::new(AtomicBool::new(false));
        let allow_c = Arc::new(AtomicBool::new(false));
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            Some(MediaTimeline {
                timescale: 1,
                media_durations: vec![1, 1, 1],
            }),
            {
                let allow_a = allow_a.clone();
                let allow_b = allow_b.clone();
                let allow_c = allow_c.clone();
                move |url: String, _expected_len: Option<u64>| match url.as_str() {
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into()],
            Some(timeline),
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
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            Some(MediaTimeline {
                timescale: 1,
                media_durations: vec![1, 1, 1],
            }),
            {
                let allow_a = allow_a.clone();
                let allow_b = allow_b.clone();
                let allow_c = allow_c.clone();
                let order = order.clone();
                move |url: String, _expected_len: Option<u64>| match url.as_str() {
                    "init" => Ok(b"INIT".to_vec()),
                    "a" => {
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
        let mut reader = MpegStreamReader::new_with_get_head(
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
            {
                let release_media = release_media.clone();
                let order = order.clone();
                move |url: String, _expected_len: Option<u64>| {
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

        let cache = reader.cache.clone();
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
        let _inner = reader.wait_until(|inner| inner.all_filled()).unwrap();

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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into()],
            Some(timeline),
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "aaaa".into(), "bbbb".into()],
            None,
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
            move |url: String, _expected_len: Option<u64>| {
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

        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "bad".into()],
            None,
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
        let result =
            MpegStreamReader::new_with_get_head(Vec::new(), None, instant_get(&[]), failing_head());
        assert!(matches!(result, Err(Error::DashManifestMissingUrls)));
    }

    #[test]
    fn head_worker_fills_sizes_without_bodies() {
        let allow_late = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let get_expected = Arc::new(Mutex::new(Vec::new()));
        let reader = MpegStreamReader::new_with_get_head(
            vec![
                "init".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
            ],
            None,
            {
                let allow_late = allow_late.clone();
                let get_expected = get_expected.clone();
                move |url: String, expected_len: Option<u64>| {
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
                move |url: String| {
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        let inner = reader
            .wait_until(|inner| inner.sizes[4].is_some() && inner.sizes[5].is_some())
            .unwrap();
        assert!(
            inner.fragments[4].is_none() && inner.fragments[5].is_none(),
            "HEAD worker should learn sizes before those bodies are fetched"
        );
        drop(inner);

        let heads = head_calls.lock().unwrap().clone();
        assert!(heads.contains(&"d".to_string()), "{heads:?}");
        assert!(heads.contains(&"e".to_string()), "{heads:?}");

        allow_late.store(true, Ordering::SeqCst);
        let _inner = reader.wait_until(|inner| inner.all_filled()).unwrap();
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
        let reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            None,
            {
                let allow_get = allow_get.clone();
                move |url: String, _expected_len: Option<u64>| {
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
                move |url: String| {
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        let inner = reader.wait_until(|inner| inner.all_sizes_known()).unwrap();
        assert!(
            inner.fragments[1].is_none()
                && inner.fragments[2].is_none()
                && inner.fragments[3].is_none(),
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
        let reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into(), "c".into()],
            None,
            instant_get(&[
                ("init", b"INIT"),
                ("a", b"AAAA"),
                ("b", b"BBBB"),
                ("c", b"CCCC"),
            ]),
            {
                let allow_heads = allow_heads.clone();
                move |_url: String| {
                    while !allow_heads.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Ok(4)
                }
            },
        )
        .unwrap();

        let _inner = reader.wait_until(|inner| inner.all_filled()).unwrap();
        allow_heads.store(true, Ordering::SeqCst);
    }

    #[test]
    fn head_worker_failure_disables_head() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(AtomicUsize::new(0));
        let reader = MpegStreamReader::new_with_get_head(
            vec![
                "init".into(),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
            ],
            None,
            {
                let allow_get = allow_get.clone();
                move |url: String, _expected_len: Option<u64>| {
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
                move |_url: String| {
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
        let _inner = reader.wait_until(|inner| inner.all_filled()).unwrap();
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into(), "b".into()],
            Some(two_second_timeline()),
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
        let mut reader = MpegStreamReader::new_with_get_head(
            vec!["init".into(), "a".into()],
            Some(two_second_timeline()),
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
}
