//! Async, lower-level access to downloaded DASH segments and bytes.

use super::{SegmentCache, Transport};
use crate::error::Error;
use crate::manifest::BehindWindow;
use crate::manifest::live::{LiveLink, LiveSource, Plan};
use crate::mp4::track::{self, Track};
use crate::segment::buffer::BufferHandle;
use crate::segment::clock::{Clock, SystemClock};
use crate::segment::index::MediaIndex;
#[cfg(any(doc, test))]
use crate::segment::schedule::TimeAccuracy;
use crate::segment::schedule::{
    self, Cursor, Part, PartBody, Position, ReadStep, Retention, Schedule, SizeStep, Supply,
    WindowChange,
};
use crate::segment::store::SegmentData;
use crate::segment::{SegmentNumber, Segments, refuse_live};
use bytes::Bytes;
use futures_util::Stream;
use std::fmt;
use std::future::{Future, poll_fn};
use std::io::{self, SeekFrom};
use std::ops::Range;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// A seek started through [`AsyncSeek::start_seek`], with the cursor and
/// position it lands on.
type SeekFuture = Pin<Box<dyn Future<Output = io::Result<(Cursor, Position)>> + Send>>;

/// Downloads a representation's segments and exposes them as whole segments or bytes.
///
/// This is a lower-level API. Use [`MediaStream`](crate::nonblocking::MediaStream) for
/// packets ready for a codec decoder. This type implements [`Stream`] for segment bodies
/// and [`AsyncRead`] plus [`AsyncSeek`] for their concatenated bytes. All views share one
/// position.
///
/// Downloads run on the Tokio runtime where the stream was created. By default all
/// segments are retained; [`Self::with_retention`] can bound memory and forward
/// downloading. Dropping the stream cancels its in-flight work.
///
/// After a download error, segment iteration reports the error once and then
/// ends, while byte reads continue to return it. [`Error::BehindLiveWindow`]
/// behaves the same way until a successful seek returns the stream to the
/// current live window.
///
/// **N.B.** `futures_util::StreamExt` and `tokio::io::AsyncReadExt` both
/// define `take` and `chain`. With both in scope, a method call such as
/// `stream.take(3)` is ambiguous and will not compile. Name the trait you
/// mean instead:
///
/// ```no_run
/// # async fn example(mut stream: playdash::nonblocking::segment::SegmentStream) {
/// use futures_util::StreamExt;
/// use tokio::io::AsyncReadExt;
///
/// let first_three = StreamExt::take(&mut stream, 3); // segments
/// # drop(first_three);
/// let first_kib = AsyncReadExt::take(stream, 1024); // bytes
/// # drop(first_kib);
/// # }
/// ```
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use futures_util::StreamExt;
/// use playdash::nonblocking::DashManifest;
/// use tokio::io::AsyncReadExt;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
///
/// // Whole segments.
/// let mut stream = manifest.segment_stream("audio").await?;
/// while let Some(segment) = stream.next().await {
///     let bytes = segment?;
///     # drop(bytes);
/// }
///
/// // Bytes, from 30 seconds in.
/// let mut stream = manifest.segment_stream("audio").await?;
/// stream.seek_time(Duration::from_secs(30)).await?;
/// let mut bytes = [0; 4096];
/// let count = stream.read(&mut bytes).await?;
/// # let _ = count;
/// # Ok(())
/// # }
/// ```
pub struct SegmentStream {
    shared: Arc<StreamShared>,
    cursor: Cursor,
    /// Estimated stream length. When set, [`SeekFrom::End`]`(0)` may report it
    /// instead of waiting for every segment size.
    length_estimate: Option<u64>,
    /// Whether segment iteration has reported a terminal error. A successful
    /// seek clears this flag.
    failed: bool,
    /// Whether the next returned segment follows an automatic live-window jump.
    jumped: bool,
    /// A seek started by [`AsyncSeek::start_seek`], until
    /// [`AsyncSeek::poll_complete`] finishes it.
    seeking: Option<SeekFuture>,
}

/// A stream's state, shared with its GET and HEAD tasks.
struct StreamShared {
    schedule: Mutex<Schedule>,
    /// Tasks waiting on `schedule`, woken after every change the download
    /// tasks make. Added to while `schedule` is locked, so no change is
    /// missed between checking and waiting.
    waiters: Mutex<Vec<Waker>>,
    /// Wakes the GET task when there may be something new to fetch.
    work: Notify,
    cancelled: AtomicBool,
    cache: Arc<SegmentCache>,
    /// The time the schedule adds segments at.
    clock: Arc<dyn Clock>,
    /// The live presentation whose refreshes keep the schedule's index
    /// current; `None` when static.
    live: Option<LiveLink>,
    /// The runtime the stream was created on, which runs its tasks.
    runtime: Handle,
    /// The GET and HEAD tasks, aborted when the stream is dropped.
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl SegmentStream {
    /// Creates a stream of `segments` fetching through a new
    /// [`reqwest::Client`]. The initialization segment is fetched before
    /// this returns. Must run on a tokio runtime.
    ///
    /// Call [`Self::start_eager_head`] to discover unknown segment sizes in
    /// the background for faster byte seeking.
    pub async fn new(segments: Segments) -> Result<Self, Error> {
        Self::new_with_cache(segments, Arc::default()).await
    }

    /// Like [`Self::new`], but fetches through `transport` with a new
    /// [`SegmentCache`] dedicated to this stream.
    pub async fn new_with_transport(
        segments: Segments,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, Error> {
        let cache = SegmentCache::new(transport);
        Self::new_with_cache(segments, Arc::new(cache)).await
    }

    /// Like [`Self::new`], but reuses data from `cache`.
    pub async fn new_with_cache(
        segments: Segments,
        cache: Arc<SegmentCache>,
    ) -> Result<Self, Error> {
        Self::new_retained(segments, cache, Retention::default()).await
    }

    /// Like [`Self::new_with_cache`], but keeps as much as `retention` says
    /// from the start, as [`Self::with_retention`] would after.
    pub(crate) async fn new_retained(
        segments: Segments,
        cache: Arc<SegmentCache>,
        retention: Retention,
    ) -> Result<Self, Error> {
        refuse_live(&segments)?;
        let first = segments.index().first();
        Self::open(&segments, first, cache, retention, None).await
    }

    /// Opens `source` and follows its live manifest refreshes, using `cache`
    /// and `retention` as [`Self::new_retained`] does.
    pub(crate) async fn new_live(
        source: LiveSource,
        cache: Arc<SegmentCache>,
        retention: Retention,
    ) -> Result<Self, Error> {
        let live = Some((source.link, source.delay));
        Self::open(&source.segments, source.first, cache, retention, live).await
    }

    /// Creates a stream that reads the initialization segment of
    /// `segments`, then its media from the segment covering `time`, through
    /// `cache`. Returns the stream and the start of that segment, which is at
    /// or before `time`. The initialization segment is fetched before this
    /// returns. Must run on a tokio runtime.
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
    /// [`DashManifest::segment_stream_at`](crate::nonblocking::DashManifest::segment_stream_at).
    pub async fn starting_at(
        segments: Segments,
        time: Duration,
        cache: Arc<SegmentCache>,
    ) -> Result<(Self, Duration), Error> {
        refuse_live(&segments)?;
        let (first, start) = schedule::start_at(segments.index(), time)?;
        let stream = Self::open(&segments, first, cache, Retention::default(), None).await?;
        Ok((stream, start))
    }

    /// Opens `segments` at `first`, backed by `cache` and bounded by
    /// `retention`. `live` supplies refresh state and the target live delay.
    async fn open(
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
        cache.get(&init).await?;
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
            waiters: Mutex::default(),
            work: Notify::new(),
            cancelled: AtomicBool::new(false),
            cache,
            clock,
            live,
            runtime: Handle::current(),
            tasks: Mutex::default(),
        });
        shared.spawn(fetch_task(shared.clone()));

        Ok(Self {
            shared,
            cursor: Cursor::start(first),
            length_estimate: None,
            failed: false,
            jumped: false,
            seeking: None,
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

    /// Fetches the body of media segment `index` of the representation, without
    /// moving the stream's position or retention window.
    ///
    /// `index` counts the representation's media segments, as
    /// [`Segments::segment_at`] does, also for a stream from
    /// [`Self::starting_at`]. A segment outside the retention window is not
    /// kept once returned, unless another stream sharing the cache keeps it.
    ///
    /// Fails with [`Error::Initialization`] if the segment is past the end or
    /// has expired from a live window. Download failures are returned without
    /// ending normal stream iteration.
    pub async fn fetch_segment(&self, index: usize) -> Result<Bytes, Error> {
        Ok(self
            .media_part_body(SegmentNumber(index as u64))
            .await?
            .body)
    }

    /// The tracks of the representation's initialization segment, as a
    /// [`PacketStream`](crate::nonblocking::packet::PacketStream) reads them.
    ///
    /// Fails with [`Error::MalformedMedia`] or [`Error::UnsupportedMedia`] if the
    /// initialization segment is not MP4 this crate reads.
    pub fn tracks(&self) -> Result<Vec<Track>, Error> {
        let init = self.shared.lock().init.clone();
        let body = init.body().ok_or_else(|| {
            Error::Initialization(
                "MPEG stream no longer holds its initialization segment".to_owned(),
            )
        })?;
        track::tracks(&body, &init.location)
    }

    /// [`Self::fetch_segment`], with where the segment sits.
    pub(crate) async fn media_part_body(&self, number: SegmentNumber) -> Result<PartBody, Error> {
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

        // Fetch in this task; the stream's own downloads carry on meanwhile.
        let body = self.shared.cache.get(&data).await?;
        let kept = self.shared.lock().fetched(part);
        if !kept {
            self.shared.cache.store.forget_if_unheld(&data);
        }
        self.shared.changed();
        Ok(PartBody {
            part,
            number: Some(number),
            span,
            location: data.location.clone(),
            body,
            jumped: false,
        })
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
            self.shared.spawn(probe_task(self.shared.clone()));
        }
    }

    /// Stops background HEAD prefetch.
    ///
    /// A request already in progress may still complete. Byte seeks can still
    /// request sizes when needed.
    pub fn stop_eager_head(&self) {
        self.shared.lock().eager_head = false;
    }

    /// Allows a demuxer to query an estimated stream length with
    /// [`SeekFrom::End`]`(0)` before every segment size is known.
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
    /// Otherwise downloads bodies until the offset is reachable.
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
    pub async fn seek_bytes(&mut self, from: SeekFrom) -> io::Result<Position> {
        let seek = seek_bytes(self.shared.clone(), self.cursor, self.length_estimate, from);
        let (cursor, position) = seek.await?;
        self.landed(cursor);
        Ok(position)
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
    pub async fn seek_time(&mut self, time: Duration) -> io::Result<Position> {
        let (cursor, position) = seek_time(self.shared.clone(), self.cursor, time).await?;
        self.landed(cursor);
        Ok(position)
    }

    /// Applies a completed seek and clears prior errors or jump markers.
    fn landed(&mut self, cursor: Cursor) {
        self.cursor = cursor;
        self.failed = false;
        self.jumped = false;
    }

    /// Moves the reader to the segment selected by a behind-window policy.
    fn jump(&mut self, to: Part, byte: Option<u64>, change: WindowChange) {
        self.shared.window_moved(change);
        self.cursor.place(to, 0, byte);
        self.jumped = true;
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

    /// Runs `task` on the stream's runtime, to be aborted with the stream.
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|error| error.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(self.runtime.spawn(task));
    }

    /// Wakes every task waiting on the schedule.
    fn changed(&self) {
        let waiters = std::mem::take(
            &mut *self
                .waiters
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// Applies cache ownership changes, evicts unused expired entries, and
    /// wakes the download task.
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
        self.work.notify_one();
    }

    /// Publishes `cursor`'s current part to the tasks and moves the retained
    /// window with it.
    ///
    /// If `cursor` is at the start of a part whose absolute byte offset is now
    /// known, records that offset for later byte-position queries.
    fn publish_cursor(&self, cursor: &mut Cursor) {
        let (change, start) = {
            let mut schedule = self.lock();
            let change = schedule.move_cursor(cursor.part, self.supply());
            (change, schedule.prefix_len(cursor.part))
        };
        cursor.learn_byte_position(start);
        self.window_moved(change);
    }

    /// Asks the GET task for the segment at `part` before anything else, if
    /// it is missing.
    fn request(&self, part: Part) {
        let mut schedule = self.lock();
        if schedule.missing_body(part) {
            schedule.request(part);
            drop(schedule);
            self.work.notify_one();
        }
    }

    /// The schedule once `ready` holds, or the error that ended the stream.
    /// Registers `cx` to be woken otherwise.
    fn poll_until(
        &self,
        cx: &mut Context<'_>,
        ready: impl Fn(&Schedule) -> bool,
    ) -> Poll<io::Result<MutexGuard<'_, Schedule>>> {
        let schedule = self.lock();
        if let Err(error) = self.check(&schedule) {
            return Poll::Ready(Err(error));
        }
        if ready(&schedule) {
            return Poll::Ready(Ok(schedule));
        }
        self.wait(cx);
        Poll::Pending
    }

    /// Polls `step` while holding the schedule lock, registering `cx` if no
    /// value is ready.
    fn poll_for<T>(
        &self,
        cx: &mut Context<'_>,
        step: impl FnOnce(&mut Schedule) -> Option<T>,
    ) -> Poll<io::Result<T>> {
        let mut schedule = self.lock();
        if let Err(error) = self.check(&schedule) {
            return Poll::Ready(Err(error));
        }
        if let Some(value) = step(&mut schedule) {
            return Poll::Ready(Ok(value));
        }
        self.wait(cx);
        Poll::Pending
    }

    /// Returns the current cancellation or download error, if any.
    fn check(&self, schedule: &Schedule) -> io::Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
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

    /// Registers `cx` for the next schedule change. The caller must hold the
    /// schedule lock so a change cannot be missed.
    fn wait(&self, cx: &mut Context<'_>) {
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !waiters.iter().any(|waiter| waiter.will_wake(cx.waker())) {
            waiters.push(cx.waker().clone());
        }
    }

    /// Prioritizes `part` and polls for the reader's next action.
    fn poll_read_step(&self, cx: &mut Context<'_>, part: Part) -> Poll<io::Result<ReadStep>> {
        self.request(part);
        self.poll_for(cx, |schedule| schedule.read_step(part, self.supply()))
    }

    /// Waits until `ready` holds, then reads what `then` takes from the
    /// schedule.
    async fn wait_until<T>(
        &self,
        ready: impl Fn(&Schedule) -> bool,
        then: impl Fn(&Schedule) -> T,
    ) -> io::Result<T> {
        poll_fn(|cx| {
            self.poll_until(cx, &ready)
                .map_ok(|schedule| then(&schedule))
        })
        .await
    }

    /// HEADs `data`, the segment at `part`, unless its size is known, or
    /// disables HEAD probing if the request fails.
    async fn probe(&self, part: Part, data: &SegmentData) {
        if let Err(error) = self.cache.probe(data).await {
            self.lock().probe_failed(part, &error);
        }
        self.changed();
    }
}

/// Downloads scheduled segments until the stream is dropped or a request fails.
async fn fetch_task(shared: Arc<StreamShared>) {
    while let Some((part, data)) = next_get(&shared).await {
        // Release data fetched only to discover its size once the request completes.
        let result = shared.cache.get(&data).await.map(drop);
        let kept = shared
            .lock()
            .finish_get(part, result, shared.clock.now());
        if !kept {
            shared.cache.store.forget_if_unheld(&data);
        }
        shared.changed();
    }
}

/// Waits for and returns the next scheduled GET.
///
/// For live streams this also adopts refreshed indexes, refreshes the MPD when
/// due, and advances availability. Returns `None` after cancellation or failure.
async fn next_get(shared: &StreamShared) -> Option<(Part, Arc<SegmentData>)> {
    loop {
        let plan = {
            let mut schedule = shared.lock();
            if shared.cancelled.load(Ordering::SeqCst) || schedule.error.is_some() {
                return None;
            }

            // Adopt an index refreshed by any stream sharing this presentation.
            let refreshed = shared.live.as_ref().map(LiveLink::refreshed);
            let change = match refreshed {
                Some(Ok(Some(index))) => schedule.replace_index(index, shared.supply()),
                Some(Err(reason)) => {
                    schedule.error = Some(reason);
                    drop(schedule);
                    shared.changed();
                    return None;
                }
                Some(Ok(None)) | None => WindowChange::default(),
            };
            if !change.is_empty() {
                drop(schedule);
                shared.window_moved(change);
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
                continue;
            }
            match &shared.live {
                Some(live) => live.plan(idle, shared.clock.now()),
                None => Plan::Wait(None),
            }
        };

        // Refresh, finish, or wait for a reader, the next segment or the next refresh.
        shared.changed();
        match plan {
            Plan::Refresh { ran_out } => {
                let live = shared.live.as_ref()?;
                let transport = &*shared.cache.transport;
                if let Err(error) = live.live.refresh(transport, live.seen(), ran_out).await {
                    shared.lock().error = Some(error.to_string());
                    shared.changed();
                    return None;
                }
            }
            Plan::Finish => {
                shared.lock().finish_index();
                shared.changed();
                shared.work.notified().await;
            }
            Plan::Wait(None) => shared.work.notified().await,
            Plan::Wait(Some(until)) => {
                let timeout = until.duration_since(shared.clock.now()).unwrap_or_default();
                // Either the time comes or something wakes the task; both look again.
                let _ = tokio::time::timeout(timeout, shared.work.notified()).await;
            }
        }
    }
}

/// Learns unknown segment sizes with HEAD while eager probing is wanted.
async fn probe_task(shared: Arc<StreamShared>) {
    loop {
        if shared.cancelled.load(Ordering::SeqCst) {
            return;
        }
        // Exit and spawn decisions share the lock so start/stop cannot leave
        // zero or two tasks.
        let next = shared.lock().next_eager_head(shared.supply());
        let Some((part, data)) = next else {
            // Wake anyone waiting for this task to stop.
            shared.changed();
            return;
        };
        // After a failure, `next_head` is `None` and the task ends.
        shared.probe(part, &data).await;
    }
}

/// Fills unknown sizes until `known` holds, via on-demand HEAD or GET
/// fallback. Also returns once nothing is left to download.
async fn ensure_sizes(
    shared: &StreamShared,
    known: impl Fn(&Schedule) -> bool + Send + Sync,
) -> io::Result<()> {
    loop {
        let step = shared.lock().size_step(&known, shared.supply());
        match step {
            SizeStep::Known => return Ok(()),
            // A failed HEAD disables probing, so the next step waits instead.
            SizeStep::Probe(part, data) => shared.probe(part, &data).await,
            SizeStep::Wait => {
                shared.work.notify_one();
                return shared
                    .wait_until(|schedule| known(schedule) || schedule.complete, |_| ())
                    .await;
            }
        }
    }
}

/// [`SegmentStream::seek_bytes`] from `cursor`, on owned state so an
/// [`AsyncSeek`] can keep it while it runs.
async fn seek_bytes(
    shared: Arc<StreamShared>,
    mut cursor: Cursor,
    length_estimate: Option<u64>,
    from: SeekFrom,
) -> io::Result<(Cursor, Position)> {
    shared.lock().check_seek_from(from)?;

    // The end, as far as it is known or estimated, without retargeting the
    // GET task.
    if let SeekFrom::End(0) = from
        && let Some((end, position)) = shared.lock().reported_end(length_estimate)
    {
        cursor.place(end, 0, position.byte);
        return Ok((cursor, position));
    }

    // Resolve the target to a segment, learning sizes as needed.
    if let SeekFrom::End(_) = from {
        ensure_sizes(&shared, Schedule::all_sizes_known).await?;
    }
    let total = shared.lock().total_len();
    let target = schedule::seek_target(from, cursor.byte_position, total)?;
    ensure_sizes(&shared, move |schedule| {
        schedule.resolve_cursor(target).is_some()
    })
    .await?;
    let (part, offset_in_segment, position, change) =
        shared.lock().land_byte_seek(target, shared.supply())?;
    shared.window_moved(change);
    cursor.place(part, offset_in_segment, Some(target));
    Ok((cursor, position))
}

/// [`SegmentStream::seek_time`] from `cursor`, on owned state.
async fn seek_time(
    shared: Arc<StreamShared>,
    mut cursor: Cursor,
    time: Duration,
) -> io::Result<(Cursor, Position)> {
    let (target, change) = shared.lock().begin_time_seek(time, shared.supply())?;
    shared.window_moved(change);
    let step = poll_fn(|cx| {
        shared.poll_for(cx, |schedule| schedule.read_step(target, shared.supply()))
    })
    .await?;
    let (part, position, change) = shared.lock().land_time_seek(step)?;
    shared.window_moved(change);
    cursor.place(part, 0, position.byte);
    Ok((cursor, position))
}

impl SegmentStream {
    /// The rest of the current segment, as [`Stream::poll_next`] yields it,
    /// with where it sits.
    pub(crate) fn poll_next_part(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<PartBody, Error>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        loop {
            // Resolve the current part, including any required live-window recovery.
            let part = self.cursor.part;
            let step = ready!(self.shared.poll_read_step(cx, part)).map_err(Error::from);
            let found = match step {
                Ok(ReadStep::Ready(found)) => found,
                Ok(ReadStep::End) => return Poll::Ready(None),
                Ok(ReadStep::Behind { available }) => {
                    self.failed = true;
                    return Poll::Ready(Some(Err(Error::BehindLiveWindow { available })));
                }
                Ok(ReadStep::Jumped { to, byte, change }) => {
                    self.jump(to, byte, change);
                    continue;
                }
                Err(error) => {
                    self.failed = true;
                    return Poll::Ready(Some(Err(error)));
                }
            };

            // Return the unread suffix and advance to the next part.
            let rest = self.cursor.take_rest(found.body.clone());
            self.shared.publish_cursor(&mut self.cursor);
            if let Some(body) = rest {
                let jumped = std::mem::take(&mut self.jumped);
                return Poll::Ready(Some(Ok(PartBody {
                    body,
                    jumped,
                    ..found
                })));
            }
        }
    }

    /// Whether nothing has been read yet: the position is the start of the
    /// initialization segment, with no seek under way.
    pub(crate) fn at_start(&self) -> bool {
        self.cursor.part == Part::Init
            && self.cursor.offset_in_segment == 0
            && self.seeking.is_none()
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

    /// Wakes the test task so it rechecks the manual clock.
    #[cfg(test)]
    pub(crate) fn wake_worker(&self) {
        self.shared.work.notify_one();
    }
}

impl Stream for SegmentStream {
    type Item = Result<Bytes, Error>;

    /// The rest of the current segment: a whole segment, unless reading or a
    /// byte seek left the position inside one.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut()
            .poll_next_part(cx)
            .map(|part| part.map(|part| part.map(|part| part.body)))
    }
}

impl AsyncRead for SegmentStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            // Resolve the current part, including any required live-window recovery.
            let part = this.cursor.part;
            let body = match ready!(this.shared.poll_read_step(cx, part))? {
                ReadStep::Ready(found) => found.body,
                ReadStep::End => {
                    return this
                        .shared
                        .poll_until(cx, |schedule| schedule.complete)
                        .map_ok(drop);
                }
                ReadStep::Behind { available } => {
                    let error = Error::BehindLiveWindow { available };
                    return Poll::Ready(Err(io::Error::other(error)));
                }
                ReadStep::Jumped { to, byte, change } => {
                    this.jump(to, byte, change);
                    continue;
                }
            };
            this.jumped = false;

            // Copy available bytes and publish a cursor move at segment boundaries.
            let copied = this.cursor.read(&body, buf.initialize_unfilled());
            buf.advance(copied);
            if this.cursor.part != part {
                this.shared.publish_cursor(&mut this.cursor);
            }
            if copied > 0 {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncSeek for SegmentStream {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        if this.seeking.is_some() {
            return Err(io::Error::other(
                "start_seek called before the previous seek completed",
            ));
        }
        this.seeking = Some(Box::pin(seek_bytes(
            this.shared.clone(),
            this.cursor,
            this.length_estimate,
            position,
        )));
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let unknown = || io::Error::other("MPEG stream byte position is unknown");
        let Some(seeking) = this.seeking.as_mut() else {
            return Poll::Ready(this.cursor.byte_position.ok_or_else(unknown));
        };
        let result = ready!(seeking.as_mut().poll(cx));
        this.seeking = None;
        let (cursor, position) = result?;
        this.landed(cursor);
        Poll::Ready(position.byte.ok_or_else(unknown))
    }
}

impl Drop for SegmentStream {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        let tasks = std::mem::take(
            &mut *self
                .shared
                .tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for task in tasks {
            task.abort();
        }
        self.shared.changed();
        // Let go of what this stream kept without dropping it, so a stream
        // rebuilt on the same cache, as after a seek, still finds it.
        let kept = self.shared.lock().kept();
        self.shared.cache.store.release(&kept, false);
    }
}

impl fmt::Debug for SegmentStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentStream")
            .field("cursor", &self.cursor)
            .field("cache", &self.shared.cache)
            .field("seeking", &self.seeking.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nonblocking::tests::{Counts, named, serve, texts};
    use crate::segment::{SegmentLocation, timed_for_test};
    use futures_util::StreamExt;
    use reqwest::Client;
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    /// The init and five one-second media segments named "1" to "5" at
    /// `base`, whose bodies are their names.
    async fn one_second_stream(base: &str, cache: Arc<SegmentCache>) -> SegmentStream {
        let segments = timed_for_test(
            format!("{base}/init"),
            (1..=5).map(|name| format!("{base}/{name}")),
            1,
            &[1; 5],
        );
        SegmentStream::new_with_cache(segments, cache)
            .await
            .unwrap()
    }

    fn client_cache() -> Arc<SegmentCache> {
        Arc::new(SegmentCache::new(Arc::new(Client::new())))
    }

    fn requests(counts: &Counts, path: &str) -> usize {
        counts.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn seconds(behind: Option<u64>, ahead: Option<u64>) -> Retention {
        Retention {
            behind: behind.map(Duration::from_secs),
            ahead: ahead.map(Duration::from_secs),
        }
    }

    #[tokio::test]
    async fn fetch_segment_does_not_move_the_stream() {
        // The first request for segment 3 fails.
        let refused = Arc::new(AtomicBool::new(false));
        let (base, counts) = serve({
            let refused = refused.clone();
            move |path, _| match path {
                "/3" if !refused.swap(true, Ordering::SeqCst) => (500, Vec::new()),
                path => (200, path.trim_start_matches('/').as_bytes().to_vec()),
            }
        });
        let stream = one_second_stream(&base, client_cache()).await;
        let mut stream = stream.with_retention(seconds(Some(0), Some(1)));

        // A side-fetch of the last segment does not move the stream cursor.
        assert_eq!(stream.fetch_segment(4).await.unwrap(), &b"5"[..]);
        assert!(matches!(
            stream.fetch_segment(5).await,
            Err(Error::Initialization(_))
        ));
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"init"[..]);
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"1"[..]);

        // Failure of a side-fetch does not prevent the stream's own later download.
        assert!(stream.fetch_segment(2).await.is_err());
        assert_eq!(texts(&mut stream).await, ["2", "3", "4", "5"]);
        assert_eq!(requests(&counts, "/3"), 2);
        // The earlier side-fetch was outside retention, so the stream downloads it again.
        assert_eq!(requests(&counts, "/5"), 2);
    }

    #[tokio::test]
    async fn streams_whole_segments_in_order() {
        let (base, _) = serve(named());
        let mut stream = one_second_stream(&base, client_cache()).await;
        assert_eq!(texts(&mut stream).await, ["init", "1", "2", "3", "4", "5"]);
    }

    #[tokio::test]
    async fn reading_streaming_and_seeking_share_one_position() {
        let (base, _) = serve(named());
        let mut stream = one_second_stream(&base, client_cache()).await;
        let mut head = [0; 5];
        stream.read_exact(&mut head).await.unwrap();
        assert_eq!(&head, b"init1");

        // Segment 1 was read whole, so the next item is segment 2.
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"2"[..]);
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"345");

        // A byte seek through `AsyncSeek` moves streaming too.
        assert_eq!(stream.seek(SeekFrom::Start(4)).await.unwrap(), 4);
        assert_eq!(texts(&mut stream).await, ["1", "2", "3", "4", "5"]);
    }

    #[tokio::test]
    async fn seeks_by_time_to_a_segment_start() {
        let (base, _) = serve(named());
        let mut stream = one_second_stream(&base, client_cache()).await;
        let position = stream.seek_time(Duration::from_secs(2)).await.unwrap();
        assert_eq!(
            position.time.map(|time| (time.timestamp, time.accuracy)),
            Some((Duration::from_secs(2), TimeAccuracy::Packet))
        );
        assert_eq!(texts(&mut stream).await, ["3", "4", "5"]);
    }

    #[tokio::test]
    async fn sends_ranges_and_slices_ignored_ones() {
        const FILE: &[u8] = b"INITaaaaBBBBBBcc";
        let (base, _) = serve(|path, range| match (path, range) {
            ("/ranged", Some(range)) => (
                206,
                FILE[*range.start() as usize..=*range.end() as usize].to_vec(),
            ),
            ("/short", Some(_)) => (206, b"x".to_vec()),
            _ => (200, FILE.to_vec()),
        });
        let ranged =
            |path: &str, range| SegmentLocation::with_range(format!("{base}{path}"), range);
        let segments = Segments::new(
            ranged("/ranged", 0..=3),
            [ranged("/plain", 8..=13), ranged("/ranged", 14..=15)],
        );
        let mut stream = SegmentStream::new(segments).await.unwrap();
        assert_eq!(texts(&mut stream).await, ["INIT", "BBBBBB", "cc"]);

        let short = Segments::new(ranged("/short", 0..=3), Vec::<SegmentLocation>::new());
        let short = SegmentStream::new(short).await;
        assert!(matches!(
            short,
            Err(Error::SegmentLength {
                expected: 4,
                actual: 1,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn ends_after_a_download_failure() {
        let (base, _) = serve(|path, range| match path {
            "/3" => (404, Vec::new()),
            _ => named()(path, range),
        });
        let stream = one_second_stream(&base, client_cache()).await;
        let items: Vec<_> = stream.collect().await;
        let (last, before) = items.split_last().unwrap();
        assert!(matches!(last, Err(Error::Io(error)) if error.to_string().contains("404")));
        assert!(before.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn retention_downloads_only_its_window_ahead() {
        let released = Arc::new(AtomicBool::new(false));
        let gate = released.clone();
        let (base, counts) = serve(move |path, range| {
            if path != "/init" {
                while !gate.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            named()(path, range)
        });
        let stream = one_second_stream(&base, client_cache())
            .await
            .with_retention(seconds(None, Some(2)));
        released.store(true, Ordering::SeqCst);

        // Two seconds ahead of the start is segments 1 and 2, then it waits.
        stream
            .shared
            .wait_until(|schedule| schedule.complete, |_| ())
            .await
            .unwrap();
        assert_eq!(requests(&counts, "/1"), 1);
        assert_eq!(requests(&counts, "/2"), 1);
        assert_eq!(requests(&counts, "/3"), 0);
    }

    #[tokio::test]
    async fn retention_forgets_behind_and_downloads_again_on_a_seek_back() {
        let (base, counts) = serve(named());
        let cache = client_cache();
        let mut stream = one_second_stream(&base, cache.clone())
            .await
            .with_retention(seconds(Some(1), Some(2)));
        let first: Vec<_> = StreamExt::take(&mut stream, 3)
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(first, [&b"init"[..], b"1", b"2"]);

        // At segment 3, one second behind keeps 2 and lets 1 go.
        let one = SegmentLocation::new(format!("{base}/1"));
        assert_eq!(cache.cached(&one), None);
        assert!(
            cache
                .cached(&SegmentLocation::new(format!("{base}/2")))
                .is_some()
        );
        assert_eq!(cache.known_size(&one), Some(1));

        stream.seek_time(Duration::ZERO).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"1"[..]);
        assert_eq!(requests(&counts, "/1"), 2);
    }

    /// [`one_second_stream`], keeping what `retention` says from the start.
    async fn retained_stream(
        base: &str,
        cache: Arc<SegmentCache>,
        retention: Retention,
    ) -> SegmentStream {
        let segments = timed_for_test(
            format!("{base}/init"),
            (1..=5).map(|name| format!("{base}/{name}")),
            1,
            &[1; 5],
        );
        SegmentStream::new_retained(segments, cache, retention)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn retention_at_open_schedules_only_its_window() {
        let (base, _) = serve(named());
        let cache = client_cache();
        let mut stream = retained_stream(&base, cache.clone(), seconds(Some(0), Some(1))).await;
        assert_eq!(cache.store.len(), 2, "the init and the first segment");
        assert_eq!(texts(&mut stream).await, ["init", "1", "2", "3", "4", "5"]);
        assert_eq!(cache.store.len(), 6);
    }

    #[tokio::test]
    async fn byte_seeks_and_fetches_reach_past_a_bounded_window() {
        let (base, _) = serve(named());
        let mut stream = retained_stream(&base, client_cache(), seconds(Some(0), Some(0))).await;

        // The end needs every size, which HEAD learns.
        assert_eq!(stream.seek(SeekFrom::End(0)).await.unwrap(), 9);
        assert_eq!(stream.seek(SeekFrom::Start(7)).await.unwrap(), 7);
        assert_eq!(texts(&mut stream).await, ["4", "5"]);

        // A side fetch adds the segments it needs.
        let fresh = retained_stream(&base, client_cache(), seconds(Some(0), Some(0))).await;
        assert_eq!(fresh.fetch_segment(4).await.unwrap(), &b"5"[..]);
    }
}
