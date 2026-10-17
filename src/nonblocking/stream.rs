//! A representation's fragments, downloaded by tokio tasks and read as a
//! `Stream` of whole fragments or as `AsyncRead` and `AsyncSeek` bytes.

use super::{FragmentCache, Transport};
use crate::buffer::BufferHandle;
use crate::error::Error;
use crate::fragment::{Fragments, MediaTimeline};
use crate::mp4::track::{self, Track};
use crate::schedule::{
    self, Cursor, Part, PartBody, Position, Retention, Schedule, StreamTimeline, TimeAccuracy,
    WindowChange,
};
use crate::store::FragmentData;
use bytes::Bytes;
use futures_util::Stream;
use std::fmt;
use std::future::{Future, poll_fn};
use std::io::{self, SeekFrom};
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

/// A representation's fragments, downloaded in the background and read in
/// order: a [`Stream`] of whole fragments, and a progressive [`AsyncRead`] and
/// [`AsyncSeek`] view of the same bytes. The async counterpart of the `sync`
/// feature's `playdash::FragmentStream`.
///
/// The initialization segment comes first, then the media segments. Streaming
/// and reading share one position: each item is everything from the current
/// position to the end of its fragment, which is a whole fragment unless
/// reading or [`Self::seek_bytes`] left the position inside one. Use
/// [`Self::seek_bytes`] for byte offsets or [`Self::seek_time`] to seek
/// to the start of the fragment containing a timestamp. Both streaming and
/// reading wait while the fragment they need downloads.
///
/// It downloads on the tokio runtime it was created on, one fragment at a
/// time, and dropping it cancels the download in flight. By default it keeps
/// the whole representation; [`Self::with_retention`] bounds how much it keeps
/// behind its position and how far ahead it downloads, as long video needs.
///
/// After a download fails, the stream yields the error once and then ends,
/// and reading keeps returning it.
///
/// **N.B.** `futures_util::StreamExt` and `tokio::io::AsyncReadExt` both
/// define `take` and `chain`. With both in scope, a method call such as
/// `stream.take(3)` is ambiguous and will not compile. Name the trait you
/// mean instead:
///
/// ```no_run
/// # async fn example(mut stream: playdash::nonblocking::FragmentStream) {
/// use futures_util::StreamExt;
/// use tokio::io::AsyncReadExt;
///
/// let first_three = StreamExt::take(&mut stream, 3); // fragments
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
/// // Whole fragments.
/// let mut stream = manifest.stream("audio").await?;
/// while let Some(fragment) = stream.next().await {
///     let bytes = fragment?;
///     # drop(bytes);
/// }
///
/// // Bytes, from 30 seconds in.
/// let mut stream = manifest.stream("audio").await?;
/// stream.seek_time(Duration::from_secs(30)).await?;
/// let mut bytes = [0; 4096];
/// let count = stream.read(&mut bytes).await?;
/// # let _ = count;
/// # Ok(())
/// # }
/// ```
pub struct FragmentStream {
    shared: Arc<StreamShared>,
    cursor: Cursor,
    timeline: Option<Arc<StreamTimeline>>,
    /// Estimated stream length. When set, [`SeekFrom::End`]`(0)` may report it
    /// instead of waiting for every fragment size.
    length_estimate: Option<u64>,
    /// Set once the stream has yielded an error, so it ends rather than
    /// yielding the same error forever.
    failed: bool,
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
    cache: Arc<FragmentCache>,
    /// The runtime the stream was created on, which runs its tasks.
    runtime: Handle,
    /// The GET and HEAD tasks, aborted when the stream is dropped.
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl FragmentStream {
    /// Creates a stream of `fragments` fetching through a new
    /// [`reqwest::Client`]. The initialization fragment is fetched before
    /// this returns. Must run on a tokio runtime.
    ///
    /// `timeline`, when given, must have one duration per media fragment.
    /// Call [`Self::start_eager_head`] to discover unknown fragment sizes in
    /// the background for faster byte seeking.
    pub async fn new(fragments: Fragments, timeline: Option<MediaTimeline>) -> Result<Self, Error> {
        Self::new_with_cache(fragments, timeline, Arc::default()).await
    }

    /// Like [`Self::new`], but fetches through `transport` with a new
    /// [`FragmentCache`] dedicated to this stream.
    pub async fn new_with_transport(
        fragments: Fragments,
        timeline: Option<MediaTimeline>,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, Error> {
        let cache = FragmentCache::new(transport);
        Self::new_with_cache(fragments, timeline, Arc::new(cache)).await
    }

    /// Like [`Self::new`], but reuses data from `cache`.
    pub async fn new_with_cache(
        fragments: Fragments,
        timeline: Option<MediaTimeline>,
        cache: Arc<FragmentCache>,
    ) -> Result<Self, Error> {
        let timeline = timeline.map(|timeline| StreamTimeline::new(timeline, 0));
        Self::open(fragments, timeline, cache).await
    }

    /// Creates a stream that reads the initialization fragment of
    /// `fragments`, then its media from the fragment covering `time`, through
    /// `cache`. Returns the stream and the start of that fragment, which is at
    /// or before `time`. The initialization fragment is fetched before this
    /// returns. Must run on a tokio runtime.
    ///
    /// The stream reports and seeks by `timeline`'s media time, as a stream
    /// of every fragment would; seeking to a time before its start fails. A
    /// `time` at or past the end starts at the last media fragment.
    /// `timeline` must have one duration per media fragment.
    ///
    /// Fails with [`Error::Initialization`] if `timeline` has no media
    /// durations or a zero timescale, or there are fewer media fragments than
    /// the timeline puts before `time`.
    pub async fn starting_at(
        fragments: Fragments,
        timeline: MediaTimeline,
        time: Duration,
        cache: Arc<FragmentCache>,
    ) -> Result<(Self, Duration), Error> {
        let (fragments, timeline, start) = schedule::start_at(fragments, timeline, time)?;
        Ok((Self::open(fragments, Some(timeline), cache).await?, start))
    }

    /// A stream of `fragments`, timed by `timeline`, fetching through `cache`.
    async fn open(
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
        cache.get(&init).await?;
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
            waiters: Mutex::default(),
            work: Notify::new(),
            cancelled: AtomicBool::new(false),
            cache,
            runtime: Handle::current(),
            tasks: Mutex::default(),
        });
        shared.spawn(fetch_task(shared.clone()));

        Ok(Self {
            shared,
            cursor: Cursor::start(),
            timeline: timeline.map(Arc::new),

            length_estimate: None,
            failed: false,
            seeking: None,
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

    /// Fetches media fragment `index` of the representation, without moving the
    /// stream's position or retention window.
    ///
    /// `index` counts the representation's media fragments, as
    /// [`MediaTimeline::fragment_at`] does, also for a stream from
    /// [`Self::starting_at`]. A fragment outside the retention window is not
    /// kept once returned, unless another stream sharing the cache keeps it.
    ///
    /// Fails with [`Error::Initialization`] for a fragment before the stream's
    /// first or past its last, and with the download's error if it fails. A
    /// failed fetch does not end the stream.
    pub async fn media_fragment(&self, index: usize) -> Result<Bytes, Error> {
        Ok(self.media_part_body(index).await?.body)
    }

    /// The tracks of the representation's initialization fragment, as a
    /// [`PacketStream`](super::PacketStream) reads them.
    ///
    /// Fails with [`Error::MalformedMedia`] or [`Error::UnsupportedMedia`] if the
    /// initialization fragment is not MP4 this crate reads.
    pub fn tracks(&self) -> Result<Vec<Track>, Error> {
        let init = self.shared.lock().init.clone();
        let body = init.body().ok_or_else(|| {
            Error::Initialization(
                "MPEG stream no longer holds its initialization fragment".to_owned(),
            )
        })?;
        track::tracks(&body, &init.fragment)
    }

    /// [`Self::media_fragment`], with where the fragment sits.
    pub(crate) async fn media_part_body(&self, index: usize) -> Result<PartBody, Error> {
        let (part, data) = {
            let part = schedule::media_part(self.timeline.as_deref(), index);
            let schedule = self.shared.lock();
            part.and_then(|part| Some((part, schedule.fragment(part)?.clone())))
                .ok_or_else(|| {
                    Error::Initialization(format!(
                        "MPEG stream has no media fragment {index} of its representation"
                    ))
                })?
        };

        // Fetch in this task; the stream's own downloads carry on meanwhile.
        let body = self.shared.cache.get(&data).await?;
        let kept = {
            let mut schedule = self.shared.lock();
            schedule.check_probe(part);
            schedule.keeps(part)
        };
        if !kept {
            self.shared.cache.store.forget_if_unheld(&data);
        }
        self.shared.changed();
        Ok(PartBody {
            part,
            index: Some(index),
            fragment: data.fragment.clone(),
            body,
        })
    }

    /// A handle reporting how much of this stream's media its cache holds.
    /// `None` if the stream has no timeline.
    pub fn buffer(&self) -> Option<BufferHandle> {
        let spans = self.timeline.as_deref()?.spans();
        let media = self.shared.lock().media.clone();
        Some(BufferHandle::new(&self.shared.cache.store, media, spans))
    }

    /// Starts background HEAD prefetch of unknown fragment sizes.
    ///
    /// This is a no-op when all sizes are known or the server does not support
    /// usable `HEAD` responses.
    pub fn start_eager_head(&self) {
        let mut schedule = self.shared.lock();
        schedule.eager_head = true;
        if schedule.head_worker_running
            || schedule.error.is_some()
            || schedule.next_head().is_none()
            || self.shared.cancelled.load(Ordering::SeqCst)
        {
            // A running task sees `eager_head` on its next check and continues.
            return;
        }
        schedule.head_worker_running = true;
        drop(schedule);
        self.shared.spawn(probe_task(self.shared.clone()));
    }

    /// Stops background HEAD prefetch.
    ///
    /// A request already in progress may still complete. Byte seeks can still
    /// request sizes when needed.
    pub fn stop_eager_head(&self) {
        self.shared.lock().eager_head = false;
    }

    /// Allows a demuxer to query an estimated stream length with
    /// [`SeekFrom::End`]`(0)` before every fragment size is known.
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
        let estimate = schedule::compat_estimate(self.timeline.as_deref(), init_len, bitrate_bps)?;
        self.length_estimate = Some(estimate);
        Ok(())
    }

    /// Seeks to an exact offset in the concatenated initialization + media bytes.
    ///
    /// When HEAD is available, discovers lengths for skipped fragments without
    /// downloading them, then prioritizes a GET of the target fragment.
    /// Otherwise downloads bodies until the offset is reachable.
    /// [`SeekFrom::Current`] requires a known absolute byte cursor.
    ///
    /// The returned byte offset is exact, except [`SeekFrom::End`]`(0)` under
    /// [`Self::set_symphonia_compat`], which may report an oversized estimate.
    /// When a timeline is available, the returned time is [`TimeAccuracy::Packet`]
    /// at a fragment boundary, and otherwise the containing fragment's start as
    /// [`TimeAccuracy::Fragment`].
    pub async fn seek_bytes(&mut self, from: SeekFrom) -> io::Result<Position> {
        let seek = seek_bytes(
            self.shared.clone(),
            self.timeline.clone(),
            self.cursor,
            self.length_estimate,
            from,
        );
        let (cursor, position) = seek.await?;
        self.cursor = cursor;
        Ok(position)
    }

    /// Seeks to the start of the media fragment containing `time`, using the
    /// DASH `SegmentTimeline`.
    ///
    /// Time `0` is the start of the first media fragment, after the
    /// initialization segment. The seek lands at byte `0` of the fragment
    /// containing `time`, so an fMP4 consumer receives the fragment's `moof`
    /// rather than an arbitrary position inside `mdat`. The target fragment is
    /// prioritized and loaded before this returns.
    ///
    /// This is fragment-granular: playback may begin earlier than `time` by up
    /// to one media-fragment duration. The returned position holds the
    /// fragment's start, which is exact for the position
    /// ([`TimeAccuracy::Packet`]); compare it with `time` to see how far back
    /// the seek landed. The byte field is `None` until every earlier fragment
    /// size is known.
    pub async fn seek_time(&mut self, time: Duration) -> io::Result<Position> {
        let (cursor, position) =
            seek_time(self.shared.clone(), self.timeline.clone(), time).await?;
        self.cursor = cursor;
        Ok(position)
    }
}

impl StreamShared {
    fn lock(&self) -> MutexGuard<'_, Schedule> {
        self.schedule
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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

    /// Applies a window move to the cache, dropping bodies no stream keeps,
    /// and wakes the GET task for what entered the window.
    fn window_moved(&self, change: WindowChange) {
        self.cache.store.acquire(&change.acquired);
        self.cache.store.release(&change.released, true);
        self.work.notify_one();
    }

    /// Tells the tasks the cursor is now at `part`, which moves the window.
    fn publish_cursor(&self, part: Part) {
        let change = self.lock().move_cursor(part);
        self.window_moved(change);
    }

    /// Asks the GET task for the fragment at `part` before anything else, if
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
        if self.cancelled.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "MPEG stream cancelled",
            )));
        }
        if let Some(error) = &schedule.error {
            return Poll::Ready(Err(io::Error::other(error.clone())));
        }
        if ready(&schedule) {
            return Poll::Ready(Ok(schedule));
        }
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !waiters.iter().any(|waiter| waiter.will_wake(cx.waker())) {
            waiters.push(cx.waker().clone());
        }
        Poll::Pending
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

    /// HEADs `data`, the fragment at `part`, unless its size is known, or
    /// disables HEAD probing if the request fails.
    async fn probe(&self, part: Part, data: &FragmentData) {
        let result = self.cache.probe(data).await;
        if let Err(error) = result {
            self.lock()
                .disable_head(&format!("HEAD failed for {part}: {error}"));
        }
        self.changed();
    }
}

/// Downloads the stream's fragments one at a time, in the order its schedule
/// says, waiting whenever the window holds everything it should, until the
/// stream is dropped or a download fails.
async fn fetch_task(shared: Arc<StreamShared>) {
    loop {
        // Wait for something to fetch.
        let (part, data) = loop {
            let work = {
                let mut schedule = shared.lock();
                if shared.cancelled.load(Ordering::SeqCst) || schedule.error.is_some() {
                    return;
                }
                let work = schedule.next_get();
                if work.is_none() {
                    schedule.complete = true;
                }
                work
            };
            match work {
                Some(work) => break work,
                None => {
                    shared.changed();
                    shared.work.notified().await;
                }
            }
        };

        // Fetch it, and drop it again if it was only wanted for its size.
        let result = shared.cache.get(&data).await;
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
        shared.changed();
    }
}

/// Learns unknown fragment sizes with HEAD while eager probing is wanted.
async fn probe_task(shared: Arc<StreamShared>) {
    loop {
        if shared.cancelled.load(Ordering::SeqCst) {
            return;
        }
        // Exit and spawn decisions share the lock so start/stop cannot leave
        // zero or two tasks.
        let next = {
            let mut schedule = shared.lock();
            let next = match schedule.eager_head && schedule.error.is_none() {
                true => schedule.next_head(),
                false => None,
            };
            if next.is_none() {
                schedule.head_worker_running = false;
            }
            next
        };
        let Some((part, data)) = next else {
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
        let next_head = {
            let schedule = shared.lock();
            if known(&schedule) {
                return Ok(());
            }
            schedule.next_head()
        };
        // A failed HEAD disables probing, so `next_head` ends the loop.
        let Some((part, data)) = next_head else {
            break;
        };
        shared.probe(part, &data).await;
    }

    // Without HEAD, download the first fragment whose size is unknown, even
    // outside the window: its size is kept after its body is dropped.
    {
        let mut schedule = shared.lock();
        if known(&schedule) {
            return Ok(());
        }
        if let Some(part) = schedule.first_unknown_size() {
            schedule.request(part);
        }
    }
    shared.work.notify_one();
    shared
        .wait_until(|schedule| known(schedule) || schedule.complete, |_| ())
        .await
}

/// [`FragmentStream::seek_bytes`] from `cursor`, on owned state so an
/// [`AsyncSeek`] can keep it while it runs.
async fn seek_bytes(
    shared: Arc<StreamShared>,
    timeline: Option<Arc<StreamTimeline>>,
    mut cursor: Cursor,
    length_estimate: Option<u64>,
    from: SeekFrom,
) -> io::Result<(Cursor, Position)> {
    // The end, as far as it is known or estimated, without retargeting the
    // GET task.
    let reported_end = match from {
        SeekFrom::End(0) => shared.lock().reported_end_len(length_estimate),
        _ => None,
    };
    if let Some(len) = reported_end {
        let end = shared.lock().end();
        cursor.place(end, 0, Some(len));
        let time = schedule::time_at_end(timeline.as_deref());
        return Ok((
            cursor,
            Position {
                byte: Some(len),
                time,
            },
        ));
    }

    // Resolve the target to a fragment, learning sizes as needed.
    if let SeekFrom::End(_) = from {
        ensure_sizes(&shared, Schedule::all_sizes_known).await?;
    }
    let total = shared.lock().total_len();
    let target = schedule::seek_target(from, cursor.byte_position, total)?;
    ensure_sizes(&shared, move |schedule| {
        schedule.resolve_cursor(target).is_some()
    })
    .await?;
    let (part, offset_in_fragment) = shared
        .lock()
        .resolve_cursor(target)
        .ok_or_else(|| io::Error::other("MPEG stream ended before the requested byte offset"))?;

    cursor.place(part, offset_in_fragment, Some(target));
    shared.publish_cursor(part);
    shared.request(part);
    let time = schedule::time_at_fragment(
        timeline.as_deref(),
        part,
        schedule::accuracy_at(offset_in_fragment),
    );
    Ok((
        cursor,
        Position {
            byte: Some(target),
            time,
        },
    ))
}

/// [`FragmentStream::seek_time`], on owned state.
async fn seek_time(
    shared: Arc<StreamShared>,
    timeline: Option<Arc<StreamTimeline>>,
    time: Duration,
) -> io::Result<(Cursor, Position)> {
    let target = Part::Media(schedule::time_target(timeline.as_deref(), time)?);
    let change = {
        let mut schedule = shared.lock();
        if schedule.past_end(target) {
            return Err(io::Error::other(
                "MPEG stream ended before the requested media time",
            ));
        }
        let change = schedule.move_cursor(target);
        schedule.request(target);
        change
    };
    shared.window_moved(change);

    let (missing, byte_position) = shared
        .wait_until(
            |schedule| !schedule.missing_body(target) || schedule.complete,
            |schedule| (schedule.missing_body(target), schedule.prefix_len(target)),
        )
        .await?;
    if missing {
        return Err(io::Error::other(
            "MPEG stream ended before the requested media time",
        ));
    }

    let mut cursor = Cursor::start();
    cursor.place(target, 0, byte_position);
    let time = schedule::time_at_fragment(timeline.as_deref(), target, TimeAccuracy::Packet);
    Ok((
        cursor,
        Position {
            byte: byte_position,
            time,
        },
    ))
}

impl FragmentStream {
    /// The rest of the current fragment, as [`Stream::poll_next`] yields it,
    /// with where it sits.
    pub(crate) fn poll_next_part(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<PartBody, Error>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        loop {
            let part = self.cursor.part;
            if self.shared.lock().past_end(part) {
                return Poll::Ready(None);
            }

            // Wait for the fragment under the cursor.
            self.shared.request(part);
            let waited = self.shared.poll_until(cx, |schedule| {
                !schedule.missing_body(part) || schedule.complete
            });
            let found = match ready!(waited) {
                Ok(mut schedule) => schedule
                    .checked_body(part)
                    .and_then(|body| Some((body, schedule.fragment(part)?.fragment.clone()))),
                Err(error) => {
                    self.failed = true;
                    return Poll::Ready(Some(Err(error.into())));
                }
            };
            let Some((body, fragment)) = found else {
                return Poll::Ready(None);
            };

            // Hand over what is left of it and move on to the next.
            let rest = self.cursor.take_rest(body);
            self.shared.publish_cursor(self.cursor.part);
            if let Some(body) = rest {
                return Poll::Ready(Some(Ok(PartBody {
                    part,
                    index: schedule::representation_index(self.timeline.as_deref(), part),
                    fragment,
                    body,
                })));
            }
        }
    }

    /// Whether nothing has been read yet: the position is the start of the
    /// initialization fragment, with no seek under way.
    pub(crate) fn at_start(&self) -> bool {
        self.cursor.part == Part::Init
            && self.cursor.offset_in_fragment == 0
            && self.seeking.is_none()
    }

    /// The representation's timeline, if the stream has one.
    pub(crate) fn representation_timeline(&self) -> Option<&MediaTimeline> {
        self.timeline.as_deref().map(StreamTimeline::timeline)
    }

    /// Index in the representation of the stream's first media fragment.
    pub(crate) fn first_index(&self) -> usize {
        self.timeline.as_deref().map_or(0, StreamTimeline::first)
    }
}

impl Stream for FragmentStream {
    type Item = Result<Bytes, Error>;

    /// The rest of the current fragment: a whole fragment, unless reading or a
    /// byte seek left the position inside one.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut()
            .poll_next_part(cx)
            .map(|part| part.map(|part| part.map(|part| part.body)))
    }
}

impl AsyncRead for FragmentStream {
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
            let part = this.cursor.part;
            if this.shared.lock().past_end(part) {
                return this
                    .shared
                    .poll_until(cx, |schedule| schedule.complete)
                    .map_ok(drop);
            }

            // Wait for the fragment under the cursor.
            this.shared.request(part);
            let waited = this.shared.poll_until(cx, |schedule| {
                !schedule.missing_body(part) || schedule.complete
            });
            let Some(body) = ready!(waited)?.checked_body(part) else {
                return Poll::Ready(Ok(()));
            };

            // Copy from it, moving on to the next fragment once it is used up.
            let copied = this.cursor.read(&body, buf.initialize_unfilled());
            buf.advance(copied);
            if this.cursor.part != part {
                this.shared.publish_cursor(this.cursor.part);
            }
            if copied > 0 {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncSeek for FragmentStream {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        if this.seeking.is_some() {
            return Err(io::Error::other(
                "start_seek called before the previous seek completed",
            ));
        }
        this.seeking = Some(Box::pin(seek_bytes(
            this.shared.clone(),
            this.timeline.clone(),
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
        this.cursor = cursor;
        Poll::Ready(position.byte.ok_or_else(unknown))
    }
}

impl Drop for FragmentStream {
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

impl fmt::Debug for FragmentStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FragmentStream")
            .field("cursor", &self.cursor)
            .field("cache", &self.shared.cache)
            .field("seeking", &self.seeking.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::Fragment;
    use crate::nonblocking::tests::{Counts, named, serve, texts};
    use futures_util::StreamExt;
    use reqwest::Client;
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    /// The init and five one-second media fragments named "1" to "5" at
    /// `base`, whose bodies are their names.
    async fn one_second_stream(base: &str, cache: Arc<FragmentCache>) -> FragmentStream {
        let fragments = Fragments::new(
            format!("{base}/init"),
            (1..=5).map(|name| format!("{base}/{name}")),
        );
        let timeline = MediaTimeline {
            timescale: 1,
            origin: 0,
            media_durations: vec![1; 5],
        };
        FragmentStream::new_with_cache(fragments, Some(timeline), cache)
            .await
            .unwrap()
    }

    fn client_cache() -> Arc<FragmentCache> {
        Arc::new(FragmentCache::new(Arc::new(Client::new())))
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
    async fn media_fragment_fetches_without_moving_the_stream() {
        // The first request for fragment 3 fails.
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

        // Fetching the last fragment leaves the stream at its start.
        assert_eq!(stream.media_fragment(4).await.unwrap(), &b"5"[..]);
        assert!(matches!(
            stream.media_fragment(5).await,
            Err(Error::Initialization(_))
        ));
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"init"[..]);
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"1"[..]);

        // A failed fetch is this caller's; the stream still downloads it itself.
        assert!(stream.media_fragment(2).await.is_err());
        assert_eq!(texts(&mut stream).await, ["2", "3", "4", "5"]);
        assert_eq!(requests(&counts, "/3"), 2);
        // Fragment 5 was outside the window, so it was not kept the first time.
        assert_eq!(requests(&counts, "/5"), 2);
    }

    #[tokio::test]
    async fn streams_whole_fragments_in_order() {
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

        // Fragment 1 was read whole, so the next item is fragment 2.
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"2"[..]);
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"345");

        // A byte seek through `AsyncSeek` moves streaming too.
        assert_eq!(stream.seek(SeekFrom::Start(4)).await.unwrap(), 4);
        assert_eq!(texts(&mut stream).await, ["1", "2", "3", "4", "5"]);
    }

    #[tokio::test]
    async fn seeks_by_time_to_a_fragment_start() {
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
        let ranged = |path: &str, range| Fragment::with_range(format!("{base}{path}"), range);
        let fragments = Fragments::new(
            ranged("/ranged", 0..=3),
            [ranged("/plain", 8..=13), ranged("/ranged", 14..=15)],
        );
        let mut stream = FragmentStream::new(fragments, None).await.unwrap();
        assert_eq!(texts(&mut stream).await, ["INIT", "BBBBBB", "cc"]);

        let short = Fragments::new(ranged("/short", 0..=3), Vec::<Fragment>::new());
        let short = FragmentStream::new(short, None).await;
        assert!(matches!(
            short,
            Err(Error::FragmentLength {
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

        // Two seconds ahead of the start is fragments 1 and 2, then it waits.
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

        // At fragment 3, one second behind keeps 2 and lets 1 go.
        let one = Fragment::new(format!("{base}/1"));
        assert_eq!(cache.cached(&one), None);
        assert!(cache.cached(&Fragment::new(format!("{base}/2"))).is_some());
        assert_eq!(cache.known_size(&one), Some(1));

        stream.seek_time(Duration::ZERO).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), &b"1"[..]);
        assert_eq!(requests(&counts, "/1"), 2);
    }
}
