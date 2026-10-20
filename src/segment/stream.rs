//! Blocking, lower-level access to downloaded DASH segments and bytes.

use super::buffer::BufferHandle;
use super::cache::SegmentCache;
use super::clock::{Clock, SystemClock};
use super::index::MediaIndex;
#[cfg(any(doc, test))]
use super::schedule::TimeAccuracy;
use super::schedule::{
    self, Cursor, Part, PartBody, Position, ReadStep, Retention, Schedule, SizeStep, Supply,
    WindowChange,
};
use super::store::SegmentData;
use super::transport::Transport;
use super::{SegmentLocation, SegmentNumber, Segments, refuse_live};
use crate::error::Error;
use crate::manifest::live::{LiveLink, LiveSource, Plan};
use crate::manifest::{BehindWindow, ContentKind};
use crate::mp4::track::{self, Track};
use bytes::Bytes;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Downloads a representation's segments and exposes them as whole segments or bytes.
///
/// This is a lower-level API. Use [`MediaStream`](crate::MediaStream) when you want
/// packets ready for a codec decoder.
///
/// The stream starts with the MP4 initialization segment, followed by the media
/// segments. Iterate to receive segment bodies, or use [`Read`] and [`Seek`] to treat
/// their concatenated bytes as one file. These views share a position: after a partial
/// byte read, the next iterator item contains only the rest of that segment.
///
/// Downloads run on a background thread. Reading blocks until the required segment is
/// available. By default all segments are retained; [`Self::with_retention`] can bound
/// memory and forward downloading.
///
/// **N.B.** [`Read`] and [`Iterator`] both define `take`, `chain` and
/// `by_ref`, and `&mut SegmentStream` implements both traits too. With
/// [`Read`] in scope, a method call such as `stream.take(3)` is ambiguous and
/// will not compile. Name the trait you mean instead:
///
/// ```no_run
/// # fn example(stream: playdash::segment::SegmentStream) {
/// use std::io::Read;
///
/// let first_three = Iterator::take(stream, 3); // segments
/// # let stream: playdash::segment::SegmentStream = todo!();
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
/// // Whole segments.
/// for segment in manifest.segment_stream("audio")? {
///     let bytes = segment?;
///     # drop(bytes);
/// }
///
/// // Bytes, from 30 seconds in.
/// let mut stream = manifest.segment_stream("audio")?;
/// stream.seek_time(Duration::from_secs(30))?;
/// let mut bytes = [0; 4096];
/// let count = stream.read(&mut bytes)?;
/// # let _ = count;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SegmentStream {
    shared: Arc<StreamShared>,
    cursor: Cursor,
    /// Estimated stream length. When set, [`SeekFrom::End`]`(0)` may report it
    /// instead of waiting for every segment size.
    length_estimate: Option<u64>,
    /// Whether iteration has reported a terminal error. A successful seek
    /// clears this flag.
    failed: bool,
    /// Whether the next returned segment follows an automatic live-window jump.
    jumped: bool,
}

/// A stream's state, shared with its GET and HEAD worker threads.
#[derive(Debug)]
struct StreamShared {
    schedule: Mutex<Schedule>,
    /// Notified after every change to `schedule`, or to a body or size in
    /// the segment data `schedule` holds, written by this stream's threads.
    condvar: Condvar,
    cancelled: AtomicBool,
    cache: Arc<SegmentCache>,
    /// The time the schedule adds segments at.
    clock: Arc<dyn Clock>,
    /// The live presentation whose refreshes keep the schedule's index
    /// current; `None` when static.
    live: Option<LiveLink>,
}

impl SegmentStream {
    /// Creates a stream of `segments` using the default HTTP transport. The
    /// initialization segment is fetched before this returns.
    ///
    /// Call [`Self::start_eager_head`] to discover unknown segment sizes in
    /// the background for faster byte seeking.
    pub fn new(segments: Segments) -> Result<Self, Error> {
        Self::new_with_cache(segments, Arc::default())
    }

    /// Like [`Self::new`], but fetches through `transport` with a new
    /// [`SegmentCache`] dedicated to this stream.
    pub fn new_with_transport(
        segments: Segments,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, Error> {
        let cache = SegmentCache::new(transport);
        Self::new_with_cache(segments, Arc::new(cache))
    }

    /// Like [`Self::new`], but reuses data from `cache`.
    pub fn new_with_cache(segments: Segments, cache: Arc<SegmentCache>) -> Result<Self, Error> {
        Self::new_retained(segments, cache, Retention::default())
    }

    /// Like [`Self::new_with_cache`], but keeps as much as `retention` says
    /// from the start, as [`Self::with_retention`] would after.
    pub(crate) fn new_retained(
        segments: Segments,
        cache: Arc<SegmentCache>,
        retention: Retention,
    ) -> Result<Self, Error> {
        refuse_live(&segments)?;
        let first = segments.index().first();
        Self::open(&segments, first, cache, retention, None)
    }

    /// Opens `source` and follows its live manifest refreshes, using `cache`
    /// and `retention` as [`Self::new_retained`] does.
    pub(crate) fn new_live(
        source: LiveSource,
        cache: Arc<SegmentCache>,
        retention: Retention,
    ) -> Result<Self, Error> {
        let live = Some((source.link, source.delay));
        Self::open(&source.segments, source.first, cache, retention, live)
    }

    /// Creates a stream that reads the initialization segment of
    /// `segments`, then its media from the segment covering `time`, through
    /// `cache`. Returns the stream and the start of that segment, which is at
    /// or before `time`. The initialization segment is fetched before this
    /// returns.
    ///
    /// Times remain relative to the full presentation. Byte offset zero is
    /// the initialization segment followed by the selected media segment, so
    /// earlier media has no byte offset in this stream. Times before the
    /// presentation start select the first segment; times at or beyond its end
    /// select the last.
    ///
    /// Fails with [`Error::Initialization`] if `segments` are untimed or have
    /// no media segments, and with [`Error::DashManifestUnsupportedLive`] for
    /// live segments, which stream from
    /// [`DashManifest::segment_stream_at`](crate::DashManifest::segment_stream_at).
    pub fn starting_at(
        segments: Segments,
        time: Duration,
        cache: Arc<SegmentCache>,
    ) -> Result<(Self, Duration), Error> {
        refuse_live(&segments)?;
        let (first, start) = schedule::start_at(segments.index(), time)?;
        let stream = Self::open(&segments, first, cache, Retention::default(), None)?;
        Ok((stream, start))
    }

    /// Opens `segments` at `first`, backed by `cache` and bounded by
    /// `retention`. `live` supplies refresh state and the target live delay.
    fn open(
        segments: &Segments,
        first: SegmentNumber,
        cache: Arc<SegmentCache>,
        retention: Retention,
        live: Option<(LiveLink, Duration)>,
    ) -> Result<Self, Error> {
        let (live, delay) = live.unzip();
        // Fetch the initialization segment, which tells how to decrypt the media.
        let init = cache.data(segments.init().clone());
        #[cfg(feature = "encryption")]
        cache.store.assign_init_role(&init);
        cache.get(&init)?;
        let role = cache.store.media_role(&init)?;

        // Schedule the media segments the retention keeps, on the presentation's clock.
        let clock: Arc<dyn Clock> = match &live {
            Some(live) => live.live.clock().clone(),
            None => Arc::new(SystemClock),
        };
        let supply = Supply {
            store: &cache.store,
            now: clock.now(),
        };
        let index = segments.index().clone();
        let delay = delay.unwrap_or_default();
        let schedule = Schedule::new(init, index, first, retention, role, delay, supply);
        cache.store.acquire(&schedule.kept());
        let shared = Arc::new(StreamShared {
            schedule: Mutex::new(schedule),
            condvar: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cache,
            clock,
            live,
        });
        spawn_get_worker(shared.clone());

        Ok(Self {
            shared,
            cursor: Cursor::start(first),
            length_estimate: None,
            failed: false,
            jumped: false,
        })
    }

    /// Keeps only as much of the representation around the position as
    /// `retention` says, and downloads only that far ahead. The default keeps
    /// everything. See [`Retention`].
    pub fn with_retention(self, retention: Retention) -> Self {
        let change = self
            .shared
            .lock()
            .set_retention(retention, self.shared.supply());
        self.shared.window_moved(change);
        self
    }

    /// Sets how reading recovers after falling behind a live presentation.
    ///
    /// The default, [`BehindWindow::Fail`], returns
    /// [`Error::BehindLiveWindow`]. This setting has no effect on static
    /// presentations.
    pub fn with_behind_window(self, policy: BehindWindow) -> Self {
        self.shared.lock().set_behind_window(policy);
        self
    }

    /// A handle reporting how much of this stream's media its cache holds.
    /// `None` if the stream's segments are untimed.
    pub fn buffer(&self) -> Option<BufferHandle> {
        let handle = BufferHandle::empty(&self.shared.cache.store);
        let mut schedule = self.shared.lock();
        schedule.index().timing()?;
        schedule.subscribe_buffer(handle.feed(), self.shared.supply());
        Some(handle)
    }

    /// Returns `true` while the stream follows a live presentation.
    ///
    /// At the live edge, reads wait for the next segment instead of ending.
    ///
    /// A live presentation whose manifest turns static stops being live.
    pub fn is_live(&self) -> bool {
        self.shared.lock().index().is_live()
    }

    /// Returns the currently available range in presentation time.
    ///
    /// Live streams report their current time-shift window; static streams
    /// report their full timed range.
    ///
    /// Returns `None` for untimed segments, and while no live segment is
    /// available.
    pub fn available(&self) -> Option<Range<Duration>> {
        self.shared.lock().available_time(self.shared.clock.now())
    }

    /// Returns the end time of the newest available live segment.
    /// Returns `None` for static streams or before live media is available.
    pub fn live_edge(&self) -> Option<Duration> {
        self.shared.lock().live_edge(self.shared.clock.now())
    }

    /// Starts background HEAD prefetch of unknown segment sizes.
    ///
    /// This is a no-op when all sizes are known or the server does not support
    /// usable `HEAD` responses.
    pub fn start_eager_head(&self) {
        if self.shared.cancelled.load(Ordering::SeqCst) {
            return;
        }
        if self.shared.lock().start_eager_head(self.shared.supply()) {
            spawn_head_worker(self.shared.clone());
        }
    }

    /// Stops background HEAD prefetch.
    ///
    /// A request already in progress may still complete. Byte seeks can still
    /// request sizes when needed.
    pub fn stop_eager_head(&self) {
        self.shared.lock().eager_head = false;
    }

    /// Allows demuxers such as Symphonia to query an estimated stream length
    /// with [`SeekFrom::End`]`(0)` before every segment size is known.
    ///
    /// Once all sizes are known, the exact length is returned. Other
    /// [`SeekFrom::End`] offsets always wait for exact sizes.
    ///
    /// Fails with [`Error::Initialization`] if the stream is live, has no
    /// timeline or media, or `bitrate_bps` is zero.
    pub fn set_symphonia_compat(&mut self, enabled: bool, bitrate_bps: u32) -> Result<(), Error> {
        if !enabled {
            self.length_estimate = None;
            return Ok(());
        }
        let estimate = self.shared.lock().compat_estimate(bitrate_bps)?;
        self.length_estimate = Some(estimate);
        Ok(())
    }

    /// Seeks to an exact offset in the concatenated initialization + media bytes.
    ///
    /// When HEAD is available, discovers lengths for skipped segments without
    /// downloading them, then prioritizes a GET of the target segment.
    /// Otherwise backfills bodies until the offset is reachable.
    /// [`SeekFrom::Current`] requires a known absolute byte cursor.
    ///
    /// Fails if earlier segment sizes cannot be learned. For live streams,
    /// offsets belonging to expired segments cannot be resolved, and
    /// [`SeekFrom::End`] returns [`io::ErrorKind::Unsupported`] because the
    /// stream has no fixed end.
    ///
    /// The returned byte offset is exact, except [`SeekFrom::End`]`(0)` under
    /// [`Self::set_symphonia_compat`], which may report an oversized estimate.
    /// For timed segments, the returned time is [`TimeAccuracy::Packet`] at a
    /// segment boundary, and otherwise the containing segment's start as
    /// [`TimeAccuracy::Segment`].
    pub fn seek_bytes(&mut self, from: SeekFrom) -> io::Result<Position> {
        self.shared.lock().check_seek_from(from)?;

        // The end, as far as it is known or estimated, without retargeting
        // the GET worker.
        let reported_end = self.shared.lock().reported_end(self.length_estimate);
        if let SeekFrom::End(0) = from
            && let Some((end, position)) = reported_end
        {
            self.landed(end, 0, position.byte);
            return Ok(position);
        }

        // Resolve the target to a segment, learning sizes as needed.
        if let SeekFrom::End(_) = from {
            self.ensure_sizes(Schedule::all_sizes_known)?;
        }
        let total = self.shared.lock().total_len();
        let target = schedule::seek_target(from, self.cursor.byte_position, total)?;
        self.ensure_sizes(|schedule| schedule.resolve_cursor(target).is_some())?;
        let (part, offset_in_segment, position, change) = self
            .shared
            .lock()
            .land_byte_seek(target, self.shared.supply())?;
        self.shared.window_moved(change);
        self.landed(part, offset_in_segment, Some(target));
        Ok(position)
    }

    /// Updates all reader state after a seek lands and clears prior errors or
    /// jump markers.
    fn landed(&mut self, part: Part, offset_in_segment: usize, byte_position: Option<u64>) {
        self.cursor.place(part, offset_in_segment, byte_position);
        self.failed = false;
        self.jumped = false;
    }

    /// Seeks to the start of the media segment containing presentation time
    /// `time`.
    ///
    /// Static times before the first segment land on that segment. For live
    /// media, times before the current window land at its start; times at or
    /// beyond the live edge land at the configured delay behind it. The
    /// selected segment is downloaded before this method returns unless it
    /// expires and the behind-window policy selects a replacement.
    ///
    /// Seeking is segment-granular, so playback may start earlier than the
    /// requested time. The returned time is the exact start of the selected
    /// segment. Its byte offset is `None` if earlier sizes are unknown or the
    /// selected segment precedes this stream's initial join point.
    ///
    /// Returns an error if a live stream has no segment available. A target
    /// can also expire from a live stream's moving window while it downloads.
    /// In that case, [`BehindWindow::Fail`] returns
    /// [`Error::BehindLiveWindow`]; the other [`BehindWindow`] policies move
    /// to their chosen current segment, which the next read downloads if
    /// necessary.
    pub fn seek_time(&mut self, time: Duration) -> io::Result<Position> {
        let (target, change) = self
            .shared
            .lock()
            .begin_time_seek(time, self.shared.supply())?;
        self.shared.window_moved(change);
        let step = self.wait_for(|schedule| schedule.read_step(target, self.shared.supply()))?;
        let (part, position, change) = self.shared.lock().land_time_seek(step)?;
        self.shared.window_moved(change);
        self.landed(part, 0, position.byte);
        Ok(position)
    }

    /// Fetches the body of media segment `index` of the representation, blocking
    /// until it downloads, without moving the stream's position or retention window.
    ///
    /// `index` counts the representation's media segments, as
    /// [`Segments::segment_at`] does, also for a stream from
    /// [`Self::starting_at`]. A segment outside the retention window is not
    /// kept once returned, unless another stream sharing the cache keeps it.
    ///
    /// Fails with [`Error::Initialization`] if the segment is past the end or
    /// has expired from a live window. Download failures are returned without
    /// ending normal stream iteration.
    pub fn fetch_segment(&self, index: usize) -> Result<Bytes, Error> {
        Ok(self.media_part_body(SegmentNumber(index as u64))?.body)
    }

    /// The tracks of the representation's initialization segment, as a
    /// [`PacketStream`](crate::packet::PacketStream) reads them.
    ///
    /// Fails with [`Error::MalformedMedia`] or [`Error::UnsupportedMedia`] if the
    /// initialization segment is not MP4 this crate reads.
    pub fn tracks(&self) -> Result<Vec<Track>, Error> {
        let (body, location) = self.init_body()?;
        track::tracks(&body, &location)
    }

    /// Returns each track's ID and content kind in initialization-segment order.
    ///
    /// Unlike [`Self::tracks`], this does not inspect codec configuration, so one
    /// unreadable codec does not prevent the other tracks from being listed. Malformed
    /// or unsupported MP4 structure still returns the same errors as [`Self::tracks`].
    pub(crate) fn track_kinds(&self) -> Result<Vec<(u32, ContentKind)>, Error> {
        let (body, location) = self.init_body()?;
        track::track_kinds(&body, &location)
    }

    /// Returns the retained initialization segment body and its location.
    ///
    /// Returns [`Error::Initialization`] if the stream no longer retains that segment.
    fn init_body(&self) -> Result<(Bytes, SegmentLocation), Error> {
        let init = self.shared.lock().init.clone();
        let body = init.body().ok_or_else(|| {
            Error::Initialization(
                "MPEG stream no longer holds its initialization segment".to_owned(),
            )
        })?;
        Ok((body, init.location.clone()))
    }

    /// [`Self::fetch_segment`], with where the segment sits.
    pub(crate) fn media_part_body(&self, number: SegmentNumber) -> Result<PartBody, Error> {
        let part = Part::Media(number);
        let (data, span) = {
            let mut schedule = self.shared.lock();
            let data = schedule
                .materialise(part, self.shared.supply())
                .cloned()
                .ok_or_else(|| {
                    Error::Initialization(format!(
                        "MPEG stream has no media segment {number} of its representation"
                    ))
                })?;
            (data, schedule.span(part))
        };

        // Fetch on this thread; the stream's own downloads carry on meanwhile.
        let body = self.shared.cache.get(&data)?;
        let kept = self.shared.lock().fetched(part);
        if !kept {
            self.shared.cache.store.forget_if_unheld(&data);
        }
        self.shared.condvar.notify_all();
        Ok(PartBody {
            part,
            number: Some(number),
            span,
            location: data.location.clone(),
            body,
            jumped: false,
        })
    }

    /// Fills unknown sizes until `known` holds, via on-demand HEAD or GET
    /// fallback. Also returns once every body is downloaded.
    fn ensure_sizes(&self, known: impl Fn(&Schedule) -> bool) -> io::Result<()> {
        loop {
            let step = self.shared.lock().size_step(&known, self.shared.supply());
            match step {
                SizeStep::Known => return Ok(()),
                // A failed HEAD disables probing, so the next step waits instead.
                SizeStep::Probe(part, data) => self.shared.probe(part, &data),
                SizeStep::Wait => {
                    self.shared.condvar.notify_all();
                    return self
                        .wait_until(|schedule| known(schedule) || schedule.complete)
                        .map(drop);
                }
            }
        }
    }

    fn wait_until<F>(&self, mut ready: F) -> io::Result<MutexGuard<'_, Schedule>>
    where
        F: FnMut(&Schedule) -> bool,
    {
        let mut inner = self.shared.lock();
        loop {
            self.check(&inner)?;
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

    /// Rechecks `step` after each schedule change until it returns a value or
    /// the stream fails.
    fn wait_for<T>(&self, mut step: impl FnMut(&mut Schedule) -> Option<T>) -> io::Result<T> {
        let mut inner = self.shared.lock();
        loop {
            self.check(&inner)?;
            if let Some(value) = step(&mut inner) {
                return Ok(value);
            }
            inner = self
                .shared
                .condvar
                .wait(inner)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    /// Returns the current cancellation or download error, if any.
    fn check(&self, schedule: &Schedule) -> io::Result<()> {
        if self.shared.cancelled.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "MPEG stream cancelled",
            ));
        }
        match &schedule.error {
            Some(error) => Err(io::Error::other(error.clone())),
            None => Ok(()),
        }
    }

    /// Prioritizes `part` and waits for the reader's next action.
    fn read_step(&self, part: Part) -> io::Result<ReadStep> {
        self.prioritize_segment(part);
        self.wait_for(|schedule| schedule.read_step(part, self.shared.supply()))
    }

    /// Moves the reader to the segment selected by a behind-window policy.
    fn jump(&mut self, to: Part, byte: Option<u64>, change: WindowChange) {
        self.shared.window_moved(change);
        self.cursor.place(to, 0, byte);
        self.jumped = true;
    }

    fn prioritize_segment(&self, part: Part) {
        let mut schedule = self.shared.lock();
        if schedule.missing_body(part) {
            schedule.request(part);
            self.shared.condvar.notify_all();
        }
    }

    /// Publishes the cursor's current part to the workers and moves the
    /// retained window with it.
    ///
    /// If the cursor is at the start of a part whose absolute byte offset is
    /// now known, records that offset for later byte-position queries.
    fn publish_cursor(&mut self) {
        let (change, start) = {
            let mut schedule = self.shared.lock();
            let change = schedule.move_cursor(self.cursor.part, self.shared.supply());
            (change, schedule.prefix_len(self.cursor.part))
        };
        self.cursor.learn_byte_position(start);
        self.shared.window_moved(change);
    }
}

/// Weak access to stream state retained after another component owns the
/// reader. Operations become no-ops or return `None` after the stream drops.
#[cfg(feature = "rodio")]
#[derive(Clone, Debug)]
pub(crate) struct StreamHandle(std::sync::Weak<StreamShared>);

#[cfg(feature = "rodio")]
impl SegmentStream {
    /// Returns weak access to this stream's shared state.
    pub(crate) fn handle(&self) -> StreamHandle {
        StreamHandle(Arc::downgrade(&self.shared))
    }

    /// Returns the presentation time of this stream's first media segment.
    pub(crate) fn join_time(&self) -> Option<Duration> {
        self.shared.lock().join_time()
    }
}

#[cfg(feature = "rodio")]
impl StreamHandle {
    /// Updates the behind-window policy if the stream still exists.
    pub(crate) fn set_behind_window(&self, policy: BehindWindow) {
        if let Some(shared) = self.0.upgrade() {
            shared.lock().set_behind_window(policy);
        }
    }

    /// Connects a buffer report to the stream if it still exists.
    pub(crate) fn subscribe_buffer(&self, feed: super::buffer::BufferFeed) {
        if let Some(shared) = self.0.upgrade() {
            shared.lock().subscribe_buffer(feed, shared.supply());
        }
    }

    /// Returns the current available range if the stream still exists.
    pub(crate) fn available(&self) -> Option<Range<Duration>> {
        let shared = self.0.upgrade()?;
        let now = shared.clock.now();
        shared.lock().available_time(now)
    }

    /// Returns the current live edge if the stream still exists.
    pub(crate) fn live_edge(&self) -> Option<Duration> {
        let shared = self.0.upgrade()?;
        let now = shared.clock.now();
        shared.lock().live_edge(now)
    }
}

impl StreamShared {
    fn lock(&self) -> MutexGuard<'_, Schedule> {
        self.schedule
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The inputs the schedule adds segments with, as of now.
    fn supply(&self) -> Supply<'_> {
        Supply {
            store: &self.cache.store,
            now: self.clock.now(),
        }
    }

    /// Applies cache ownership changes, evicts unused expired entries, and
    /// wakes the download worker.
    fn window_moved(&self, change: WindowChange) {
        let WindowChange {
            acquired,
            released,
            trimmed,
        } = change;
        self.cache.store.acquire(&acquired);
        self.cache.store.release(&released, true);
        // Drop temporary ownership before checking whether trimmed entries are unused.
        drop((acquired, released));
        self.cache.store.evict(trimmed);
        self.condvar.notify_all();
    }

    /// HEADs `data`, the segment at `part`, unless its size is known, or
    /// disables HEAD probing if the request fails.
    fn probe(&self, part: Part, data: &SegmentData) {
        if let Err(error) = self.cache.probe(data) {
            self.lock().probe_failed(part, &error);
        }
        self.condvar.notify_all();
    }
}

/// Starts the worker that downloads scheduled segments until cancellation or failure.
fn spawn_get_worker(shared: Arc<StreamShared>) {
    std::thread::spawn(move || {
        while let Some((part, data)) = next_get(&shared) {
            // Release data fetched only to discover its size once the request completes.
            let result = shared.cache.get(&data).map(drop);
            let kept = shared
                .lock()
                .finish_get(part, result, shared.clock.now());
            if !kept {
                shared.cache.store.forget_if_unheld(&data);
            }
            shared.condvar.notify_all();
        }
    });
}

/// Waits for and returns the next scheduled GET.
///
/// For live streams this also adopts refreshed indexes, refreshes the MPD when
/// due, and advances availability. Returns `None` after cancellation or failure.
fn next_get(shared: &StreamShared) -> Option<(Part, Arc<SegmentData>)> {
    let mut schedule = shared.lock();
    loop {
        if shared.cancelled.load(Ordering::SeqCst) || schedule.error.is_some() {
            return None;
        }

        // Adopt an index refreshed by any stream sharing this presentation.
        let refreshed = shared.live.as_ref().map(LiveLink::refreshed);
        let change = match refreshed {
            Some(Ok(Some(index))) => schedule.replace_index(index, shared.supply()),
            Some(Err(reason)) => {
                schedule.error = Some(reason);
                shared.condvar.notify_all();
                return None;
            }
            Some(Ok(None)) | None => WindowChange::default(),
        };
        if !change.is_empty() {
            drop(schedule);
            shared.window_moved(change);
            schedule = shared.lock();
            continue;
        }

        if let Some(work) = schedule.take_get(shared.clock.now()) {
            return Some(work);
        }

        // Nothing to fetch: take what has become available since.
        let (change, idle) = schedule.advance(shared.supply());
        if !change.is_empty() {
            drop(schedule);
            shared.window_moved(change);
            schedule = shared.lock();
            continue;
        }
        let plan = match &shared.live {
            Some(live) => live.plan(idle, shared.clock.now()),
            None => Plan::Wait(None),
        };

        // Refresh, finish, or wait for a reader, the next segment or the next refresh.
        shared.condvar.notify_all();
        let until = match plan {
            Plan::Refresh { ran_out } => {
                drop(schedule);
                let live = shared.live.as_ref()?;
                let refreshed =
                    live.live
                        .refresh_blocking(&*shared.cache.transport, live.seen(), ran_out);
                schedule = shared.lock();
                if let Err(error) = refreshed {
                    schedule.error = Some(error.to_string());
                    shared.condvar.notify_all();
                    return None;
                }
                continue;
            }
            Plan::Finish => {
                schedule.finish_index();
                shared.condvar.notify_all();
                None
            }
            Plan::Wait(until) => until,
        };
        schedule = match until {
            None => shared
                .condvar
                .wait(schedule)
                .unwrap_or_else(|error| error.into_inner()),
            Some(until) => {
                let timeout = until.duration_since(shared.clock.now()).unwrap_or_default();
                shared
                    .condvar
                    .wait_timeout(schedule, timeout)
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
        };
    }
}

fn spawn_head_worker(shared: Arc<StreamShared>) {
    std::thread::spawn(move || {
        loop {
            if shared.cancelled.load(Ordering::SeqCst) {
                return;
            }

            // Exit and spawn decisions share the lock so start/stop cannot
            // leave zero or two workers.
            let next = shared.lock().next_eager_head(shared.supply());
            let Some((part, data)) = next else {
                // Wake anyone waiting for this worker to stop.
                shared.condvar.notify_all();
                return;
            };

            // After a failure, `next_head` is `None` and the worker exits.
            shared.probe(part, &data);
        }
    });
}

impl Read for SegmentStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            // Resolve the current part, including any required live-window recovery.
            let part = self.cursor.part;
            let body = match self.read_step(part)? {
                ReadStep::Ready(found) => found.body,
                ReadStep::End => {
                    let _schedule = self.wait_until(|schedule| schedule.complete)?;
                    return Ok(0);
                }
                ReadStep::Behind { available } => {
                    return Err(io::Error::other(Error::BehindLiveWindow { available }));
                }
                ReadStep::Jumped { to, byte, change } => {
                    self.jump(to, byte, change);
                    continue;
                }
            };
            self.jumped = false;

            // Copy available bytes and publish a cursor move at segment boundaries.
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

impl SegmentStream {
    /// The rest of the current segment, as [`Iterator::next`] yields it, with
    /// where it sits.
    pub(crate) fn next_part(&mut self) -> Option<Result<PartBody, Error>> {
        if self.failed {
            return None;
        }
        loop {
            // Resolve the current part, including any required live-window recovery.
            let part = self.cursor.part;
            let step = self.read_step(part).map_err(Error::from);
            let found = match step {
                Ok(ReadStep::Ready(found)) => found,
                Ok(ReadStep::End) => return None,
                Ok(ReadStep::Behind { available }) => {
                    self.failed = true;
                    return Some(Err(Error::BehindLiveWindow { available }));
                }
                Ok(ReadStep::Jumped { to, byte, change }) => {
                    self.jump(to, byte, change);
                    continue;
                }
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            };

            // Return the unread suffix and advance to the next part.
            let rest = self.cursor.take_rest(found.body.clone());
            self.publish_cursor();
            if let Some(body) = rest {
                let jumped = std::mem::take(&mut self.jumped);
                return Some(Ok(PartBody {
                    body,
                    jumped,
                    ..found
                }));
            }
        }
    }

    /// Whether nothing has been read yet: the position is the start of the
    /// initialization segment.
    pub(crate) fn at_start(&self) -> bool {
        self.cursor.part == Part::Init && self.cursor.offset_in_segment == 0
    }

    /// The representation's media segments.
    pub(crate) fn representation_index(&self) -> Arc<MediaIndex> {
        self.shared.lock().index().clone()
    }

    /// Returns the earliest media segment currently reachable by the stream.
    pub(crate) fn earliest_number(&self) -> SegmentNumber {
        self.shared.lock().earliest(self.shared.clock.now())
    }

    /// Clamps `time` as [`Self::seek_time`] would.
    ///
    /// Fails if a live stream currently has no available segment.
    pub(crate) fn clamp_time(&self, time: Duration) -> io::Result<Duration> {
        self.shared.lock().clamp_time(time, self.shared.clock.now())
    }

    /// Returns the number of media entries currently held in the schedule.
    #[cfg(test)]
    pub(crate) fn scheduled_len(&self) -> usize {
        self.shared.lock().media_data().count()
    }

    /// Wakes the test worker so it rechecks the manual clock.
    #[cfg(test)]
    pub(crate) fn wake_worker(&self) {
        let _schedule = self.shared.lock();
        self.shared.condvar.notify_all();
    }
}

impl Iterator for SegmentStream {
    type Item = Result<Bytes, Error>;

    /// The rest of the current segment: a whole segment, unless reading or a
    /// byte seek left the position inside one. Blocks while it downloads.
    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_part()?.map(|part| part.body))
    }
}

impl Seek for SegmentStream {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.seek_bytes(from)?
            .byte
            .ok_or_else(|| io::Error::other("MPEG stream byte position is unknown"))
    }
}

impl Drop for SegmentStream {
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
    use crate::segment::schedule::TimePosition;
    use crate::segment::{SegmentLocation, parse_byte_range, timed_for_test};
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
        G: Fn(&SegmentLocation, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync,
        H: Fn(&SegmentLocation) -> Result<u64, Error> + Send + Sync,
    {
        fn get(&self, segment: &SegmentLocation, size_hint: Option<u64>) -> Result<Bytes, Error> {
            (self.get)(segment, size_hint).map(Bytes::from)
        }

        fn head(&self, segment: &SegmentLocation) -> Result<u64, Error> {
            (self.head)(segment)
        }
    }

    /// Test timing: one duration per media segment, in `timescale` ticks,
    /// back to back from time zero.
    #[derive(Clone)]
    struct Durations {
        timescale: u32,
        media_durations: Vec<u64>,
    }

    impl Durations {
        /// `segments`, timed by these durations.
        fn time(&self, segments: Segments) -> Segments {
            timed_for_test(
                segments.init().clone(),
                segments.media(),
                self.timescale,
                &self.media_durations,
            )
        }
    }

    /// A stream fetching through GET and HEAD closures, probing sizes in the
    /// background from the start when `eager_head` is set. `durations`, when
    /// given, times `segments`.
    fn closure_reader<G, H>(
        segments: Segments,
        durations: Option<Durations>,
        eager_head: bool,
        get: G,
        head: H,
    ) -> Result<SegmentStream, Error>
    where
        G: Fn(&SegmentLocation, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync + 'static,
        H: Fn(&SegmentLocation) -> Result<u64, Error> + Send + Sync + 'static,
    {
        let segments = match durations {
            Some(durations) => durations.time(segments),
            None => segments,
        };
        let transport = Arc::new(FnTransport { get, head });
        let stream = SegmentStream::new_with_transport(segments, transport)?;
        if eager_head {
            stream.start_eager_head();
        }
        Ok(stream)
    }

    /// [`SegmentStream::new_with_cache`], probing sizes in the background
    /// from the start when `eager_head` is set.
    fn cached_stream(
        segments: Segments,
        eager_head: bool,
        cache: Arc<SegmentCache>,
    ) -> Result<SegmentStream, Error> {
        let stream = SegmentStream::new_with_cache(segments, cache)?;
        if eager_head {
            stream.start_eager_head();
        }
        Ok(stream)
    }

    /// The part of the media segment numbered `number`.
    fn media(number: u64) -> Part {
        Part::Media(SegmentNumber(number))
    }

    /// The media segment numbered `number` in `schedule`.
    fn data(schedule: &Schedule, number: u64) -> &Arc<SegmentData> {
        schedule
            .part_data(media(number))
            .expect("a scheduled segment")
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
    ) -> impl Fn(&SegmentLocation, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |segment: &SegmentLocation, _expected_len: Option<u64>| {
            let url = segment.url.clone();
            payloads
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, bytes)| bytes.to_vec())
                .ok_or_else(|| Error::Initialization(format!("unknown url {url}")))
        }
    }

    fn instant_head(
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> impl Fn(&SegmentLocation) -> Result<u64, Error> + Send + Sync {
        move |segment: &SegmentLocation| {
            let url = segment.url.clone();
            payloads
                .iter()
                .find(|(name, _)| *name == url)
                .map(|(_, bytes)| bytes.len() as u64)
                .ok_or_else(|| Error::Initialization(format!("unknown url {url}")))
        }
    }

    fn failing_head() -> impl Fn(&SegmentLocation) -> Result<u64, Error> + Send + Sync {
        move |_segment: &SegmentLocation| Err(Error::Initialization("HEAD unavailable".into()))
    }

    fn start_instant(
        segments: Segments,
        payloads: &'static [(&'static str, &'static [u8])],
    ) -> Result<SegmentStream, Error> {
        closure_reader(
            segments,
            None,
            true,
            instant_get(payloads),
            instant_head(payloads),
        )
    }

    fn gated_get(
        allow_rest: Arc<AtomicBool>,
        payload: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    ) -> impl Fn(&SegmentLocation, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |segment: &SegmentLocation, _expected_len: Option<u64>| {
            let url = segment.url.clone();
            if url != "init" {
                while !allow_rest.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(payload(&url))
        }
    }

    #[test]
    fn reads_init_then_media_segments() {
        let mut reader = start_instant(
            Segments::new("init", ["a", "b"]),
            &[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BB")],
        )
        .unwrap();

        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"INITAAAABB");
    }

    #[test]
    fn blocks_until_later_segments_arrive() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            Segments::new("init", ["more"]),
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
            Segments::new("init", ["body"]),
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
            Segments::new("init", ["tail"]),
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
            Segments::new("init", ["a", "b"]),
            None,
            true,
            {
                let allow_get = allow_get.clone();
                let get_order = get_order.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                move |segment: &SegmentLocation| {
                    let url = segment.url.clone();
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        // The init and "a" are 4 bytes each, so byte 9 is one byte into "b".
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
            Segments::new("init", ["a", "b"]),
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
            move |segment: &SegmentLocation| {
                let url = segment.url.clone();
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
            move |segment: &SegmentLocation, size_hint: Option<u64>| {
                if segment.url == "a" {
                    a_gets.fetch_add(1, Ordering::SeqCst);
                }
                gated(segment, size_hint)
            }
        };
        // Lie about "a" so its GET will disagree with its HEAD.
        let head = |segment: &SegmentLocation| match segment.url.as_str() {
            "a" => Ok(99),
            url => Ok(url.len() as u64),
        };
        let cache = Arc::new(SegmentCache::new(Arc::new(FnTransport { get, head })));
        let segments = || Segments::new("init", ["a"]);

        // The first stream probes "a", then fetches it.
        let mut first = cached_stream(segments(), true, cache.clone()).unwrap();
        wait_for(|| cache.known_size(&"a".into()) == Some(99));
        allow_get.store(true, Ordering::SeqCst);
        let mut bytes = Vec::new();
        first.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"INITA");

        // The second stream finds "a" already held, so its worker never
        // fetches it and only its reader sees the body.
        let mut second = cached_stream(segments(), false, cache.clone()).unwrap();
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
            Segments::new("init", ["a", "b", "c"]),
            Some(Durations {
                timescale: 1,
                media_durations: vec![1, 1, 1],
            }),
            true,
            {
                let allow_a = allow_a.clone();
                let allow_b = allow_b.clone();
                let allow_c = allow_c.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| match segment
                    .url
                    .as_str()
                {
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
            let position = reader.seek_time(Duration::from_secs(2)).unwrap();
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
                    accuracy: TimeAccuracy::Packet,
                }),
            }
        );
        assert_eq!(kind, io::ErrorKind::InvalidInput);

        allow_b.store(true, Ordering::SeqCst);
    }

    #[test]
    fn seek_time_maps_timeline_to_segment_starts() {
        let timeline = Durations {
            timescale: 100,
            media_durations: vec![100, 100],
        };
        let mut reader = closure_reader(
            Segments::new("init", ["a", "b"]),
            Some(timeline),
            true,
            instant_get(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
            instant_head(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
        )
        .unwrap();

        reader.read_to_end(&mut Vec::new()).unwrap();

        assert_eq!(
            reader.seek_time(Duration::from_secs(0)).unwrap(),
            position(Some(4), Some((Duration::ZERO, TimeAccuracy::Packet))),
            "time 0 is the first media segment, after init"
        );
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"AAAA");

        assert_eq!(
            reader.seek_time(Duration::from_secs(1)).unwrap(),
            position(
                Some(8),
                Some((Duration::from_secs(1), TimeAccuracy::Packet))
            )
        );
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"BBBB");

        assert_eq!(
            reader.seek_time(Duration::from_millis(500)).unwrap(),
            position(Some(4), Some((Duration::ZERO, TimeAccuracy::Packet))),
            "seeks land at the containing segment's moof, and report its start exactly"
        );
        reader.read_exact(&mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], b"AA");
    }

    #[test]
    fn seek_time_prioritizes_target_segment() {
        let allow_a = Arc::new(AtomicBool::new(false));
        let allow_b = Arc::new(AtomicBool::new(false));
        let allow_c = Arc::new(AtomicBool::new(false));
        let a_started = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            Segments::new("init", ["a", "b", "c"]),
            Some(Durations {
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
                move |segment: &SegmentLocation, _expected_len: Option<u64>| match segment
                    .url
                    .as_str()
                {
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
            let position = reader.seek_time(Duration::from_secs(2)).unwrap();
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
                    accuracy: TimeAccuracy::Packet,
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
            Segments::new("init", ["a", "b", "c", "d"]),
            Some(Durations {
                timescale: 1,
                media_durations: vec![1, 1, 1, 1],
            }),
            true,
            {
                let release_media = release_media.clone();
                let order = order.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                if cache.lock().cursor >= media(2) {
                    release_media_when_seeked.store(true, Ordering::SeqCst);
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        reader.seek_time(Duration::from_secs(2)).unwrap();
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
    fn seek_time_returns_known_when_prefix_filled() {
        let timeline = Durations {
            timescale: 1,
            media_durations: vec![1, 1],
        };
        let mut reader = closure_reader(
            Segments::new("init", ["a", "b"]),
            Some(timeline),
            true,
            instant_get(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
            instant_head(&[("init", b"INIT"), ("a", b"AAAA"), ("b", b"BBBB")]),
        )
        .unwrap();

        reader.read_to_end(&mut Vec::new()).unwrap();
        assert_eq!(
            reader.seek_time(Duration::from_secs(1)).unwrap(),
            position(
                Some(8),
                Some((Duration::from_secs(1), TimeAccuracy::Packet))
            )
        );
    }

    #[test]
    fn seek_bytes_backfills_leading_segments_without_head() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            Segments::new("init", ["aaaa", "bbbb"]),
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
    fn seek_time_requires_timeline() {
        let mut reader = start_instant(
            Segments::new("init", Vec::<SegmentLocation>::new()),
            &[("init", b"xyz")],
        )
        .unwrap();
        let error = reader.seek_time(Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn eof_after_complete_download() {
        let mut reader = start_instant(
            Segments::new("init", Vec::<SegmentLocation>::new()),
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
            move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                let url = segment.url.clone();
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(b"INIT".to_vec())
                } else {
                    Err(Error::Initialization(format!("failed to fetch {url}")))
                }
            }
        };

        let mut reader = closure_reader(
            Segments::new("init", ["bad"]),
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

    fn abc() -> Segments {
        Segments::new("init", ["a", "b"])
    }

    fn texts(items: impl Iterator<Item = Result<Bytes, Error>>) -> Vec<String> {
        items
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn iterates_whole_segments() {
        let stream = start_instant(abc(), PAYLOADS).unwrap();
        assert_eq!(texts(stream), ["INIT", "aaaa", "bb"]);
    }

    #[test]
    fn iterating_and_reading_share_one_position() {
        let mut stream = start_instant(abc(), PAYLOADS).unwrap();
        let mut head = [0; 6];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"INITaa");

        // The rest of the segment the read stopped in, then reading resumes after it.
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
        let timeline = Durations {
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
        stream.seek_time(Duration::from_millis(1_500)).unwrap();
        assert_eq!(texts(stream), ["bb"]);
    }

    #[test]
    fn iteration_ends_after_a_download_failure() {
        let release = Arc::new(AtomicBool::new(false));
        let get = {
            let release = release.clone();
            move |segment: &SegmentLocation, _expected_len: Option<u64>| match segment.url.as_str()
            {
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
            Segments::new("init", ["bad", "never"]),
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
            Segments::new("init", ["a", "b", "c", "d", "e"]),
            None,
            true,
            {
                let allow_late = allow_late.clone();
                let get_expected = get_expected.clone();
                move |segment: &SegmentLocation, expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                move |segment: &SegmentLocation| {
                    let url = segment.url.clone();
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        let inner = reader
            .wait_until(|inner| data(inner, 3).size().is_some() && data(inner, 4).size().is_some())
            .unwrap();
        assert!(
            inner.missing_body(media(3)) && inner.missing_body(media(4)),
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
            Segments::new("init", ["a", "b", "c"]),
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                move |segment: &SegmentLocation| {
                    let url = segment.url.clone();
                    head_calls.lock().unwrap().push(url.clone());
                    Ok(4)
                }
            },
        )
        .unwrap();

        let inner = reader.wait_until(|inner| inner.all_sizes_known()).unwrap();
        assert!(
            (0..3).all(|number| inner.missing_body(media(number))),
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
            Segments::new("init", ["a", "b", "c"]),
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
                move |_segment: &SegmentLocation| {
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
            Segments::new("init", ["a", "b", "c", "d"]),
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                move |_segment: &SegmentLocation| {
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
    ) -> impl Fn(&SegmentLocation) -> Result<u64, Error> + Send + Sync {
        move |segment: &SegmentLocation| {
            let url = segment.url.clone();
            calls.lock().unwrap().push(url);
            Ok(4)
        }
    }

    #[test]
    fn stopped_eager_head_spawns_no_worker() {
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let reader = closure_reader(
            Segments::new("init", ["a", "b"]),
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
            Segments::new("init", ["a", "b", "c"]),
            None,
            false,
            {
                let allow_get = allow_get.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
        assert!(inner.media_data().all(|data| data.body().is_none()));
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
            Segments::new("init", ["a", "b", "c", "d"]),
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                move |_segment: &SegmentLocation| {
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
            data(&inner, 0).size(),
            Some(4),
            "in-flight HEAD should be kept"
        );
        assert!(inner.media_data().skip(1).all(|data| data.size().is_none()));
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
            Segments::new("init", ["a", "b", "c"]),
            None,
            true,
            {
                let allow_get = allow_get.clone();
                move |segment: &SegmentLocation, _expected_len: Option<u64>| {
                    let url = segment.url.clone();
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
                move |_segment: &SegmentLocation| {
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
            Segments::new("init", ["a"]),
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
            Segments::new("init", ["a", "b"]),
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

    /// Two one-second segments.
    fn two_second_timeline() -> Durations {
        Durations {
            timescale: 44_100,
            media_durations: vec![44_100, 44_100],
        }
    }

    #[test]
    fn symphonia_compat_end_zero_does_not_wait_for_bodies() {
        let allow_rest = Arc::new(AtomicBool::new(false));
        let mut reader = closure_reader(
            Segments::new("init", ["a", "b"]),
            Some(two_second_timeline()),
            true,
            gated_get(allow_rest.clone(), |url| vec![url.as_bytes()[0]; 4]),
            failing_head(),
        )
        .unwrap();
        reader.set_symphonia_compat(true, 128_000).unwrap();

        let end = reader.seek_bytes(SeekFrom::End(0)).unwrap();
        let estimate = two_second_timeline()
            .time(Segments::new("init", ["a", "b"]))
            .estimate_len(4, 128_000)
            .unwrap();
        assert_eq!(end.byte, Some(estimate));
        assert_eq!(
            end.time,
            Some(TimePosition {
                timestamp: Duration::from_secs(2),
                accuracy: TimeAccuracy::Packet,
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
            Segments::new("init", ["a"]),
            Some(Durations {
                timescale: 44_100,
                media_durations: vec![44_100],
            }),
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
            Segments::new("init", ["a"]),
            &[("init", b"INIT"), ("a", b"AAAA")],
        )
        .unwrap();
        let error = reader.set_symphonia_compat(true, 128_000).unwrap_err();
        assert!(
            matches!(error, Error::Initialization(message) if message.contains("media timeline"))
        );
    }

    /// Serves byte ranges of one in-memory file, keyed by segment range.
    fn ranged_get(
        file: &'static [u8],
    ) -> impl Fn(&SegmentLocation, Option<u64>) -> Result<Vec<u8>, Error> + Send + Sync {
        move |segment: &SegmentLocation, _expected_len: Option<u64>| {
            let range = segment.range.as_ref().expect("test segments are ranged");
            Ok(file[*range.start() as usize..=*range.end() as usize].to_vec())
        }
    }

    fn ranged(url: &str, start: u64, end: u64) -> SegmentLocation {
        SegmentLocation::with_range(url, start..=end)
    }

    #[test]
    fn ranged_segments_know_sizes_without_head() {
        const FILE: &[u8] = b"INITaaaaBBBBBBcc";
        let allow_media = Arc::new(AtomicBool::new(false));
        let head_calls = Arc::new(Mutex::new(Vec::new()));
        let mut reader = closure_reader(
            Segments::new(
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
                move |segment: &SegmentLocation, expected_len: Option<u64>| {
                    assert_eq!(expected_len, segment.known_size());
                    if *segment.range.as_ref().unwrap().start() > 0 {
                        while !allow_media.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    get(segment, expected_len)
                }
            },
            recording_head(head_calls.clone()),
        )
        .unwrap();

        // Every size comes from a range, so byte seeks resolve before any media body arrives.
        assert_eq!(reader.seek_bytes(SeekFrom::End(0)).unwrap().byte, Some(16));
        assert_eq!(reader.seek_bytes(SeekFrom::Start(9)).unwrap().byte, Some(9));
        assert!(reader.shared.lock().missing_body(media(1)));

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
    fn ranged_segment_with_wrong_length_fails() {
        let result = closure_reader(
            Segments::new(ranged("file", 0, 7), Vec::<SegmentLocation>::new()),
            None,
            false,
            |_segment: &SegmentLocation, _expected_len: Option<u64>| Ok(b"INIT".to_vec()),
            failing_head(),
        );
        assert!(matches!(
            result,
            Err(Error::SegmentLength { segment, expected: 8, actual: 4 })
                if segment == "file (bytes 0-7)"
        ));

        let mut reader = closure_reader(
            Segments::new(ranged("file", 0, 3), [ranged("file", 4, 7)]),
            None,
            false,
            |segment: &SegmentLocation, _expected_len: Option<u64>| match *segment
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
        let mut reader = SegmentStream::new(Segments::new(
            ranged(&format!("{base}/ranged"), 0, 3),
            [
                ranged(&format!("{base}/plain"), 8, 13),
                ranged(&format!("{base}/ranged"), 14, 15),
            ],
        ))
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
        fn head(&self, segment: &SegmentLocation) -> Result<u64, Error> {
            self.heads.lock().unwrap().push(segment.url.clone());
            Ok(segment.url.len() as u64)
        }

        fn get(&self, segment: &SegmentLocation, _size_hint: Option<u64>) -> Result<Bytes, Error> {
            self.gets.lock().unwrap().push(segment.url.clone());
            if self.gated.contains(&segment.url.as_str()) {
                while !self.released.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(Bytes::copy_from_slice(segment.url.as_bytes()))
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
    ) -> (Arc<SegmentCache>, Arc<CountingTransport>) {
        let transport = Arc::new(CountingTransport::new(gated));
        (Arc::new(SegmentCache::new(transport.clone())), transport)
    }

    #[test]
    fn segment_cache_reuses_bodies_across_readers() {
        let (cache, transport) = counting_cache(&[]);
        let mut first =
            SegmentStream::new_with_cache(Segments::new("init", ["a", "b"]), cache.clone())
                .unwrap();
        let mut bytes = Vec::new();
        first.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initab");

        // A rebuilt reader, as after a seek, gets everything from memory.
        let mut second = cached_stream(Segments::new("init", ["b"]), true, cache.clone()).unwrap();
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
        let reader = SegmentStream::new_with_cache(
            timed_for_test("init", ["aa", "bb"], 10, &[10, 10]),
            cache,
        )
        .unwrap();
        let buffer = reader.buffer().unwrap();
        let reported = Arc::new(Mutex::new(Vec::new()));
        buffer.on_change({
            let reported = reported.clone();
            move |status: &BufferStatus| reported.lock().unwrap().push(status.clone())
        });

        // "bb" is held back, so only the first one-second segment is here.
        wait_for(|| buffer.status().downloaded.segments == 1);
        let status = buffer.status();
        assert_eq!(status.total.segments, 2);
        assert_eq!(status.total.end, Duration::from_secs(2));
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

    #[test]
    fn fetch_segment_does_not_move_the_stream() {
        let (cache, transport) = counting_cache(&[]);
        // Keep nothing behind, and only the next second ahead.
        let (stream, _) =
            SegmentStream::starting_at(uneven(), Duration::from_secs(2), cache.clone()).unwrap();
        let mut stream = stream.with_retention(Retention {
            behind: Some(Duration::ZERO),
            ahead: Some(Duration::from_secs(1)),
        });
        assert_eq!(stream.next().unwrap().unwrap(), &b"init"[..]);

        // An index beyond the representation still fails.
        assert!(matches!(
            stream.fetch_segment(3),
            Err(Error::Initialization(_))
        ));

        // Side-fetches on either side do not move the iterator cursor.
        assert_eq!(stream.fetch_segment(0).unwrap(), &b"a"[..]);
        assert_eq!(stream.fetch_segment(2).unwrap(), &b"c"[..]);
        assert_eq!(stream.next().unwrap().unwrap(), &b"b"[..]);
        assert_eq!(stream.next().unwrap().unwrap(), &b"c"[..]);
        assert!(stream.next().is_none());

        // Once outside retention, a side-fetch downloads the segment again without keeping it.
        wait_for(|| cache.cached(&"b".into()).is_none());
        assert_eq!(stream.fetch_segment(1).unwrap(), &b"b"[..]);
        assert_eq!(cache.cached(&"b".into()), None);
        assert_eq!(transport.gets().iter().filter(|url| *url == "b").count(), 2);
    }

    #[test]
    fn fetch_segment_errors_are_the_callers_alone() {
        let failed = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let get = {
            let (failed, released) = (failed.clone(), released.clone());
            move |segment: &SegmentLocation, _: Option<u64>| match segment.url.as_str() {
                // The stream's own downloads wait at "a" until released.
                "a" => {
                    while !released.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Ok(b"a".to_vec())
                }
                // The first GET of "b" fails; later ones succeed.
                "b" if !failed.swap(true, Ordering::SeqCst) => {
                    Err(Error::Initialization("refused".to_owned()))
                }
                url => Ok(url.as_bytes().to_vec()),
            }
        };
        let mut stream = closure_reader(
            Segments::new("init", ["a", "b"]),
            None,
            false,
            get,
            failing_head(),
        )
        .unwrap();

        // The side fetch is the first GET of "b", and fails.
        assert!(matches!(
            stream.fetch_segment(1),
            Err(Error::Initialization(reason)) if reason == "refused"
        ));

        // The stream carries on, and downloads "b" itself.
        released.store(true, Ordering::SeqCst);
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initab");
    }

    /// Four one-second segments, whose bodies are their names.
    fn abcd() -> Segments {
        timed_for_test("init", ["a", "b", "c", "d"], 1, &[1; 4])
    }

    /// Keeps nothing behind the position and `ahead` seconds ahead of it.
    fn ahead(ahead: u64) -> Retention {
        Retention {
            behind: Some(Duration::ZERO),
            ahead: Some(Duration::from_secs(ahead)),
        }
    }

    #[test]
    fn retention_at_open_schedules_only_its_window() {
        let (cache, transport) = counting_cache(&[]);
        let mut stream = SegmentStream::new_retained(abcd(), cache.clone(), ahead(1)).unwrap();
        assert_eq!(cache.store.len(), 2, "the init and the first segment");

        // Reading on reaches the rest.
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initabcd");
        assert_eq!(cache.store.len(), 5);
        assert_eq!(transport.gets(), ["init", "a", "b", "c", "d"]);
    }

    #[test]
    fn byte_seeks_reach_past_a_bounded_window() {
        let (cache, transport) = counting_cache(&[]);
        let mut stream = SegmentStream::new_retained(abcd(), cache.clone(), ahead(0)).unwrap();

        // The end needs every size, which HEAD learns without downloading.
        assert_eq!(stream.seek_bytes(SeekFrom::End(0)).unwrap().byte, Some(8));
        for url in ["b", "c", "d"] {
            assert!(transport.heads().contains(&url.to_owned()), "{url}");
        }
        let seek = stream.seek_bytes(SeekFrom::Start(6)).unwrap();
        assert_eq!(
            seek.time.map(|time| time.timestamp),
            Some(Duration::from_secs(2))
        );
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"cd");
        assert!(!transport.gets().contains(&"b".to_owned()));
    }

    #[test]
    fn eager_head_learns_sizes_past_a_bounded_window() {
        let (cache, _) = counting_cache(&[]);
        let stream = SegmentStream::new_retained(abcd(), cache, ahead(0)).unwrap();
        stream.start_eager_head();
        wait_for(|| stream.shared.lock().total_len() == Some(8));
    }

    #[test]
    fn byte_seeks_without_head_fail_where_a_bounded_window_stops_short() {
        let open = || {
            let get =
                |segment: &SegmentLocation, _: Option<u64>| Ok(segment.url.as_bytes().to_vec());
            let durations = Durations {
                timescale: 1,
                media_durations: vec![1; 4],
            };
            let segments = Segments::new("init", ["a", "b", "c", "d"]);
            closure_reader(segments, Some(durations), false, get, failing_head())
                .unwrap()
                .with_retention(ahead(0))
        };

        // Neither an end-relative nor an absolute seek lands anywhere.
        for from in [SeekFrom::End(0), SeekFrom::End(-1), SeekFrom::Start(7)] {
            let mut stream = open();
            let error = stream.seek_bytes(from).unwrap_err();
            assert!(
                error.to_string().contains("HEAD requests failed"),
                "{from:?}: {error}"
            );

            // The stream reads on from where it was.
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"initabcd", "{from:?}");
        }
    }

    #[test]
    fn fetch_segment_reaches_past_a_bounded_window() {
        let (cache, _) = counting_cache(&[]);
        let stream = SegmentStream::new_retained(abcd(), cache, ahead(0)).unwrap();
        assert_eq!(stream.fetch_segment(3).unwrap(), &b"d"[..]);
        assert!(matches!(
            stream.fetch_segment(4),
            Err(Error::Initialization(_))
        ));
    }

    /// Segments of 1.5, 2 and 1 seconds, whose bodies are their names.
    fn uneven() -> Segments {
        timed_for_test("init", ["a", "b", "c"], 10, &[15, 20, 10])
    }

    #[test]
    fn starting_at_reads_init_then_media_from_the_covering_segment() {
        let (cache, transport) = counting_cache(&[]);
        let (mut stream, start) =
            SegmentStream::starting_at(uneven(), Duration::from_millis(2_750), cache).unwrap();
        assert_eq!(start, Duration::from_millis(1_500));
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initbc");
        assert_eq!(transport.gets(), ["init", "b", "c"]);

        // Buffer and seek times remain relative to the full presentation.
        let status = stream.buffer().unwrap().status();
        assert_eq!(
            status.ranges,
            [Duration::from_millis(1_500)..Duration::from_millis(4_500)]
        );
        assert_eq!(status.total.segments, 3, "the representation's, all reachable");
        assert_eq!(status.total.start, Duration::ZERO);
        assert_eq!(
            stream.seek_time(Duration::from_millis(3_500)).unwrap(),
            position(
                Some(5),
                Some((Duration::from_millis(3_500), TimeAccuracy::Packet))
            )
        );
        assert_eq!(stream.next().unwrap().unwrap(), &b"c"[..]);

        // Media before the join point remains seekable by time but has no stream byte offset.
        assert_eq!(
            stream.seek_time(Duration::from_secs(1)).unwrap(),
            position(None, Some((Duration::ZERO, TimeAccuracy::Packet)))
        );
        assert_eq!(stream.next().unwrap().unwrap(), &b"a"[..]);

        // Reaching the stream's initial segment restores absolute byte positions.
        let here = stream.seek_bytes(SeekFrom::Current(0)).unwrap();
        assert_eq!(here.byte, Some(4), "just past the initialization segment");
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"bc");
        assert_eq!(stream.seek_bytes(SeekFrom::Current(0)).unwrap().byte, Some(6));
    }

    #[test]
    fn starting_at_past_the_end_starts_at_the_last_segment() {
        let (cache, _transport) = counting_cache(&[]);
        let (stream, start) =
            SegmentStream::starting_at(uneven(), Duration::from_secs(60), cache).unwrap();
        assert_eq!(start, Duration::from_millis(3_500));
        assert_eq!(texts(stream), ["init", "c"]);
    }

    #[test]
    fn starting_at_needs_timed_media() {
        let (cache, _transport) = counting_cache(&[]);
        let empty = SegmentStream::starting_at(
            timed_for_test("init", Vec::<SegmentLocation>::new(), 10, &[]),
            Duration::ZERO,
            cache.clone(),
        );
        assert!(matches!(empty, Err(Error::Initialization(_))));

        let untimed =
            SegmentStream::starting_at(Segments::new("init", ["a"]), Duration::ZERO, cache);
        assert!(matches!(untimed, Err(Error::Initialization(_))));
    }

    #[test]
    fn buffer_needs_timed_segments() {
        let (cache, _transport) = counting_cache(&[]);
        let reader = SegmentStream::new_with_cache(Segments::new("init", ["a"]), cache).unwrap();
        assert!(reader.buffer().is_none());
    }

    #[test]
    fn segment_cache_keeps_in_flight_bodies_after_reader_drop() {
        let (cache, transport) = counting_cache(&["b"]);
        let first = SegmentStream::new_with_cache(Segments::new("init", ["a", "b"]), cache.clone())
            .unwrap();
        wait_for(|| transport.gets().contains(&"b".to_owned()));
        drop(first);

        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| cache.cached(&"b".into()).is_some());

        let mut second =
            SegmentStream::new_with_cache(Segments::new("init", ["b"]), cache.clone()).unwrap();
        let mut bytes = Vec::new();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
    }

    #[test]
    fn segment_cache_joins_a_get_already_in_flight() {
        let (cache, transport) = counting_cache(&["b"]);
        let first = SegmentStream::new_with_cache(Segments::new("init", ["a", "b"]), cache.clone())
            .unwrap();
        wait_for(|| transport.gets().contains(&"b".to_owned()));
        drop(first);

        // The rebuilt reader waits on the dropped reader's GET of "b".
        let mut second =
            SegmentStream::new_with_cache(Segments::new("init", ["b"]), cache.clone()).unwrap();
        transport.released.store(true, Ordering::SeqCst);
        let mut bytes = Vec::new();
        second.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"initb");
        assert_eq!(transport.gets(), ["init", "a", "b"]);
    }

    #[test]
    fn segment_cache_skips_wrong_length_ranged_bodies() {
        let cache = SegmentCache::new(Arc::new(FnTransport {
            get: |_segment: &SegmentLocation, _size_hint: Option<u64>| Ok(b"short".to_vec()),
            head: |_segment: &SegmentLocation| Ok(0),
        }));
        let segment = ranged("file", 0, 7);
        assert!(matches!(
            cache.get(&cache.data(segment.clone())),
            Err(Error::SegmentLength {
                expected: 8,
                actual: 5,
                ..
            })
        ));
        assert_eq!(cache.cached(&segment), None);

        let whole = SegmentLocation::new("file");
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
    fn segment_cache_reuses_pooled_connections_across_readers() {
        let (base, connections) = spawn_keep_alive_server();
        let segments =
            |media: &str| Segments::new(format!("{base}/init"), [format!("{base}/{media}")]);
        let read_all = |mut reader: SegmentStream| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        };

        let cache = Arc::new(SegmentCache::default());
        let first = SegmentStream::new_with_cache(segments("a"), cache.clone()).unwrap();
        assert_eq!(read_all(first), b"inita");
        let second = SegmentStream::new_with_cache(segments("b"), cache.clone()).unwrap();
        assert_eq!(read_all(second), b"initb");
        assert_eq!(connections.load(Ordering::SeqCst), 1);

        // Separate agents cannot share the connection.
        let third = SegmentStream::new(segments("c")).unwrap();
        assert_eq!(read_all(third), b"initc");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn segment_cache_remembers_head_lengths_across_readers() {
        let (cache, transport) = counting_cache(&["a", "bb"]);
        let segments = || Segments::new("init", ["a", "bb"]);
        let first = cached_stream(segments(), true, cache.clone()).unwrap();
        wait_for(|| cache.known_size(&"bb".into()).is_some());
        drop(first);

        // The rebuilt reader starts with every size and sends no HEAD at all.
        let mut second = cached_stream(segments(), true, cache.clone()).unwrap();
        assert!(second.shared.lock().all_sizes_known());
        assert_eq!(second.seek_bytes(SeekFrom::End(0)).unwrap().byte, Some(7));
        assert_eq!(transport.heads(), ["a", "bb"]);
        assert_eq!(cache.cached(&"bb".into()), None);

        // A later GET's body takes precedence over the HEAD length.
        transport.released.store(true, Ordering::SeqCst);
        wait_for(|| cache.cached(&"bb".into()).is_some());
        assert_eq!(cache.known_size(&"bb".into()), Some(2));
    }

    /// The init and five one-second media segments named "1" to "5", whose
    /// bodies are their names.
    fn one_second_stream(cache: Arc<SegmentCache>) -> SegmentStream {
        let segments = timed_for_test("init", ["1", "2", "3", "4", "5"], 1, &[1; 5]);
        SegmentStream::new_with_cache(segments, cache).unwrap()
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

        // Two seconds ahead of the start is segments 1 and 2, then it waits.
        wait_for(|| stream.shared.lock().complete);
        assert_eq!(transport.gets(), ["init", "1", "2"]);
    }

    #[test]
    fn retention_forgets_behind_and_downloads_again_on_a_seek_back() {
        let (cache, transport) = counting_cache(&[]);
        let mut stream = one_second_stream(cache.clone()).with_retention(seconds(Some(1), Some(2)));
        assert_eq!(texts(Iterator::take(&mut stream, 3)), ["init", "1", "2"]);

        // At segment 3, one second behind keeps 2 and lets 1 go.
        wait_for(|| cache.cached(&"3".into()).is_some());
        assert_eq!(cache.cached(&"1".into()), None);
        assert!(cache.cached(&"2".into()).is_some());
        assert!(cache.cached(&"init".into()).is_some());
        assert_eq!(cache.known_size(&"1".into()), Some(1));

        stream.seek_time(Duration::ZERO).unwrap();
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
