//! Progressive in-memory streaming of MPEG-DASH fragments: the blocking API of the `sync`
//! feature.

use crate::buffer::BufferHandle;
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use crate::fragment::{Fragment, Fragments, MediaTimeline, slice_range};
use crate::schedule::{
    self, Cursor, Part, Position, Retention, Schedule, StreamTimeline, TimeAccuracy, WindowChange,
};
use crate::store::{FragmentData, Store};
use bytes::Bytes;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Fetches fragment bodies and sizes for a [`FragmentCache`].
///
/// [`ureq::Agent`] implements this with its connection pool.
/// Implement this trait to load fragments from another HTTP client, local
/// storage, or an application-specific source.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use bytes::Bytes;
/// use playdash::{Error, Fragment, FragmentCache, Transport};
///
/// struct MemoryTransport;
///
/// impl Transport for MemoryTransport {
///     fn get(&self, fragment: &Fragment, _: Option<u64>) -> Result<Bytes, Error> {
///         let bytes: &'static [u8] = match fragment.url.as_str() {
///             "memory://init" => b"initialization data",
///             _ => b"media data",
///         };
///         Ok(Bytes::from_static(bytes))
///     }
/// }
///
/// let cache = FragmentCache::new(Arc::new(MemoryTransport));
/// assert!(cache.cached(&Fragment::new("memory://init")).is_none());
/// ```
pub trait Transport: Send + Sync {
    /// Body of `fragment`.
    ///
    /// Must honor [`Fragment::range`] and return exactly those bytes; a ranged
    /// body of any other length fails the stream. `size_hint` is the length
    /// when already known, for preallocating the body.
    fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Bytes, Error>;

    /// Byte length of `fragment`, typically from a HEAD `Content-Length`.
    ///
    /// Never called for ranged fragments, whose sizes are already known. An
    /// error disables HEAD probing for the stream; the default always errors.
    fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
        Err(Error::Initialization(format!(
            "transport cannot HEAD {fragment}"
        )))
    }
}

impl Transport for ureq::Agent {
    /// Sends a `Range` header for ranged fragments; if a server ignores it and
    /// returns the whole body, the range is sliced out of that body.
    fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Bytes, Error> {
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
        let body = Bytes::from(bytes);
        match &fragment.range {
            Some(range) if !partial => {
                #[cfg(feature = "log")]
                log::warn!(
                    "server ignored the Range request for {fragment}; slicing the full body"
                );
                Ok(slice_range(&body, range)
                    .map_or_else(|| body.clone(), |part| body.slice_ref(part)))
            }
            _ => Ok(body),
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
                Error::Initialization(format!("HEAD for {url} did not return Content-Length"))
            })
    }
}

/// Downloaded fragment data that can be shared by multiple readers.
///
/// With the `encryption` feature, `with_keys` makes the cache decrypt
/// Common Encryption before making fragment data available.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use playdash::{DashManifest, FragmentCache};
///
/// # fn example(manifest: &DashManifest) -> Result<(), playdash::Error> {
/// let cache = Arc::new(FragmentCache::default());
/// let first = manifest.stream_with_cache("audio", cache.clone())?;
/// let second = manifest.stream_with_cache("audio", cache)?;
/// // Both streams reuse fragment bodies that either one downloads.
/// # drop((first, second));
/// # Ok(())
/// # }
/// ```
pub struct FragmentCache {
    transport: Arc<dyn Transport>,
    pub(crate) store: Store,
}

impl FragmentCache {
    /// An empty cache that fetches through `transport`.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            store: Store::default(),
        }
    }

    /// Decrypts `cenc` and `cbcs` Common Encryption with `keys` before
    /// caching each body, so readers and [`Self::cached`] see plaintext.
    ///
    /// Each reader parses its representation's protection from the
    /// initialization segment, and fails with
    /// [`Error::MissingContentKey`] before any media GET if `keys` lacks
    /// its KID. Unprotected representations pass through unchanged.
    #[cfg(feature = "encryption")]
    pub fn with_keys(mut self, keys: ContentKeys) -> Self {
        self.store.set_keys(keys);
        self
    }

    /// Whether this cache decrypts Common Encryption; see `with_keys`
    /// (requires the `encryption` feature).
    pub fn decrypts(&self) -> bool {
        self.store.decrypts()
    }

    /// The body of `fragment`, if it is held. On a cache with content keys,
    /// this is the decrypted body.
    pub fn cached(&self, fragment: &Fragment) -> Option<Bytes> {
        self.store.cached(fragment)
    }

    /// The byte length of `fragment`, if its range, a HEAD or a GET has
    /// established it.
    pub fn known_size(&self, fragment: &Fragment) -> Option<u64> {
        self.store.known_size(fragment)
    }

    /// The shared entry for `fragment`, created empty if new.
    pub(crate) fn data(&self, fragment: Fragment) -> Arc<FragmentData> {
        self.store.data(fragment)
    }

    /// The body of `data`, fetching it unless it is held or another stream
    /// is already fetching it. A ranged body of the wrong length is an error
    /// and is not kept.
    pub(crate) fn get(&self, data: &FragmentData) -> Result<Bytes, Error> {
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let _in_flight = data
            .blocking_fetch
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let body = self.transport.get(&data.fragment, data.size())?;
        self.store.accept(data, body)
    }

    /// HEADs `data` unless its size is already known or another stream is
    /// already probing it.
    fn probe(&self, data: &FragmentData) -> Result<(), Error> {
        if data.size().is_some() {
            return Ok(());
        }
        let _in_flight = data
            .blocking_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if data.size().is_none() {
            let size = self.transport.head(&data.fragment)?;
            self.store.accept_probe(data, size);
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
            .field("fragments", &self.store.len())
            .field("decrypts", &self.decrypts())
            .finish_non_exhaustive()
    }
}

/// Maximum idle time for an HTTP connection in the ureq pool.
///
/// Fastly allows idle client reuse for up to 10 minutes. Stay well under
/// that so we close first, and under typical NAT idle timeouts.
const POOL_IDLE_AGE: Duration = Duration::from_secs(90);

/// A ureq agent with this crate's connection-pool settings.
pub(crate) fn ureq_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_idle_age(POOL_IDLE_AGE)
            .build(),
    )
}

/// A representation's fragments, downloaded in the background and read in
/// order: an [`Iterator`] of whole fragments, and a progressive [`Read`] and
/// [`Seek`] view of the same bytes. The blocking counterpart of the `async`
/// feature's `nonblocking::FragmentStream`.
///
/// The initialization segment comes first, then the media segments. Iterating
/// and reading share one position: [`Iterator::next`] yields everything from
/// the current position to the end of its fragment, which is a whole fragment
/// unless [`Read`] or [`Self::seek_bytes`] left the position inside one. Use
/// [`Self::seek_bytes`] for byte offsets or [`Self::seek_time_coarse`] to seek
/// to the start of the fragment containing a timestamp. Both iterating and
/// reading block while the fragment they need downloads.
///
/// It downloads on a background thread, one fragment at a time. By default it
/// keeps the whole representation; [`Self::with_retention`] bounds how much it
/// keeps behind its position and how far ahead it downloads, as long video
/// needs.
///
/// After a download fails, iteration yields the error once and then ends, and
/// [`Read`] keeps returning it.
///
/// **N.B.** [`Read`] and [`Iterator`] both define `take`, `chain` and
/// `by_ref`, and `&mut FragmentStream` implements both traits too. With
/// [`Read`] in scope, a method call such as `stream.take(3)` is ambiguous and
/// will not compile. Name the trait you mean instead:
///
/// ```no_run
/// # fn example(stream: playdash::FragmentStream) {
/// use std::io::Read;
///
/// let first_three = Iterator::take(stream, 3); // fragments
/// # let stream: playdash::FragmentStream = todo!();
/// let first_kib = Read::take(stream, 1024); // bytes
/// # }
/// ```
///
/// # Examples
///
/// ```no_run
/// use std::io::Read;
/// use std::time::Duration;
/// use playdash::DashManifest;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
///
/// // Whole fragments.
/// for fragment in manifest.stream("audio")? {
///     let bytes = fragment?;
///     # drop(bytes);
/// }
///
/// // Bytes, from 30 seconds in.
/// let mut stream = manifest.stream("audio")?;
/// stream.seek_time_coarse(Duration::from_secs(30))?;
/// let mut bytes = [0; 4096];
/// let count = stream.read(&mut bytes)?;
/// # let _ = count;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct FragmentStream {
    shared: Arc<StreamShared>,
    cursor: Cursor,
    timeline: Option<StreamTimeline>,
    /// Estimated stream length. When set, [`SeekFrom::End`]`(0)` may report it
    /// instead of waiting for every fragment size.
    length_estimate: Option<u64>,
    /// Set once iteration has yielded an error, so it ends rather than
    /// yielding the same error forever.
    failed: bool,
}

/// A stream's state, shared with its GET and HEAD worker threads.
#[derive(Debug)]
struct StreamShared {
    schedule: Mutex<Schedule>,
    /// Notified after every change to `schedule`, or to a body or size in
    /// `schedule.fragments` written by this stream's threads.
    condvar: Condvar,
    cancelled: AtomicBool,
    cache: Arc<FragmentCache>,
}

impl FragmentStream {
    /// Creates a stream of `fragments` using the default HTTP transport. The
    /// initialization fragment is fetched before this returns.
    ///
    /// `timeline`, when given, must have one duration per media fragment.
    /// Call [`Self::start_eager_head`] to discover unknown fragment sizes in
    /// the background for faster byte seeking.
    pub fn new(fragments: Fragments, timeline: Option<MediaTimeline>) -> Result<Self, Error> {
        Self::new_with_cache(fragments, timeline, Arc::default())
    }

    /// Like [`Self::new`], but fetches through `transport` with a new
    /// [`FragmentCache`] dedicated to this stream.
    pub fn new_with_transport(
        fragments: Fragments,
        timeline: Option<MediaTimeline>,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, Error> {
        let cache = FragmentCache::new(transport);
        Self::new_with_cache(fragments, timeline, Arc::new(cache))
    }

    /// Like [`Self::new`], but reuses data from `cache`.
    pub fn new_with_cache(
        fragments: Fragments,
        timeline: Option<MediaTimeline>,
        cache: Arc<FragmentCache>,
    ) -> Result<Self, Error> {
        let timeline = timeline.map(|timeline| StreamTimeline::new(timeline, 0));
        Self::open(fragments, timeline, cache)
    }

    /// Creates a stream that reads the initialization fragment of
    /// `fragments`, then its media from the fragment covering `time`, through
    /// `cache`. Returns the stream and the start of that fragment, which is at
    /// or before `time`. The initialization fragment is fetched before this
    /// returns.
    ///
    /// The stream reports and seeks by `timeline`'s media time, as a stream
    /// of every fragment would; seeking to a time before its start fails. A
    /// `time` at or past the end starts at the last media fragment.
    /// `timeline` must have one duration per media fragment.
    ///
    /// Fails with [`Error::Initialization`] if `timeline` has no media
    /// durations or a zero timescale, or there are fewer media fragments than
    /// the timeline puts before `time`.
    pub fn starting_at(
        fragments: Fragments,
        timeline: MediaTimeline,
        time: Duration,
        cache: Arc<FragmentCache>,
    ) -> Result<(Self, Duration), Error> {
        let (fragments, timeline, start) = schedule::start_at(fragments, timeline, time)?;
        Ok((Self::open(fragments, Some(timeline), cache)?, start))
    }

    /// A stream of `fragments`, timed by `timeline`, fetching through `cache`.
    fn open(
        fragments: Fragments,
        timeline: Option<StreamTimeline>,
        cache: Arc<FragmentCache>,
    ) -> Result<Self, Error> {
        // Fetch the initialization fragment, which tells how to decrypt the media.
        let init = cache.data(fragments.init);
        let media: Vec<_> = fragments
            .media
            .into_iter()
            .map(|fragment| cache.data(fragment))
            .collect();
        #[cfg(feature = "encryption")]
        cache.store.assign_init_role(&init);
        cache.get(&init)?;
        #[cfg(feature = "encryption")]
        cache.store.assign_media_roles(&init, &media)?;

        // Keep everything until a retention says otherwise.
        let spans = timeline
            .as_ref()
            .map(StreamTimeline::spans)
            .unwrap_or_default();
        let schedule = Schedule::new(init, media, spans);
        cache.store.acquire(&schedule.kept());
        let shared = Arc::new(StreamShared {
            schedule: Mutex::new(schedule),
            condvar: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cache,
        });
        spawn_get_worker(shared.clone());

        Ok(Self {
            shared,
            cursor: Cursor::start(),
            timeline,
            length_estimate: None,
            failed: false,
        })
    }

    /// Keeps only as much of the representation around the position as
    /// `retention` says, and downloads only that far ahead. The default keeps
    /// everything. See [`Retention`].
    pub fn with_retention(self, retention: Retention) -> Self {
        let change = self.shared.lock().set_retention(retention);
        self.shared.window_moved(change);
        self
    }

    /// A handle reporting how much of this stream's media its cache holds.
    /// `None` if the stream has no timeline.
    pub fn buffer(&self) -> Option<BufferHandle> {
        let spans = self.timeline.as_ref()?.spans();
        let media = self.shared.lock().media.clone();
        Some(BufferHandle::new(&self.shared.cache.store, media, spans))
    }

    /// Starts background HEAD prefetch of unknown fragment sizes.
    ///
    /// This is a no-op when all sizes are known or the server does not support
    /// usable `HEAD` responses.
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
    /// A request already in progress may still complete. Byte seeks can still
    /// request sizes when needed.
    pub fn stop_eager_head(&self) {
        self.shared.lock().eager_head = false;
    }

    /// Allows demuxers such as Symphonia to query an estimated stream length
    /// with [`SeekFrom::End`]`(0)` before every fragment size is known.
    ///
    /// Once all sizes are known, the exact length is returned. Other
    /// [`SeekFrom::End`] offsets always wait for exact sizes.
    ///
    /// Requires a [`MediaTimeline`] and a non-zero bitrate.
    pub fn set_symphonia_compat(&mut self, enabled: bool, bitrate_bps: u32) -> Result<(), Error> {
        if !enabled {
            self.length_estimate = None;
            return Ok(());
        }
        let init_len = self.shared.lock().init.size().unwrap_or(0);
        let estimate = schedule::compat_estimate(self.timeline.as_ref(), init_len, bitrate_bps)?;
        self.length_estimate = Some(estimate);
        Ok(())
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
        // The end, as far as it is known or estimated.
        let reported_end = match from {
            SeekFrom::End(0) => self.shared.lock().reported_end_len(self.length_estimate),
            _ => None,
        };
        if let Some(len) = reported_end {
            return Ok(self.park_at_end(len));
        }

        // Resolve the target to a fragment, learning sizes as needed.
        if let SeekFrom::End(_) = from {
            self.ensure_sizes(Schedule::all_sizes_known)?;
        }
        let total = self.shared.lock().total_len();
        let target = schedule::seek_target(from, self.cursor.byte_position, total)?;
        self.ensure_sizes(|schedule| schedule.resolve_cursor(target).is_some())?;
        let (part, offset_in_fragment) =
            self.shared.lock().resolve_cursor(target).ok_or_else(|| {
                io::Error::other("MPEG stream ended before the requested byte offset")
            })?;

        self.cursor.place(part, offset_in_fragment, Some(target));
        self.publish_cursor();
        self.prioritize_fragment(part);
        Ok(Position {
            byte: Some(target),
            time: schedule::time_at_fragment(
                self.timeline.as_ref(),
                part,
                schedule::accuracy_at(offset_in_fragment),
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
    /// to one media-fragment duration. The returned [`TimePosition`](crate::TimePosition) contains
    /// the actual fragment-start timestamp and marks it [`TimeAccuracy::Exact`]
    /// only when the requested time is exactly that boundary. The byte field is
    /// `None` until every earlier fragment size is known.
    pub fn seek_time_coarse(&mut self, time: Duration) -> io::Result<Position> {
        let (media_index, exact) = schedule::time_target(self.timeline.as_ref(), time)?;
        let target = Part::Media(media_index);
        let change = {
            let mut schedule = self.shared.lock();
            if schedule.past_end(target) {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            }
            let change = schedule.move_cursor(target);
            schedule.request(target);
            change
        };
        self.shared.window_moved(change);

        let byte_position = {
            let inner = self.wait_until(|inner| !inner.missing_body(target) || inner.complete)?;
            if inner.missing_body(target) {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            }
            inner.prefix_len(target)
        };

        self.cursor.place(target, 0, byte_position);
        Ok(Position {
            byte: byte_position,
            time: schedule::time_at_fragment(
                self.timeline.as_ref(),
                target,
                match exact {
                    true => TimeAccuracy::Exact,
                    false => TimeAccuracy::Coarse,
                },
            ),
        })
    }

    /// Fills unknown sizes until `known` holds, via on-demand HEAD or GET
    /// fallback. Also returns once every body is downloaded.
    fn ensure_sizes(&self, known: impl Fn(&Schedule) -> bool) -> io::Result<()> {
        loop {
            let next_head = {
                let inner = self.shared.lock();
                if known(&inner) {
                    return Ok(());
                }
                inner.next_head()
            };
            // A failed HEAD disables probing, so `next_head` ends the loop.
            let Some((part, data)) = next_head else {
                break;
            };
            self.shared.probe(part, &data);
        }

        // Without HEAD, download the first fragment whose size is unknown,
        // even outside the window: its size is kept after its body is dropped.
        {
            let mut schedule = self.shared.lock();
            if known(&schedule) {
                return Ok(());
            }
            if let Some(part) = schedule.first_unknown_size() {
                schedule.request(part);
                self.shared.condvar.notify_all();
            }
        }

        self.wait_until(|inner| known(inner) || inner.complete)
            .map(drop)
    }

    fn wait_until<F>(&self, mut ready: F) -> io::Result<MutexGuard<'_, Schedule>>
    where
        F: FnMut(&Schedule) -> bool,
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

    fn prioritize_fragment(&self, part: Part) {
        let mut schedule = self.shared.lock();
        if schedule.missing_body(part) {
            schedule.request(part);
            self.shared.condvar.notify_all();
        }
    }

    /// Tells the workers where the cursor is now, which moves the window.
    fn publish_cursor(&self) {
        let change = self.shared.lock().move_cursor(self.cursor.part);
        self.shared.window_moved(change);
    }

    /// Reports the end for [`SeekFrom::End`]`(0)` without retargeting the GET
    /// worker.
    fn park_at_end(&mut self, len: u64) -> Position {
        let end = self.shared.lock().end();
        self.cursor.place(end, 0, Some(len));
        Position {
            byte: Some(len),
            time: schedule::time_at_end(self.timeline.as_ref()),
        }
    }
}

impl StreamShared {
    fn lock(&self) -> MutexGuard<'_, Schedule> {
        self.schedule
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Applies a window move to the cache, dropping bodies no stream keeps,
    /// and wakes the GET worker for what entered the window.
    fn window_moved(&self, change: WindowChange) {
        self.cache.store.acquire(&change.acquired);
        self.cache.store.release(&change.released, true);
        self.condvar.notify_all();
    }

    /// HEADs `data`, the fragment at `part`, unless its size is known, or
    /// disables HEAD probing if the request fails.
    fn probe(&self, part: Part, data: &FragmentData) {
        let result = self.cache.probe(data);
        let mut state = self.lock();
        if let Err(error) = result {
            state.disable_head(&format!("HEAD failed for {part}: {error}"));
        }
        self.condvar.notify_all();
    }
}

/// Downloads the stream's fragments one at a time, in the order its schedule
/// says, parking whenever the window holds everything it should, until the
/// stream is dropped or a download fails.
fn spawn_get_worker(shared: Arc<StreamShared>) {
    std::thread::spawn(move || {
        loop {
            // Wait for something to fetch.
            let (part, data) = {
                let mut schedule = shared.lock();
                loop {
                    if shared.cancelled.load(Ordering::SeqCst) || schedule.error.is_some() {
                        return;
                    }
                    if let Some(work) = schedule.next_get() {
                        break work;
                    }
                    schedule.complete = true;
                    shared.condvar.notify_all();
                    schedule = shared
                        .condvar
                        .wait(schedule)
                        .unwrap_or_else(|error| error.into_inner());
                }
            };

            // Fetch it, and drop it again if it was only wanted for its size.
            let result = shared.cache.get(&data);
            let kept = {
                let mut schedule = shared.lock();
                match result {
                    Ok(_) => schedule.check_probe(part),
                    Err(error) => schedule.error = Some(error.to_string()),
                }
                schedule.keeps(part)
            };
            if !kept {
                shared.cache.store.forget_if_unheld(&data);
            }
            shared.condvar.notify_all();
        }
    });
}

fn spawn_head_worker(shared: Arc<StreamShared>) {
    std::thread::spawn(move || {
        loop {
            if shared.cancelled.load(Ordering::SeqCst) {
                return;
            }

            // Exit and spawn decisions share the lock so start/stop cannot
            // leave zero or two workers.
            let Some((part, data)) = ({
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
            shared.probe(part, &data);
        }
    });
}

impl Read for FragmentStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let part = self.cursor.part;
            if self.shared.lock().past_end(part) {
                let _schedule = self.wait_until(|schedule| schedule.complete)?;
                return Ok(0);
            }

            // Wait for the fragment under the cursor.
            self.prioritize_fragment(part);
            let body = self
                .wait_until(|schedule| !schedule.missing_body(part) || schedule.complete)?
                .checked_body(part);
            let Some(body) = body else {
                return Ok(0);
            };

            // Copy from it, moving on to the next fragment once it is used up.
            let copied = self.cursor.read(&body, buf);
            if self.cursor.part != part {
                self.publish_cursor();
            }
            if copied > 0 {
                return Ok(copied);
            }
        }
    }
}

impl Iterator for FragmentStream {
    type Item = Result<Bytes, Error>;

    /// The rest of the current fragment: a whole fragment, unless reading or a
    /// byte seek left the position inside one. Blocks while it downloads.
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            let part = self.cursor.part;
            if self.shared.lock().past_end(part) {
                return None;
            }

            // Wait for the fragment under the cursor.
            self.prioritize_fragment(part);
            let waited = self
                .wait_until(|schedule| !schedule.missing_body(part) || schedule.complete)
                .map(|mut schedule| schedule.checked_body(part));
            let body = match waited {
                Ok(body) => body?,
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error.into()));
                }
            };

            // Hand over what is left of it and move on to the next.
            let rest = self.cursor.take_rest(body);
            self.publish_cursor();
            if let Some(rest) = rest {
                return Some(Ok(rest));
            }
        }
    }
}

impl Seek for FragmentStream {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.seek_bytes(from)?
            .byte
            .ok_or_else(|| io::Error::other("MPEG stream byte position is unknown"))
    }
}

impl Drop for FragmentStream {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.shared.condvar.notify_all();
        // Let go of what this stream kept without dropping it, so a stream
        // rebuilt on the same cache, as after a seek, still finds it.
        let kept = self.shared.lock().kept();
        self.shared.cache.store.release(&kept, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BufferStatus;
    use crate::fragment::parse_byte_range;
    use crate::schedule::TimePosition;
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
        fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Bytes, Error> {
            (self.get)(fragment, size_hint).map(Bytes::from)
        }

        fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
            (self.head)(fragment)
        }
    }

    /// A stream fetching through GET and HEAD closures, probing sizes in the
    /// background from the start when `eager_head` is set.
    fn closure_reader<G, H>(
        fragments: Fragments,
        timeline: Option<MediaTimeline>,
        eager_head: bool,
        get: G,
        head: H,
    ) -> Result<FragmentStream, Error>
    where
        G: Fn(&Fragment, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync + 'static,
        H: Fn(&Fragment) -> Result<u64, Error> + Send + Sync + 'static,
    {
        let transport = Arc::new(FnTransport { get, head });
        let stream = FragmentStream::new_with_transport(fragments, timeline, transport)?;
        if eager_head {
            stream.start_eager_head();
        }
        Ok(stream)
    }

    /// [`FragmentStream::new_with_cache`], probing sizes in the background
    /// from the start when `eager_head` is set.
    fn cached_stream(
        fragments: Fragments,
        eager_head: bool,
        cache: Arc<FragmentCache>,
    ) -> Result<FragmentStream, Error> {
        let stream = FragmentStream::new_with_cache(fragments, None, cache)?;
        if eager_head {
            stream.start_eager_head();
        }
        Ok(stream)
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
                .ok_or_else(|| Error::Initialization(format!("unknown url {url}")))
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
                .ok_or_else(|| Error::Initialization(format!("unknown url {url}")))
        }
    }

    fn failing_head() -> impl Fn(&Fragment) -> Result<u64, Error> + Send + Sync {
        move |_fragment: &Fragment| Err(Error::Initialization("HEAD unavailable".into()))
    }

    fn start_instant(
        fragments: Fragments,
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> Result<FragmentStream, Error> {
        closure_reader(
            fragments,
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
            Fragments::new("init", ["a", "b"]),
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
            Fragments::new("init", ["more"]),
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
            Fragments::new("init", ["body"]),
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
            Fragments::new("init", ["tail"]),
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
            Fragments::new("init", ["a", "b"]),
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
            Fragments::new("init", ["a", "b"]),
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
                        other => Err(Error::Initialization(format!("unknown url {other}"))),
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
                    other => Err(Error::Initialization(format!("unknown url {other}"))),
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
    fn reading_a_body_another_stream_fetched_disables_head_on_length_mismatch() {
        let allow_get = Arc::new(AtomicBool::new(false));
        let a_gets = Arc::new(AtomicUsize::new(0));
        let get = {
            let gated = gated_get(allow_get.clone(), |url| url.to_uppercase().into_bytes());
            let a_gets = a_gets.clone();
            move |fragment: &Fragment, size_hint: Option<u64>| {
                if fragment.url == "a" {
                    a_gets.fetch_add(1, Ordering::SeqCst);
                }
                gated(fragment, size_hint)
            }
        };
        // Lie about "a" so its GET will disagree with its HEAD.
        let head = |fragment: &Fragment| match fragment.url.as_str() {
            "a" => Ok(99),
            url => Ok(url.len() as u64),
        };
        let cache = Arc::new(FragmentCache::new(Arc::new(FnTransport { get, head })));
        let fragments = || Fragments::new("init", ["a"]);

        // The first stream probes "a", then fetches it.
        let mut first = cached_stream(fragments(), true, cache.clone()).unwrap();
        wait_for(|| cache.known_size(&"a".into()) == Some(99));
        allow_get.store(true, Ordering::SeqCst);
        let mut bytes = Vec::new();
        first.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"INITA");

        // The second stream finds "a" already held, so its worker never
        // fetches it and only its reader sees the body.
        let mut second = cached_stream(fragments(), false, cache.clone()).unwrap();
        bytes.clear();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"INITA");
        assert_eq!(a_gets.load(Ordering::SeqCst), 1);
        assert!(
            !second.shared.lock().head_available,
            "HEAD should be disabled once the reader sees a GET/HEAD length mismatch"
        );
    }

    #[test]
    fn seek_bytes_current_requires_known_position() {
        let allow_a = Arc::new(AtomicBool::new(false));
        let allow_b = Arc::new(AtomicBool::new(false));
        let allow_c = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            Fragments::new("init", ["a", "b", "c"]),
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
                    other => Err(Error::Initialization(format!("unknown url {other}"))),
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
            Fragments::new("init", ["a", "b"]),
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
            Fragments::new("init", ["a", "b", "c"]),
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
                    other => Err(Error::Initialization(format!("unknown url {other}"))),
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
            Fragments::new("init", ["a", "b", "c", "d"]),
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
                if cache.lock().cursor >= Part::Media(2) {
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
            Fragments::new("init", ["a", "b"]),
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
            Fragments::new("init", ["aaaa", "bbbb"]),
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
        let mut reader = start_instant(
            Fragments::new("init", Vec::<Fragment>::new()),
            &[("init", b"xyz")],
        )
        .unwrap();
        let error = reader.seek_time_coarse(Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn eof_after_complete_download() {
        let mut reader = start_instant(
            Fragments::new("init", Vec::<Fragment>::new()),
            &[("init", b"xyz")],
        )
        .unwrap();

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
                    Err(Error::Initialization(format!("failed to fetch {url}")))
                }
            }
        };

        let mut reader = closure_reader(
            Fragments::new("init", ["bad"]),
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

    const PAYLOADS: &[(&str, &[u8])] = &[("init", b"INIT"), ("a", b"aaaa"), ("b", b"bb")];

    fn abc() -> Fragments {
        Fragments::new("init", ["a", "b"])
    }

    fn texts(items: impl Iterator<Item = Result<Bytes, Error>>) -> Vec<String> {
        items
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn iterates_whole_fragments() {
        let stream = start_instant(abc(), PAYLOADS).unwrap();
        assert_eq!(texts(stream), ["INIT", "aaaa", "bb"]);
    }

    #[test]
    fn iterating_and_reading_share_one_position() {
        let mut stream = start_instant(abc(), PAYLOADS).unwrap();
        let mut head = [0; 6];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"INITaa");

        // The rest of the fragment the read stopped in, then reading resumes after it.
        assert_eq!(stream.next().unwrap().unwrap(), &b"aa"[..]);
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"bb");

        // A byte seek moves iteration too.
        assert_eq!(stream.seek_bytes(SeekFrom::Start(0)).unwrap().byte, Some(0));
        assert_eq!(texts(stream), ["INIT", "aaaa", "bb"]);
    }

    #[test]
    fn iterates_from_a_coarse_seek() {
        let timeline = MediaTimeline {
            timescale: 1,
            media_durations: vec![1, 1],
        };
        let mut stream = closure_reader(
            abc(),
            Some(timeline),
            false,
            instant_get(PAYLOADS),
            failing_head(),
        )
        .unwrap();
        stream
            .seek_time_coarse(Duration::from_millis(1_500))
            .unwrap();
        assert_eq!(texts(stream), ["bb"]);
    }

    #[test]
    fn iteration_ends_after_a_download_failure() {
        let release = Arc::new(AtomicBool::new(false));
        let get = {
            let release = release.clone();
            move |fragment: &Fragment, _expected_len: Option<u64>| match fragment.url.as_str() {
                "init" => Ok(b"INIT".to_vec()),
                url => {
                    while !release.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(Error::Initialization(format!("failed to fetch {url}")))
                }
            }
        };
        let mut stream = closure_reader(
            Fragments::new("init", ["bad", "never"]),
            None,
            false,
            get,
            failing_head(),
        )
        .unwrap();

        assert_eq!(stream.next().unwrap().unwrap(), &b"INIT"[..]);
        release.store(true, Ordering::SeqCst);
        let error = stream.next().unwrap().unwrap_err();
        assert!(error.to_string().contains("failed to fetch bad"));
        assert!(stream.next().is_none());
    }

    #[test]
    fn head_worker_fills_sizes_without_bodies() {
        let allow_late = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let get_expected = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            Fragments::new("init", ["a", "b", "c", "d", "e"]),
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
            .wait_until(|inner| inner.media[3].size().is_some() && inner.media[4].size().is_some())
            .unwrap();
        assert!(
            inner.missing_body(Part::Media(3)) && inner.missing_body(Part::Media(4)),
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
            Fragments::new("init", ["a", "b", "c"]),
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
            (0..3).all(|index| inner.missing_body(Part::Media(index))),
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
            Fragments::new("init", ["a", "b", "c"]),
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
            Fragments::new("init", ["a", "b", "c", "d"]),
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
                    Err(Error::Initialization("HEAD unavailable".into()))
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
            Fragments::new("init", ["a", "b"]),
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
            Fragments::new("init", ["a", "b", "c"]),
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
        assert!(inner.media.iter().all(|data| data.body().is_none()));
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
            Fragments::new("init", ["a", "b", "c", "d"]),
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
            inner.media[0].size(),
            Some(4),
            "in-flight HEAD should be kept"
        );
        assert!(inner.media[1..].iter().all(|data| data.size().is_none()));
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
            Fragments::new("init", ["a", "b", "c"]),
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
            Fragments::new("init", ["a"]),
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
            Fragments::new("init", ["a", "b"]),
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
    fn symphonia_compat_end_zero_does_not_wait_for_bodies() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            Fragments::new("init", ["a", "b"]),
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
            Fragments::new("init", ["a"]),
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
            Fragments::new("init", ["a"]),
            &[("init", b"INIT"), ("a", b"AAAA")],
        )
        .unwrap();
        let error = reader.set_symphonia_compat(true, 128_000).unwrap_err();
        assert!(
            matches!(error, Error::Initialization(message) if message.contains("SegmentTimeline"))
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
            Fragments::new(
                ranged("file", 0, 3),
                [
                    ranged("file", 4, 7),
                    ranged("file", 8, 13),
                    ranged("file", 14, 15),
                ],
            ),
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
        assert!(reader.shared.lock().missing_body(Part::Media(1)));

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
            Fragments::new(ranged("file", 0, 7), Vec::<Fragment>::new()),
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
            Fragments::new(ranged("file", 0, 3), [ranged("file", 4, 7)]),
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
        let mut reader = FragmentStream::new(
            Fragments::new(
                ranged(&format!("{base}/ranged"), 0, 3),
                [
                    ranged(&format!("{base}/plain"), 8, 13),
                    ranged(&format!("{base}/ranged"), 14, 15),
                ],
            ),
            None,
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

        fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<Bytes, Error> {
            self.gets.lock().unwrap().push(fragment.url.clone());
            if self.gated.contains(&fragment.url.as_str()) {
                while !self.released.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(Bytes::copy_from_slice(fragment.url.as_bytes()))
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
        let mut first =
            FragmentStream::new_with_cache(Fragments::new("init", ["a", "b"]), None, cache.clone())
                .unwrap();
        let mut bytes = Vec::new();
        first.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initab");

        // A rebuilt reader, as after a seek, gets everything from memory.
        let mut second = cached_stream(Fragments::new("init", ["b"]), true, cache.clone()).unwrap();
        bytes.clear();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
        assert_eq!(cache.known_size(&"a".into()), Some(1));
        assert!(transport.heads().is_empty());
    }

    #[test]
    fn buffer_reports_cache_changes() {
        let (cache, transport) = counting_cache(&["bb"]);
        let reader = FragmentStream::new_with_cache(
            Fragments::new("init", ["aa", "bb"]),
            Some(MediaTimeline {
                timescale: 10,
                media_durations: vec![10, 10],
            }),
            cache,
        )
        .unwrap();
        let buffer = reader.buffer().unwrap();
        let reported = Arc::new(Mutex::new(Vec::new()));
        buffer.on_change({
            let reported = reported.clone();
            move |status: &BufferStatus| reported.lock().unwrap().push(status.clone())
        });

        // "bb" is held back, so only the first one-second fragment is here.
        wait_for(|| buffer.status().downloaded.fragments == 1);
        let status = buffer.status();
        assert_eq!(status.total.fragments, 2);
        assert_eq!(status.total.time, Duration::from_secs(2));
        assert_eq!(status.ranges, [Duration::ZERO..Duration::from_secs(1)]);

        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| {
            reported
                .lock()
                .unwrap()
                .iter()
                .any(BufferStatus::is_complete)
        });
        let status = buffer.status();
        assert_eq!(status.ranges, [Duration::ZERO..Duration::from_secs(2)]);
        assert_eq!(status.downloaded.bytes, 4);
    }

    /// Fragments of 1.5, 2 and 1 seconds, whose bodies are their names.
    fn uneven() -> (Fragments, MediaTimeline) {
        let timeline = MediaTimeline {
            timescale: 10,
            media_durations: vec![15, 20, 10],
        };
        (Fragments::new("init", ["a", "b", "c"]), timeline)
    }

    #[test]
    fn starting_at_reads_init_then_media_from_the_covering_fragment() {
        let (cache, transport) = counting_cache(&[]);
        let (fragments, timeline) = uneven();
        let (mut stream, start) =
            FragmentStream::starting_at(fragments, timeline, Duration::from_millis(2_750), cache)
                .unwrap();
        assert_eq!(start, Duration::from_millis(1_500));
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initbc");
        assert_eq!(transport.gets(), ["init", "b", "c"]);

        // Times stay the representation's: buffered ranges, seeks and positions.
        let status = stream.buffer().unwrap().status();
        assert_eq!(
            status.ranges,
            [Duration::from_millis(1_500)..Duration::from_millis(4_500)]
        );
        assert_eq!(status.total.fragments, 2);
        assert_eq!(
            stream
                .seek_time_coarse(Duration::from_millis(3_500))
                .unwrap(),
            position(
                Some(5),
                Some((Duration::from_millis(3_500), TimeAccuracy::Exact))
            )
        );
        assert_eq!(stream.next().unwrap().unwrap(), &b"c"[..]);
        let before_start = stream.seek_time_coarse(Duration::from_secs(1)).unwrap_err();
        assert_eq!(before_start.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn starting_at_past_the_end_starts_at_the_last_fragment() {
        let (cache, _transport) = counting_cache(&[]);
        let (fragments, timeline) = uneven();
        let (stream, start) =
            FragmentStream::starting_at(fragments, timeline, Duration::from_secs(60), cache)
                .unwrap();
        assert_eq!(start, Duration::from_millis(3_500));
        assert_eq!(texts(stream), ["init", "c"]);
    }

    #[test]
    fn starting_at_needs_media_where_the_timeline_puts_it() {
        let (cache, _transport) = counting_cache(&[]);
        let (_, timeline) = uneven();
        let empty = MediaTimeline {
            media_durations: Vec::new(),
            ..timeline.clone()
        };
        let no_timeline = FragmentStream::starting_at(
            Fragments::new("init", ["a"]),
            empty,
            Duration::ZERO,
            cache.clone(),
        );
        assert!(matches!(no_timeline, Err(Error::Initialization(_))));

        // The timeline puts 2 seconds in the second fragment, which is missing.
        let too_few = FragmentStream::starting_at(
            Fragments::new("init", ["a"]),
            timeline,
            Duration::from_secs(2),
            cache,
        );
        assert!(matches!(too_few, Err(Error::Initialization(_))));
    }

    #[test]
    fn buffer_needs_a_timeline() {
        let (cache, _transport) = counting_cache(&[]);
        let reader =
            FragmentStream::new_with_cache(Fragments::new("init", ["a"]), None, cache).unwrap();
        assert!(reader.buffer().is_none());
    }

    #[test]
    fn fragment_cache_keeps_in_flight_bodies_after_reader_drop() {
        let (cache, transport) = counting_cache(&["b"]);
        let first =
            FragmentStream::new_with_cache(Fragments::new("init", ["a", "b"]), None, cache.clone())
                .unwrap();
        wait_for(|| transport.gets().contains(&"b".to_owned()));
        drop(first);

        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| cache.cached(&"b".into()).is_some());

        let mut second =
            FragmentStream::new_with_cache(Fragments::new("init", ["b"]), None, cache.clone())
                .unwrap();
        let mut bytes = Vec::new();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
    }

    #[test]
    fn fragment_cache_joins_a_get_already_in_flight() {
        let (cache, transport) = counting_cache(&["b"]);
        let first =
            FragmentStream::new_with_cache(Fragments::new("init", ["a", "b"]), None, cache.clone())
                .unwrap();
        wait_for(|| transport.gets().contains(&"b".to_owned()));
        drop(first);

        // The rebuilt reader waits on the dropped reader's GET of "b".
        let mut second =
            FragmentStream::new_with_cache(Fragments::new("init", ["b"]), None, cache.clone())
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
        let fragments =
            |media: &str| Fragments::new(format!("{base}/init"), [format!("{base}/{media}")]);
        let read_all = |mut reader: FragmentStream| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        };

        let cache = Arc::new(FragmentCache::default());
        let first = FragmentStream::new_with_cache(fragments("a"), None, cache.clone()).unwrap();
        assert_eq!(read_all(first), b"inita");
        let second = FragmentStream::new_with_cache(fragments("b"), None, cache.clone()).unwrap();
        assert_eq!(read_all(second), b"initb");
        assert_eq!(connections.load(Ordering::SeqCst), 1);

        // Separate agents cannot share the connection.
        let third = FragmentStream::new(fragments("c"), None).unwrap();
        assert_eq!(read_all(third), b"initc");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fragment_cache_remembers_head_lengths_across_readers() {
        let (cache, transport) = counting_cache(&["a", "bb"]);
        let fragments = || Fragments::new("init", ["a", "bb"]);
        let first = cached_stream(fragments(), true, cache.clone()).unwrap();
        wait_for(|| cache.known_size(&"bb".into()).is_some());
        drop(first);

        // The rebuilt reader starts with every size and sends no HEAD at all.
        let mut second = cached_stream(fragments(), true, cache.clone()).unwrap();
        assert!(second.shared.lock().all_sizes_known());
        assert_eq!(second.seek_bytes(SeekFrom::End(0)).unwrap().byte, Some(7));
        assert_eq!(transport.heads(), ["a", "bb"]);
        assert_eq!(cache.cached(&"bb".into()), None);

        // A later GET's body takes precedence over the HEAD length.
        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| cache.cached(&"bb".into()).is_some());
        assert_eq!(cache.known_size(&"bb".into()), Some(2));
    }

    /// The init and five one-second media fragments named "1" to "5", whose
    /// bodies are their names.
    fn one_second_stream(cache: Arc<FragmentCache>) -> FragmentStream {
        let fragments = Fragments::new("init", ["1", "2", "3", "4", "5"]);
        let timeline = MediaTimeline {
            timescale: 1,
            media_durations: vec![1; 5],
        };
        FragmentStream::new_with_cache(fragments, Some(timeline), cache).unwrap()
    }

    fn seconds(behind: Option<u64>, ahead: Option<u64>) -> Retention {
        Retention {
            behind: behind.map(Duration::from_secs),
            ahead: ahead.map(Duration::from_secs),
        }
    }

    #[test]
    fn retention_downloads_only_its_window_ahead() {
        let (cache, transport) = counting_cache(&["1", "2", "3", "4", "5"]);
        let stream = one_second_stream(cache).with_retention(seconds(None, Some(2)));
        transport.released.store(true, Ordering::SeqCst);

        // Two seconds ahead of the start is fragments 1 and 2, then it waits.
        wait_for(|| stream.shared.lock().complete);
        assert_eq!(transport.gets(), ["init", "1", "2"]);
    }

    #[test]
    fn retention_forgets_behind_and_downloads_again_on_a_seek_back() {
        let (cache, transport) = counting_cache(&[]);
        let mut stream = one_second_stream(cache.clone()).with_retention(seconds(Some(1), Some(2)));
        assert_eq!(texts(Iterator::take(&mut stream, 3)), ["init", "1", "2"]);

        // At fragment 3, one second behind keeps 2 and lets 1 go.
        wait_for(|| cache.cached(&"3".into()).is_some());
        assert_eq!(cache.cached(&"1".into()), None);
        assert!(cache.cached(&"2".into()).is_some());
        assert!(cache.cached(&"init".into()).is_some());
        assert_eq!(cache.known_size(&"1".into()), Some(1));

        stream.seek_time_coarse(Duration::ZERO).unwrap();
        assert_eq!(stream.next().unwrap().unwrap(), &b"1"[..]);
        let fetches_of_1 = transport.gets().iter().filter(|url| *url == "1").count();
        assert_eq!(fetches_of_1, 2);
    }

    #[test]
    fn retention_keeps_what_another_stream_of_the_cache_keeps() {
        let (cache, _transport) = counting_cache(&[]);
        let everything = one_second_stream(cache.clone());
        wait_for(|| everything.shared.lock().complete);

        let windowed = one_second_stream(cache.clone()).with_retention(seconds(Some(0), Some(1)));
        assert_eq!(texts(windowed), ["init", "1", "2", "3", "4", "5"]);
        for name in ["1", "2", "3", "4", "5"] {
            assert!(cache.cached(&name.into()).is_some(), "{name} was dropped");
        }
        drop(everything);
    }
}
