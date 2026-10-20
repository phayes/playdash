//! Shared scheduling state for blocking and asynchronous segment streams.
//!
//! This module tracks stream positions, retention windows, and download work.
//! It performs no I/O and does not wait directly.

#[cfg(test)]
use super::Segments;
use super::buffer::BufferFeed;
use super::index::MediaIndex;
use super::store::{MediaRole, SegmentData, Store};
use super::{SegmentLocation, SegmentNumber, each};
use crate::error::Error;
use crate::manifest::BehindWindow;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, SeekFrom};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Retention windows for a [`SegmentStream`](crate::segment::SegmentStream): the media time of a
/// representation it keeps around its position. The default keeps everything and downloads to
/// the end, which suits audio; long video wants both bounded.
///
/// Both windows are measured from the start of the segment under the
/// stream's position, and that segment is always kept. The initialization
/// segment is always kept too. Retention needs timed
/// [`Segments`](crate::segment::Segments); a stream of untimed ones keeps
/// everything.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use playdash::Retention;
///
/// // Keep 30 seconds behind for a quick rewind, and download a minute ahead.
/// let retention = Retention {
///     behind: Some(Duration::from_secs(30)),
///     ahead: Some(Duration::from_secs(60)),
/// };
/// assert_ne!(retention, Retention::default());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Amount of media to retain before the current segment.
    ///
    /// Released segments may need to be downloaded again after a backward
    /// seek. `None` retains all earlier static media, or the full current
    /// window for a live presentation. A live stream may temporarily retain
    /// the segment being read after it leaves that window.
    pub behind: Option<Duration>,

    /// Media duration to download ahead of the current segment's start.
    ///
    /// Downloading pauses when the retained window reaches this limit and resumes as
    /// playback advances. `None` allows downloading through the end.
    pub ahead: Option<Duration>,
}

/// Cache updates produced when a stream's retained window moves.
#[derive(Debug, Default)]
pub(crate) struct WindowChange {
    pub(crate) acquired: Vec<Arc<SegmentData>>,
    pub(crate) released: Vec<Arc<SegmentData>>,
    /// Live segments removed from this schedule and eligible for eviction
    /// when no other stream uses them.
    pub(crate) trimmed: Vec<Arc<SegmentData>>,
}

impl WindowChange {
    /// Returns `true` if no cache ownership or schedule entries changed.
    pub(crate) fn is_empty(&self) -> bool {
        self.acquired.is_empty() && self.released.is_empty() && self.trimmed.is_empty()
    }
}

/// Iterates over the segments retained by `window`, plus `pin` if it lies
/// outside that window.
fn kept_numbers(
    window: &Range<SegmentNumber>,
    pin: Option<SegmentNumber>,
) -> impl Iterator<Item = SegmentNumber> {
    let pin = pin.filter(|pin| !window.contains(pin));
    each(window.clone()).chain(pin)
}

/// Byte and media-time information for a stream position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Absolute offset in the virtual concatenation of initialization and media bytes.
    ///
    /// This is `None` when preceding segment sizes are not yet known.
    pub byte: Option<u64>,

    /// Playback timestamp corresponding to this position, when a timeline is available.
    pub time: Option<TimePosition>,
}

/// Media timestamp of a stream position, and how precisely it describes that position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimePosition {
    /// Best-known playback timestamp for the position.
    /// It may differ from the requested seek time according to [`Self::accuracy`].
    pub timestamp: Duration,

    /// Granularity at which [`Self::timestamp`] is known.
    pub accuracy: TimeAccuracy,
}

/// Granularity of a reported media timestamp: how precisely it describes a stream
/// position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeAccuracy {
    /// To the media segment: the timestamp is the start of the segment the position
    /// is inside.
    Segment,

    /// To the packet: the timestamp is that of the packet at the position.
    Packet,
}

/// A place in a stream's bytes: the initialization segment, or a media
/// segment by its number. `Media` just past the last segment is the
/// end of the stream. Ordered as the bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Part {
    Init,
    Media(SegmentNumber),
}

impl Part {
    /// Returns the next part in a stream whose first media segment is `join`.
    fn next(self, join: SegmentNumber) -> Self {
        match self {
            Part::Init => Part::Media(join),
            Part::Media(number) => Part::Media(number.next()),
        }
    }

    /// The number of this media segment. `None` for the initialization
    /// segment.
    pub(crate) fn number(self) -> Option<SegmentNumber> {
        match self {
            Part::Init => None,
            Part::Media(number) => Some(number),
        }
    }

    /// Returns the media number, treating initialization as `join`.
    fn number_or(self, join: SegmentNumber) -> SegmentNumber {
        match self {
            Part::Init => join,
            Part::Media(number) => number,
        }
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Part::Init => f.write_str("the initialization segment"),
            Part::Media(number) => write!(f, "media segment {number}"),
        }
    }
}

/// The inputs a schedule adds media segments with: the store whose segment
/// data they share, and the time now.
#[derive(Clone, Copy)]
pub(crate) struct Supply<'a> {
    pub(crate) store: &'a Store,
    pub(crate) now: SystemTime,
}

/// A scheduled segment and its original presentation-time span.
///
/// The span remains stable even after a refreshed index drops the segment.
#[derive(Debug)]
struct Scheduled {
    data: Arc<SegmentData>,
    span: Option<Range<Duration>>,
}

/// Reason the GET worker currently has no segment to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Idle {
    /// Wait for this deadline, or indefinitely until the schedule changes.
    Wait(Option<SystemTime>),
    /// Refresh the live manifest because the current index lists no next segment.
    Refresh,
    /// The stream has reached its final segment.
    Ended,
}

/// Availability state of the next requested segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// It is available.
    Ready,
    /// It becomes available at this time.
    NotBefore(SystemTime),
    /// The live index does not list it yet.
    Refresh,
    /// It is past the last segment there will ever be.
    End,
}

/// Download state shared by a stream and its GET and HEAD workers.
///
/// Segments are added when they enter the retention window or are requested
/// by a seek, size probe, or direct fetch. Added entries remain in the
/// schedule even if their cached bodies are later released.
#[derive(Debug)]
pub(crate) struct Schedule {
    /// Initialization segment, always kept.
    pub(crate) init: Arc<SegmentData>,
    /// The representation's media segments, which the schedule adds from.
    index: Arc<MediaIndex>,
    /// Whether this index is final and its last segment ends the stream.
    index_final: bool,
    /// The decryption role each media segment gets as it is added.
    role: MediaRole,
    /// First media segment in the stream's byte sequence.
    join: SegmentNumber,
    /// Number assigned to the first entry in `media`.
    front: SegmentNumber,
    /// Scheduled media in timeline order, numbered from `front`.
    media: VecDeque<Scheduled>,
    /// Total size of media from `join` to `front`, or `None` if any size in
    /// that removed prefix is unknown.
    skipped: Option<u64>,
    /// Buffer subscribers and the first segment each has not received.
    buffers: Vec<(BufferFeed, SegmentNumber)>,
    /// When false, byte seeks wait on downloaded bodies instead of HEAD.
    pub(crate) head_available: bool,
    /// Background HEAD prefetch is wanted. Does not gate on-demand seek HEADs.
    pub(crate) eager_head: bool,
    /// A HEAD worker is alive, possibly finishing an in-flight request.
    pub(crate) head_worker_running: bool,
    /// Read/seek playhead. The GET worker fills at or after it first.
    pub(crate) cursor: Part,
    pub(crate) priority: Option<Part>,
    /// Whether all currently requested bodies are present.
    /// Live streams may become incomplete again when new segments appear.
    pub(crate) complete: bool,
    pub(crate) error: Option<String>,
    retention: Retention,
    /// The media segments the stream keeps now: its retention window, always
    /// among the segments added.
    pub(crate) window: Range<SegmentNumber>,
    /// Cursor segment retained outside `window` so an in-progress read can
    /// finish after the live window moves.
    pin: Option<SegmentNumber>,
    /// Action to take when the reader falls behind the live window.
    policy: BehindWindow,
    /// Target distance behind the live edge; zero for static streams.
    delay: Duration,
}

impl Schedule {
    /// Creates a schedule beginning with media segment `first`.
    ///
    /// Added media receives `role` and is retained according to `retention`.
    /// Untimed media is retained in full. `delay` is the target distance from
    /// the live edge and should be zero for a static stream.
    pub(crate) fn new(
        init: Arc<SegmentData>,
        index: Arc<MediaIndex>,
        first: SegmentNumber,
        retention: Retention,
        role: MediaRole,
        delay: Duration,
        supply: Supply,
    ) -> Self {
        let mut schedule = Self {
            init,
            index,
            index_final: false,
            role,
            join: first,
            front: first,
            media: VecDeque::new(),
            skipped: Some(0),
            buffers: Vec::new(),
            head_available: true,
            eager_head: false,
            head_worker_running: false,
            cursor: Part::Init,
            priority: None,
            complete: false,
            error: None,
            retention,
            window: first..first,
            pin: None,
            policy: BehindWindow::default(),
            delay,
        };

        // Add the window's segments, and see whether any is still to fetch.
        let change = schedule.rewindow(supply);
        debug_assert!(change.released.is_empty() && change.trimmed.is_empty());
        let window = schedule.window.clone();
        schedule.complete = !schedule.missing_body(Part::Init)
            && each(window).all(|number| !schedule.missing_body(Part::Media(number)));
        schedule
    }

    /// Returns the earliest segment reachable at `now`.
    pub(crate) fn earliest(&self, now: SystemTime) -> SegmentNumber {
        let first = self.index.first();
        match self.index.is_live() {
            true => first.max(self.index.available(now).start),
            false => first,
        }
    }

    /// Returns whether `part` has left the live window at `now`.
    fn expired(&self, part: Part, now: SystemTime) -> bool {
        match part {
            Part::Media(number) => number < self.earliest(now),
            Part::Init => false,
        }
    }

    /// The number just past the last media segment added.
    fn frontier(&self) -> SegmentNumber {
        self.front.plus(self.media.len())
    }

    /// Creates a scheduled entry for `number`, or returns `None` if it is not
    /// in the index.
    fn entry(&self, number: SegmentNumber, supply: Supply) -> Option<Scheduled> {
        let segment = self.index.segment(number)?;
        let data = supply.store.data(segment.location);
        self.role.assign(&data);
        Some(Scheduled {
            data,
            span: segment.span,
        })
    }

    /// Appends reachable index entries up to, but not including, `end`.
    fn extend_to(&mut self, end: SegmentNumber, supply: Supply) {
        if self.frontier() < self.earliest(supply.now) {
            return;
        }
        let end = end.min(self.horizon(supply.now));
        while self.frontier() < end {
            let Some(scheduled) = self.entry(self.frontier(), supply) else {
                break;
            };
            self.media.push_back(scheduled);
        }
    }

    /// Prepends reachable index entries down to `start`.
    fn extend_back_to(&mut self, start: SegmentNumber, supply: Supply) {
        let start = start.max(self.earliest(supply.now));
        while start < self.front {
            let Some(number) = self.front.checked_prev() else {
                break;
            };
            let Some(scheduled) = self.entry(number, supply) else {
                break;
            };
            self.media.push_front(scheduled);
            self.front = number;
        }
    }

    /// Advances an empty schedule to `start`, invalidating byte offsets that
    /// depend on skipped segment sizes.
    fn skip_to(&mut self, start: SegmentNumber) {
        if !self.media.is_empty() || start <= self.front {
            return;
        }
        if self.front.max(self.join) < start {
            self.skipped = None;
        }
        self.front = start;
    }

    /// Removes live segments that are before the retained window, current
    /// live window, and cursor, returning their cached data.
    fn trim(&mut self, now: SystemTime) -> Vec<Arc<SegmentData>> {
        if !self.index.is_live() {
            return Vec::new();
        }
        let keep_from = self
            .window
            .start
            .min(self.earliest(now))
            .min(self.cursor.number_or(self.join));
        let mut trimmed = Vec::new();
        while self.front < keep_from
            && let Some(scheduled) = self.media.pop_front()
        {
            // Preserve byte numbering from the stream's original join point.
            if self.front >= self.join {
                self.skipped = self
                    .skipped
                    .zip(scheduled.data.size())
                    .map(|(skipped, size)| skipped + size);
            }
            if self.priority == Some(Part::Media(self.front)) {
                self.priority = None;
            }
            self.front = self.front.next();
            trimmed.push(scheduled.data);
        }
        trimmed
    }

    /// Returns one past the last segment that may be added at `now`.
    fn horizon(&self, now: SystemTime) -> SegmentNumber {
        self.index.available(now).end.max(self.front)
    }

    /// Classifies whether segment `number` can be added at `now`.
    fn step(&self, number: SegmentNumber, now: SystemTime) -> Step {
        let live = self.index.is_live();
        if let Some(end) = self.index.end()
            && number >= end
        {
            return match live && !self.index_final {
                true => Step::Refresh,
                false => Step::End,
            };
        }
        if live && number >= self.index.available(now).end {
            return self
                .index
                .available_at(number)
                .map_or(Step::Refresh, Step::NotBefore);
        }
        Step::Ready
    }

    /// The media segment numbered `number`, if added.
    fn scheduled(&self, number: SegmentNumber) -> Option<&Scheduled> {
        self.media.get(number.offset_from(self.front)?)
    }

    /// The data of the media segment numbered `number`, if added.
    fn segment(&self, number: SegmentNumber) -> Option<&Arc<SegmentData>> {
        Some(&self.scheduled(number)?.data)
    }

    /// The presentation time the segment at `part` spans, if it is an added,
    /// timed media segment.
    pub(crate) fn span(&self, part: Part) -> Option<Range<Duration>> {
        self.scheduled(part.number()?)?.span.clone()
    }

    /// The data of the segment at `part`, or `None` for a media segment not
    /// added.
    pub(crate) fn part_data(&self, part: Part) -> Option<&Arc<SegmentData>> {
        match part {
            Part::Init => Some(&self.init),
            Part::Media(number) => self.segment(number),
        }
    }

    /// Returns data for `part`, extending the schedule to it when possible.
    ///
    /// Returns `None` if the part is past the end or is no longer reachable,
    /// unless it was already scheduled.
    pub(crate) fn materialise(&mut self, part: Part, supply: Supply) -> Option<&Arc<SegmentData>> {
        if let Part::Media(number) = part {
            match number < self.front {
                true => self.extend_back_to(number, supply),
                false => self.extend_to(number.next(), supply),
            }
        }
        self.part_data(part)
    }

    /// Iterates over the parts in the stream's byte sequence: initialization,
    /// then scheduled media from `join` onward.
    fn byte_parts(&self) -> impl Iterator<Item = (Part, &Arc<SegmentData>)> {
        let media = self
            .media
            .iter()
            .enumerate()
            .map(|(slot, scheduled)| (self.front.plus(slot), &scheduled.data))
            .filter(|&(number, _)| number >= self.join)
            .map(|(number, data)| (Part::Media(number), data));
        std::iter::once((Part::Init, &self.init)).chain(media)
    }

    /// The media segments added, in byte order.
    #[cfg(test)]
    pub(crate) fn media_data(&self) -> impl Iterator<Item = &Arc<SegmentData>> {
        self.media.iter().map(|scheduled| &scheduled.data)
    }

    /// Returns the position just past the final segment, or `None` while more may appear.
    pub(crate) fn end(&self) -> Option<Part> {
        Some(Part::Media(self.end_number()?))
    }

    /// Returns one past the final media number, or `None` while the index may refresh.
    fn end_number(&self) -> Option<SegmentNumber> {
        match self.index.is_live() && !self.index_final {
            true => None,
            false => Some(self.index.end()?.max(self.join)),
        }
    }

    /// Whether `part` is at or past the end of the stream.
    pub(crate) fn past_end(&self, part: Part) -> bool {
        self.end().is_some_and(|end| part >= end)
    }

    /// Returns the initialization segment and all media currently retained.
    pub(crate) fn kept(&self) -> Vec<Arc<SegmentData>> {
        let media = kept_numbers(&self.window, self.pin).filter_map(|number| self.segment(number));
        std::iter::once(&self.init).chain(media).cloned().collect()
    }

    /// Sets the action taken when a reader falls behind the live window.
    pub(crate) fn set_behind_window(&mut self, policy: BehindWindow) {
        self.policy = policy;
    }

    /// Changes the retention, and the window with it.
    pub(crate) fn set_retention(&mut self, retention: Retention, supply: Supply) -> WindowChange {
        self.retention = retention;
        self.rewindow(supply)
    }

    /// Moves the cursor to `part`, and the window with it.
    pub(crate) fn move_cursor(&mut self, part: Part, supply: Supply) -> WindowChange {
        self.cursor = part;
        self.rewindow(supply)
    }

    /// Asks the GET worker for the segment at `part` before anything else,
    /// even outside the window.
    pub(crate) fn request(&mut self, part: Part) {
        if self.missing_body(part) {
            self.priority = Some(part);
            self.complete = false;
        }
    }

    /// Whether the stream keeps the segment at `part`.
    pub(crate) fn keeps(&self, part: Part) -> bool {
        match part {
            Part::Init => true,
            Part::Media(number) => self.window.contains(&number) || self.pin == Some(number),
        }
    }

    /// Recomputes the retained window and reports the resulting cache changes.
    fn rewindow(&mut self, supply: Supply) -> WindowChange {
        // Pin the cursor only when it is scheduled but outside the new window.
        let window = self.window_at(self.cursor, supply.now);
        let pin = self
            .cursor
            .number()
            .filter(|number| !window.contains(number) && self.scheduled(*number).is_some());
        let old_window = std::mem::replace(&mut self.window, window);
        let old_pin = std::mem::replace(&mut self.pin, pin);

        // Record releases before trimming removes their schedule entries.
        let released = kept_numbers(&old_window, old_pin)
            .filter(|&number| !self.keeps(Part::Media(number)))
            .filter_map(|number| self.segment(number).cloned())
            .collect();

        // Remove unreachable entries, then fill the new window.
        let trimmed = self.trim(supply.now);
        let Range { start, end } = self.window;
        self.skip_to(start);
        self.extend_back_to(start, supply);
        self.extend_to(end, supply);
        let start = start.max(self.front);
        self.window = start..end.min(self.frontier()).max(start);
        self.feed_buffers(supply);

        // Acquire entries newly covered by the window or pin.
        let kept_before = |number: &SegmentNumber| {
            old_window.contains(number) || old_pin == Some(*number)
        };
        let acquired = kept_numbers(&self.window, self.pin)
            .filter(|number| !kept_before(number))
            .filter_map(|number| self.segment(number).cloned())
            .collect();
        if each(self.window.clone()).any(|number| self.missing_body(Part::Media(number))) {
            self.complete = false;
        }
        WindowChange {
            acquired,
            released,
            trimmed,
        }
    }

    /// Returns configured retention, using the live window depth as the
    /// default backward limit.
    fn effective_retention(&self) -> Retention {
        let depth = self
            .index
            .live()
            .and_then(|live| live.time_shift_buffer_depth);
        Retention {
            behind: self.retention.behind.or(depth),
            ..self.retention
        }
    }

    /// Computes the reachable retention window around `cursor` at `now`.
    ///
    /// Initialization is treated as the join point. A backward seek may move
    /// the start before the join point, but a live window never includes
    /// expired media.
    fn window_at(&self, cursor: Part, now: SystemTime) -> Range<SegmentNumber> {
        let end = self.horizon(now);
        let cursor = cursor.number_or(self.join);
        let origin = self.join.min(cursor).max(self.earliest(now));
        let retention = self.effective_retention();
        if self.index.timing().is_none() || retention == Retention::default() {
            return origin..end.max(origin);
        }

        // Past the last segment, the position is the end of the media.
        let playhead = match self.index.span(cursor) {
            Some(span) => span.start,
            None => end
                .checked_prev()
                .filter(|&last| last >= self.index.first())
                .and_then(|last| self.index.span(last))
                .map_or(Duration::ZERO, |span| span.end),
        };

        // As far behind and ahead of the playhead as retention says.
        let start = match retention.behind {
            Some(behind) => self
                .index
                .ending_after(playhead.saturating_sub(behind))
                .map_or(origin, |start| start.max(origin)),
            None => origin,
        };
        let until = match retention.ahead {
            Some(ahead) => self
                .index
                .starting_from(playhead.saturating_add(ahead))
                .map_or(end, |until| until.max(origin)),
            None => end,
        };
        // Clamp a live cursor that is waiting beyond the current availability horizon.
        let until = until.max(cursor.next()).min(end);
        start.min(until)..until
    }

    pub(crate) fn all_sizes_known(&self) -> bool {
        self.total_len().is_some()
    }

    /// Whether the segment at `part` exists and its body is not held.
    pub(crate) fn missing_body(&self, part: Part) -> bool {
        self.part_data(part).is_some_and(|data| !data.has_body())
    }

    /// Returns the first byte-sequence part with an unknown size, extending
    /// the schedule as needed.
    pub(crate) fn first_unknown_size(&mut self, supply: Supply) -> Option<Part> {
        // Among the segments added so far.
        if let Some((part, _)) = self.byte_parts().find(|(_, data)| data.size().is_none()) {
            return Some(part);
        }

        // Past them, one at a time.
        loop {
            let number = self.frontier();
            self.extend_to(number.next(), supply);
            if self.segment(number)?.size().is_none() {
                return Some(Part::Media(number));
            }
        }
    }

    #[cfg_attr(not(feature = "log"), allow(unused_variables))]
    pub(crate) fn disable_head(&mut self, reason: &str) {
        if self.head_available {
            self.head_available = false;
            #[cfg(feature = "log")]
            log::warn!("MPEG stream disabling HEAD size probes: {reason}");
        }
    }

    /// Selects the next segment to download at `now`.
    ///
    /// Explicit requests take priority, followed by initialization and missing
    /// media near the cursor. Expired live segments are never selected.
    pub(crate) fn next_get(&mut self, now: SystemTime) -> Option<(Part, Arc<SegmentData>)> {
        let Range { start, end } = self.window;
        let cursor = self.cursor.number_or(self.join).clamp(start, end);
        let priority = self.priority.take();
        let fetchable = |part: Part| self.missing_body(part) && !self.expired(part, now);
        let part = priority
            .filter(|&priority| fetchable(priority))
            .or_else(|| {
                let media = each(cursor..end).chain(each(start..cursor));
                std::iter::once(Part::Init)
                    .chain(media.map(Part::Media))
                    .find(|&part| fetchable(part))
            })?;
        Some((part, self.part_data(part)?.clone()))
    }

    /// The next segment whose size a HEAD should learn.
    pub(crate) fn next_head(&mut self, supply: Supply) -> Option<(Part, Arc<SegmentData>)> {
        if !self.head_available {
            return None;
        }
        let part = self.first_unknown_size(supply)?;
        Some((part, self.part_data(part)?.clone()))
    }

    /// Returns the absolute byte offset at which `part` starts.
    ///
    /// Returns `None` when a required earlier size is unknown, or when `part`
    /// lies before the join point, outside the schedule, or beyond scheduled
    /// media.
    pub(crate) fn prefix_len(&self, part: Part) -> Option<u64> {
        let Part::Media(number) = part else {
            return Some(0);
        };
        if number < self.join || number < self.front || number > self.frontier() {
            return None;
        }
        let media: Option<u64> = self
            .byte_parts()
            .skip(1)
            .take_while(|&(before, _)| before < part)
            .map(|(_, data)| data.size())
            .sum();
        self.init.size()?.checked_add(self.skipped?)?.checked_add(media?)
    }

    /// Returns the exact byte length once all segment sizes are known.
    /// Returns `None` while live or while any size is unknown.
    pub(crate) fn total_len(&self) -> Option<u64> {
        self.prefix_len(self.end()?)
    }

    /// The exact total length when every size is known; otherwise, given a
    /// symphonia-compat `estimate`, the larger of it and the known sizes.
    pub(crate) fn reported_end_len(&self, estimate: Option<u64>) -> Option<u64> {
        if let Some(exact) = self.total_len() {
            return Some(exact);
        }
        let known: u64 = self.byte_parts().filter_map(|(_, data)| data.size()).sum();
        estimate.map(|estimate| estimate.max(known))
    }

    // TODO: Publish a media segment's leading `moof` as soon as those bytes
    // have arrived, instead of waiting for the trailing `mdat`. IsoMp4 open
    // and the first packet only need the `moof`; `Read` currently blocks until
    // the whole GET has arrived. Stream the body, parse top-level box sizes,
    // and wake readers once the `moof` is complete.
    /// Stops trusting HEAD if the body of the segment at `part` disagrees
    /// with its probe.
    pub(crate) fn check_probe(&mut self, part: Part) {
        let Some(data) = self.part_data(part) else {
            return;
        };
        if let (Some(&expected), Some(len)) = (data.probed_size.get(), data.fetched_size())
            && expected != len
        {
            self.disable_head(&format!(
                "GET length {len} != HEAD length {expected} for {part}"
            ));
        }
    }

    /// The body of the segment at `part`, after checking it against its HEAD
    /// probe. Readers take bodies through this, not straight from the store:
    /// the body is published before the fetching worker checks it, and may
    /// have been fetched by another stream sharing the cache.
    ///
    /// Also settles a request for `part`. One made while `part` was already
    /// downloading outlives that download, and would otherwise fetch `part`
    /// again once the window has dropped it.
    pub(crate) fn checked_body(&mut self, part: Part) -> Option<Bytes> {
        if self.priority == Some(part) {
            self.priority = None;
        }
        self.check_probe(part);
        self.part_data(part)?.body()
    }

    /// Resolves an absolute byte offset to a part and an offset within it.
    ///
    /// Returns `None` if required sizes are unknown or the offset belongs to
    /// live media removed from the schedule. Once the final segment is known,
    /// offsets beyond the end land at the end of the last part.
    pub(crate) fn resolve_cursor(&self, target: u64) -> Option<(Part, usize)> {
        // Byte positions begin with initialization and continue from `join`.
        let mut parts = self.byte_parts();
        let (_, init) = parts.next()?;
        let init_len = init.size()?;
        if target < init_len {
            return Some((Part::Init, target as usize));
        }
        let mut acc = init_len.checked_add(self.skipped?)?;
        if target < acc {
            return None;
        }
        for (part, data) in parts {
            let len = data.size()?;
            if target < acc + len {
                return Some((part, (target - acc) as usize));
            }
            acc += len;
        }

        // An offset beyond added data is valid only after the final segment is known.
        if self.end_number().is_none_or(|end| self.frontier() < end) {
            return None;
        }
        let last = self
            .frontier()
            .checked_prev()
            .filter(|&last| last >= self.join)
            .map_or(Part::Init, Part::Media);
        let last_len = self.part_data(last)?.size().unwrap_or(0);
        Some((last, last_len as usize))
    }
}

/// The rest of a stream's segment, with where it sits.
#[derive(Clone, Debug)]
pub(crate) struct PartBody {
    pub(crate) part: Part,
    /// Number in the representation, for a media segment.
    pub(crate) number: Option<SegmentNumber>,
    /// Presentation-time span of a timed media segment.
    pub(crate) span: Option<Range<Duration>>,
    pub(crate) location: SegmentLocation,
    pub(crate) body: Bytes,
    /// Whether playback jumped here after falling behind the live window.
    pub(crate) jumped: bool,
}

/// A stream reader's position: a part, an offset into it, and the absolute
/// byte offset when the sizes before it are known.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    pub(crate) part: Part,
    pub(crate) offset_in_segment: usize,
    pub(crate) byte_position: Option<u64>,
    /// First media segment in this cursor's byte sequence.
    join: SegmentNumber,
}

impl Cursor {
    /// Creates a cursor at byte zero, before media segment `join`.
    pub(crate) fn start(join: SegmentNumber) -> Self {
        Self {
            part: Part::Init,
            offset_in_segment: 0,
            byte_position: Some(0),
            join,
        }
    }

    /// Copies from `body`, the segment under the cursor, into `buf` as far as
    /// it fits, and moves past what was copied. Zero means nothing was left of
    /// the segment, and the cursor has moved on to the next one.
    pub(crate) fn read(&mut self, body: &[u8], buf: &mut [u8]) -> usize {
        let offset = self.offset_in_segment;
        if offset >= body.len() {
            self.next_segment();
            return 0;
        }
        let copied = (body.len() - offset).min(buf.len());
        buf[..copied].copy_from_slice(&body[offset..offset + copied]);
        self.offset_in_segment += copied;
        if self.offset_in_segment >= body.len() {
            self.next_segment();
        }
        self.advance_bytes(copied);
        copied
    }

    /// The rest of `body`, the segment under the cursor, moving on to the
    /// next segment. `None` when nothing was left of it.
    pub(crate) fn take_rest(&mut self, body: Bytes) -> Option<Bytes> {
        let offset = self.offset_in_segment.min(body.len());
        let rest = body.len() - offset;
        self.next_segment();
        self.advance_bytes(rest);
        (rest > 0).then(|| body.slice(offset..))
    }

    /// Moves to `offset_in_segment` bytes into `part`, at absolute byte
    /// `byte_position` when known.
    pub(crate) fn place(
        &mut self,
        part: Part,
        offset_in_segment: usize,
        byte_position: Option<u64>,
    ) {
        self.part = part;
        self.offset_in_segment = offset_in_segment;
        self.byte_position = byte_position;
    }

    /// Records the cursor's absolute byte offset when it is at the start of a
    /// part whose offset is known.
    ///
    /// This can restore byte-position reporting after forward reading reaches
    /// a part with a known place in the stream.
    pub(crate) fn learn_byte_position(&mut self, start: Option<u64>) {
        if self.byte_position.is_none() && self.offset_in_segment == 0 {
            self.byte_position = start;
        }
    }

    fn next_segment(&mut self) {
        self.part = self.part.next(self.join);
        self.offset_in_segment = 0;
    }

    fn advance_bytes(&mut self, count: usize) {
        if let Some(position) = self.byte_position {
            self.byte_position = Some(position + count as u64);
        }
    }
}

/// The absolute offset `from` names, given the current position and, for
/// [`SeekFrom::End`], the total length. Fails for [`SeekFrom::Current`]
/// without a position, and for [`SeekFrom::End`] without a total.
pub(crate) fn seek_target(
    from: SeekFrom,
    position: Option<u64>,
    total: Option<u64>,
) -> io::Result<u64> {
    let (base, delta) = match from {
        SeekFrom::Start(offset) => return Ok(offset),
        SeekFrom::Current(delta) => {
            let Some(position) = position else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "MPEG stream absolute byte position is unknown; use seek_time or SeekFrom::Start",
                ));
            };
            (position, delta)
        }
        SeekFrom::End(delta) => {
            let Some(total) = total else {
                return Err(io::Error::other(
                    "MPEG stream length is unknown: HEAD requests failed to learn the sizes of segments not yet downloaded",
                ));
            };
            (total, delta)
        }
    };
    Ok(match delta >= 0 {
        true => base.saturating_add(delta as u64),
        false => base.saturating_sub(delta.unsigned_abs()),
    })
}

/// Time queries, answered from the schedule's index.
impl Schedule {
    /// The representation's media segments.
    pub(crate) fn index(&self) -> &Arc<MediaIndex> {
        &self.index
    }

    /// Clamps `time` to media available at `now`, using the stream's live
    /// delay when `time` is at or beyond the live edge.
    ///
    /// Fails if a live stream currently has no available segment.
    pub(crate) fn clamp_time(&self, time: Duration, now: SystemTime) -> io::Result<Duration> {
        self.index
            .clamp_time(time, now, self.delay)
            .ok_or_else(|| io::Error::other("MPEG stream has no live media segment available yet"))
    }

    /// Returns the segment on which a seek to `time` lands after clamping it
    /// to currently available media.
    pub(crate) fn time_target(&self, time: Duration, now: SystemTime) -> io::Result<SegmentNumber> {
        let invalid = |reason: &str| io::Error::new(io::ErrorKind::InvalidInput, reason);
        if self.index.timing().is_none() {
            return Err(invalid("MPEG stream has no media timeline"));
        }
        let time = self.clamp_time(time, now)?;
        self.index
            .number_at(time)
            .ok_or_else(|| invalid("MPEG stream has no media segments"))
    }

    /// Returns the presentation time at which this stream's first media
    /// segment begins, or `None` for untimed or missing media.
    #[cfg(feature = "rodio")]
    pub(crate) fn join_time(&self) -> Option<Duration> {
        Some(self.index.span(self.join)?.start)
    }

    /// The start of the media segment at `part`, as a position of `accuracy`.
    /// `None` for the initialization segment, which has no media time, and for
    /// untimed segments.
    pub(crate) fn time_at(&self, part: Part, accuracy: TimeAccuracy) -> Option<TimePosition> {
        let Part::Media(number) = part else {
            return None;
        };
        Some(TimePosition {
            timestamp: self.index.span(number)?.start,
            accuracy,
        })
    }

    /// The presentation time the media segments available at `now` span:
    /// every one of a static stream's. `None` when untimed, or while no live
    /// segment is available.
    pub(crate) fn available_time(&self, now: SystemTime) -> Option<Range<Duration>> {
        let numbers = self.index.available(now);
        let last = numbers
            .end
            .checked_prev()
            .filter(|&last| last >= numbers.start)?;
        Some(self.index.span(numbers.start)?.start..self.index.span(last)?.end)
    }

    /// Returns the live edge at `now`.
    ///
    /// Returns `None` for static streams or before any live segment is available.
    pub(crate) fn live_edge(&self, now: SystemTime) -> Option<Duration> {
        match self.index.is_live() {
            true => self.index.live_edge(now),
            false => None,
        }
    }

    /// The end of the representation's media, as a packet-accurate position.
    pub(crate) fn time_at_end(&self) -> Option<TimePosition> {
        self.index.end_time().map(|timestamp| TimePosition {
            timestamp,
            accuracy: TimeAccuracy::Packet,
        })
    }

    /// Initializes `feed` with available timed media and subscribes it to
    /// future live-window updates.
    pub(crate) fn subscribe_buffer(&mut self, feed: BufferFeed, supply: Supply) {
        feed.replace(self.timed_media(supply.store, supply.now));
        if self.index.is_live() {
            self.buffers.push((feed, self.horizon(supply.now)));
        }
    }

    /// Returns timed media available at `now` and its presentation spans.
    fn timed_media(
        &self,
        store: &Store,
        now: SystemTime,
    ) -> Vec<(Arc<SegmentData>, Range<Duration>)> {
        timed_media(&self.index, self.earliest(now)..self.horizon(now), store)
    }

    /// Updates buffer subscribers for changes to the live window and removes
    /// subscribers whose handles were dropped.
    fn feed_buffers(&mut self, supply: Supply) {
        if self.buffers.is_empty() {
            return;
        }
        let horizon = self.horizon(supply.now);
        let window_start = self
            .index
            .span(self.earliest(supply.now))
            .map_or(Duration::ZERO, |span| span.start);
        let index = &self.index;
        self.buffers.retain_mut(|(feed, fed)| {
            let media = match *fed < horizon {
                true => timed_media(index, *fed..horizon, supply.store),
                false => Vec::new(),
            };
            *fed = (*fed).max(horizon);
            feed.update(media, window_start)
        });
    }

    /// The length a symphonia-compat stream reports before every size is
    /// known, for media at `bitrate_bps`.
    pub(crate) fn compat_estimate(&self, bitrate_bps: u32) -> Result<u64, Error> {
        if self.index.timing().is_none() {
            return Err(Error::Initialization(
                "symphonia_compat requires a media timeline".to_owned(),
            ));
        }
        if self.index.is_live() {
            return Err(Error::Initialization(
                "symphonia_compat needs a length, which a live stream does not have".to_owned(),
            ));
        }
        let init_len = self.init.size().unwrap_or(0);
        self.index
            .estimate_len(self.join, init_len, bitrate_bps)
            .ok_or_else(|| {
                Error::Initialization(
                    "symphonia_compat requires a non-zero bitrate and media segments".to_owned(),
                )
            })
    }
}

/// Result of checking whether a stream reader can make progress.
pub(crate) enum ReadStep {
    /// Read the returned segment data.
    Ready(PartBody),
    /// No more media will become available.
    End,
    /// Playback fell behind and the configured policy requires an error.
    /// `available` is the current seekable presentation-time range.
    Behind { available: Range<Duration> },
    /// Playback should resume at `to`; `byte` is its absolute offset when
    /// known, and `change` describes the corresponding cache update.
    Jumped {
        to: Part,
        byte: Option<u64>,
        change: WindowChange,
    },
}

/// What a stream does next to learn segment sizes: see
/// [`Schedule::size_step`].
pub(crate) enum SizeStep {
    /// The sizes wanted are known.
    Known,
    /// HEAD this segment, then take another step.
    Probe(Part, Arc<SegmentData>),
    /// Wait until the sizes are known or the GET worker has nothing left to
    /// fetch.
    Wait,
}

/// The steps the blocking and async streams and their workers take, between
/// the waiting and I/O each does its own way.
impl Schedule {
    /// The next step toward `known` holding: done, a HEAD to make, or, once
    /// HEAD is unavailable, a GET of the first segment of unknown size, even
    /// outside the window, to wait on. Its size is kept after its body is
    /// dropped.
    pub(crate) fn size_step(
        &mut self,
        known: impl Fn(&Schedule) -> bool,
        supply: Supply,
    ) -> SizeStep {
        if known(self) {
            return SizeStep::Known;
        }
        if let Some((part, data)) = self.next_head(supply) {
            return SizeStep::Probe(part, data);
        }
        if let Some(part) = self.first_unknown_size(supply) {
            self.request(part);
        }
        SizeStep::Wait
    }

    /// Records that a HEAD of the segment at `part` failed with `error`,
    /// which stops HEAD probing.
    pub(crate) fn probe_failed(&mut self, part: Part, error: &Error) {
        self.disable_head(&format!("HEAD failed for {part}: {error}"));
    }

    /// Takes the next segment to download at `now`, or marks the current work
    /// complete if none is ready.
    pub(crate) fn take_get(&mut self, now: SystemTime) -> Option<(Part, Arc<SegmentData>)> {
        let work = self.next_get(now);
        if work.is_none() {
            self.complete = true;
        }
        work
    }

    /// Records the result of downloading `part` and returns whether the
    /// stream still retains it.
    ///
    /// A failure ends the stream unless the segment expired while the request
    /// was in flight; readers handle that case through [`BehindWindow`].
    pub(crate) fn finish_get(
        &mut self,
        part: Part,
        result: Result<(), Error>,
        now: SystemTime,
    ) -> bool {
        match result {
            Ok(()) => self.fetched(part),
            Err(_) if self.expired(part, now) => self.keeps(part),
            Err(error) => {
                self.error = Some(error.to_string());
                self.keeps(part)
            }
        }
    }

    /// Checks the just fetched body of the segment at `part` against its HEAD
    /// probe. Returns whether the stream keeps the segment.
    pub(crate) fn fetched(&mut self, part: Part) -> bool {
        self.check_probe(part);
        self.keeps(part)
    }

    /// Asks for background HEADs of unknown sizes. Returns whether a HEAD
    /// worker should start for them: none runs yet, and there is one to make.
    pub(crate) fn start_eager_head(&mut self, supply: Supply) -> bool {
        self.eager_head = true;
        if self.head_worker_running || self.error.is_some() || self.next_head(supply).is_none() {
            // A running worker sees `eager_head` on its next check and continues.
            return false;
        }
        self.head_worker_running = true;
        true
    }

    /// The next HEAD the background worker should make. `None` when it should
    /// stop, which records that it has.
    pub(crate) fn next_eager_head(&mut self, supply: Supply) -> Option<(Part, Arc<SegmentData>)> {
        let next = match self.eager_head && self.error.is_none() {
            true => self.next_head(supply),
            false => None,
        };
        if next.is_none() {
            self.head_worker_running = false;
        }
        next
    }

    /// Returns the next action for a reader at `part`, or `None` while it must
    /// wait for a download or newly published live media.
    ///
    /// Already downloaded data remains readable after it expires. Missing
    /// expired data is handled according to the stream's [`BehindWindow`]
    /// policy.
    pub(crate) fn read_step(&mut self, part: Part, supply: Supply) -> Option<ReadStep> {
        // End-of-stream and cached data can be decided immediately.
        if self.past_end(part) {
            return Some(ReadStep::End);
        }
        if self.part_data(part).is_some_and(|data| data.has_body()) {
            return self.checked_part(part).map(ReadStep::Ready);
        }

        // Missing expired data invokes the configured behind-window policy.
        if self.expired(part, supply.now) {
            return Some(self.behind(supply));
        }

        // Wait if a download can still provide this part. For live media, a
        // part beyond the current schedule may still become available as time
        // passes or the manifest refreshes.
        let unlisted_live = self.index.is_live()
            && part
                .number()
                .is_some_and(|number| number >= self.frontier());
        if unlisted_live || (self.missing_body(part) && !self.complete) {
            return None;
        }
        Some(self.checked_part(part).map_or(ReadStep::End, ReadStep::Ready))
    }

    /// Applies the [`BehindWindow`] policy after the next segment expires.
    fn behind(&mut self, supply: Supply) -> ReadStep {
        let target = match self.policy {
            BehindWindow::Fail => {
                let available = self.available_time(supply.now);
                return ReadStep::Behind {
                    available: available.unwrap_or_default(),
                };
            }
            BehindWindow::WindowStart => self.earliest(supply.now),
            BehindWindow::LiveEdge => self.index.behind_edge(supply.now, self.delay),
        };
        let to = Part::Media(target);
        let change = self.move_cursor(to, supply);
        self.request(to);
        ReadStep::Jumped {
            to,
            byte: self.prefix_len(to),
            change,
        }
    }

    /// Moves the window over live segments that have become available by
    /// `supply`'s time, for the GET worker once it has nothing to fetch.
    /// Returns the window change, and why there may still be nothing to
    /// fetch.
    pub(crate) fn advance(&mut self, supply: Supply) -> (WindowChange, Idle) {
        let change = self.rewindow(supply);
        let idle = match self.step(self.window.end, supply.now) {
            // Retention, rather than availability, currently limits the window.
            Step::Ready => Idle::Wait(None),
            Step::NotBefore(time) => Idle::Wait(Some(time)),
            Step::Refresh => Idle::Refresh,
            Step::End => Idle::Ended,
        };
        (change, idle)
    }

    /// Replaces the index with `index`, a refreshed copy numbered as the old
    /// one, and moves the window over the segments it makes available.
    /// Segments already added keep their data and spans.
    pub(crate) fn replace_index(&mut self, index: Arc<MediaIndex>, supply: Supply) -> WindowChange {
        self.index = index;
        self.rewindow(supply)
    }

    /// Marks the current index final so the stream ends after its last segment.
    pub(crate) fn finish_index(&mut self) {
        self.index_final = true;
    }

    /// The body of the segment at `part`, as [`Self::checked_body`] takes
    /// it, with where the segment sits.
    pub(crate) fn checked_part(&mut self, part: Part) -> Option<PartBody> {
        let body = self.checked_body(part)?;
        Some(PartBody {
            part,
            number: part.number(),
            span: self.span(part),
            location: self.part_data(part)?.location.clone(),
            body,
            jumped: false,
        })
    }

    /// The part and position a [`SeekFrom::End`]`(0)` reports without
    /// waiting on sizes: the end, at the exact length once every size is
    /// known, else at least `estimate`. `None` with neither.
    pub(crate) fn reported_end(&self, estimate: Option<u64>) -> Option<(Part, Position)> {
        let end = self.end()?;
        let len = self.reported_end_len(estimate)?;
        let position = Position {
            byte: Some(len),
            time: self.time_at_end(),
        };
        Some((end, position))
    }

    /// Lands a byte seek at absolute offset `target`, once the sizes before it
    /// are known: moves the cursor and the window there and asks for its
    /// segment first. Returns the part and offset into it, the position, and
    /// the window change.
    pub(crate) fn land_byte_seek(
        &mut self,
        target: u64,
        supply: Supply,
    ) -> io::Result<(Part, usize, Position, WindowChange)> {
        let unplaced = match self.index.is_live() {
            true => {
                "MPEG stream cannot place the requested byte offset: it is in live segments that have left the stream, or of unknown size, or not yet available"
            }
            false => {
                "MPEG stream cannot place the requested byte offset: HEAD requests failed to learn the sizes of the segments before it, which are not yet downloaded"
            }
        };
        let (part, offset_in_segment) = self
            .resolve_cursor(target)
            .ok_or_else(|| io::Error::other(unplaced))?;
        let change = self.move_cursor(part, supply);
        self.request(part);
        let position = Position {
            byte: Some(target),
            time: self.time_at(part, accuracy_at(offset_in_segment)),
        };
        Ok((part, offset_in_segment, position, change))
    }

    /// Rejects [`SeekFrom::End`] for a live stream with
    /// [`io::ErrorKind::Unsupported`].
    pub(crate) fn check_seek_from(&self, from: SeekFrom) -> io::Result<()> {
        match (from, self.end()) {
            (SeekFrom::End(_), None) if self.index.is_live() => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "MPEG stream is live and has no end to seek from",
            )),
            _ => Ok(()),
        }
    }

    /// Begins a seek to `time`, clamping it to available media, moving the
    /// cursor and retained window, and prioritizing the target segment.
    pub(crate) fn begin_time_seek(
        &mut self,
        time: Duration,
        supply: Supply,
    ) -> io::Result<(Part, WindowChange)> {
        let target = Part::Media(self.time_target(time, supply.now)?);
        if self.past_end(target) {
            return Err(io::Error::other(
                "MPEG stream ended before the requested media time",
            ));
        }
        let change = self.move_cursor(target, supply);
        self.request(target);
        Ok((target, change))
    }

    /// Converts the reader's completed `step` into the result of a time seek.
    ///
    /// Returns the part where reading will resume, its starting position, and
    /// any retention-window change caused by applying the [`BehindWindow`]
    /// policy to an expired live target.
    ///
    /// Fails if the stream ends before the target is reached. When
    /// [`BehindWindow::Fail`] is configured, it also fails with
    /// [`Error::BehindLiveWindow`] if the target expires while the seek is in
    /// progress.
    pub(crate) fn land_time_seek(
        &self,
        step: ReadStep,
    ) -> io::Result<(Part, Position, WindowChange)> {
        let (part, change) = match step {
            ReadStep::Ready(found) => (found.part, WindowChange::default()),
            ReadStep::Jumped { to, change, .. } => (to, change),
            ReadStep::End => {
                return Err(io::Error::other(
                    "MPEG stream ended before the requested media time",
                ));
            }
            ReadStep::Behind { available } => {
                return Err(io::Error::other(Error::BehindLiveWindow { available }));
            }
        };
        let position = Position {
            byte: self.prefix_len(part),
            time: self.time_at(part, TimeAccuracy::Packet),
        };
        Ok((part, position, change))
    }
}

/// The timed media segments of `index` numbered in `numbers`, with their data
/// in `store` and the presentation time each spans. Empty for an untimed
/// index.
pub(crate) fn timed_media(
    index: &MediaIndex,
    numbers: Range<SegmentNumber>,
    store: &Store,
) -> Vec<(Arc<SegmentData>, Range<Duration>)> {
    each(numbers)
        .filter_map(|number| {
            let segment = index.segment(number)?;
            Some((store.data(segment.location), segment.span?))
        })
        .collect()
}

/// The number of the media segment of `index` covering `time`, where a
/// stream starting at a time begins, and that segment's start. A time at or
/// past the end starts at the last media segment.
pub(crate) fn start_at(
    index: &MediaIndex,
    time: Duration,
) -> Result<(SegmentNumber, Duration), Error> {
    index
        .number_at(time)
        .and_then(|first| Some((first, index.span(first)?.start)))
        .ok_or_else(|| {
            Error::Initialization(
                "MPEG stream needs timed media segments to start at a time".to_owned(),
            )
        })
}

/// The accuracy of a segment's start time for a byte position
/// `offset_in_segment` bytes into that segment: exact to the packet at its
/// start, and only to the segment past it.
pub(crate) fn accuracy_at(offset_in_segment: usize) -> TimeAccuracy {
    match offset_in_segment == 0 {
        true => TimeAccuracy::Packet,
        false => TimeAccuracy::Segment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::store::Store;
    use crate::segment::timed_for_test;

    fn n(number: u64) -> SegmentNumber {
        SegmentNumber(number)
    }

    fn media(number: u64) -> Part {
        Part::Media(n(number))
    }

    /// Four one-second segments of ten bytes each, after a four-byte init.
    fn four_seconds() -> Segments {
        let ranged = |start: u64| SegmentLocation::with_range("file", start..=start + 9);
        timed_for_test(
            SegmentLocation::with_range("file", 0..=3),
            [ranged(4), ranged(14), ranged(24), ranged(34)],
            1,
            &[1; 4],
        )
    }

    /// Four one-second segments of unknown size.
    fn four_unsized() -> Segments {
        timed_for_test("init", ["a", "b", "c", "d"], 1, &[1; 4])
    }

    /// Keeps nothing behind the position, and `ahead` seconds ahead of it.
    fn ahead(ahead: u64) -> Retention {
        Retention {
            behind: Some(Duration::ZERO),
            ahead: Some(secs(ahead)),
        }
    }

    /// Inputs that add segments with data in `store`, at a fixed time.
    fn supply(store: &Store) -> Supply<'_> {
        Supply {
            store,
            now: SystemTime::UNIX_EPOCH,
        }
    }

    /// A schedule of `segments` from the media segment numbered `first`,
    /// keeping everything.
    fn schedule_from(segments: &Segments, first: u64, store: &Store) -> Schedule {
        retained_from(segments, first, Retention::default(), store)
    }

    /// A schedule of `segments` from the media segment numbered `first`,
    /// keeping what `retention` says.
    fn retained_from(
        segments: &Segments,
        first: u64,
        retention: Retention,
        store: &Store,
    ) -> Schedule {
        let init = store.data(segments.init().clone());
        let index = segments.index().clone();
        let role = MediaRole::default();
        let delay = Duration::ZERO;
        Schedule::new(init, index, n(first), retention, role, delay, supply(store))
    }

    /// The numbers of the media segments `schedule` has added.
    fn added(schedule: &Schedule) -> Range<SegmentNumber> {
        schedule.front..schedule.frontier()
    }

    /// Reference time used by tests of static schedules.
    const EPOCH: SystemTime = SystemTime::UNIX_EPOCH;

    #[test]
    fn time_target_lands_on_reported_segment_starts() {
        // A third of a second does not land on a whole nanosecond.
        let store = Store::default();
        let segments = timed_for_test("init", ["a", "b"], 3, &[1, 1]);
        let schedule = schedule_from(&segments, 0, &store);
        let second_start = segments.span(1).unwrap().start;
        let target = |time| schedule.time_target(time, EPOCH).unwrap();
        assert_eq!(target(second_start), n(1));
        assert_eq!(target(second_start - Duration::from_nanos(1)), n(0));
        assert_eq!(target(Duration::from_secs(5)), n(1));

        let empty = timed_for_test("init", Vec::<SegmentLocation>::new(), 3, &[]);
        let untimed = Segments::new("init", ["a"]);
        for segments in [empty, untimed] {
            let schedule = schedule_from(&segments, 0, &store);
            let error = schedule.time_target(Duration::ZERO, EPOCH).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn a_stream_from_partway_keeps_the_representation_times() {
        // Join at the third of four one-second segments.
        let segments = four_seconds();
        let store = Store::default();
        let schedule = schedule_from(&segments, 2, &store);

        // Buffer reporting covers the representation, including media before the join point.
        let timed = schedule.timed_media(&store, SystemTime::UNIX_EPOCH);
        let scheduled: Vec<_> = timed
            .iter()
            .map(|(data, span)| (data.location.clone(), span.clone()))
            .collect();
        let segment = |start: u64, time: u64| {
            let location = SegmentLocation::with_range("file", start..=start + 9);
            (location, secs(time)..secs(time + 1))
        };
        assert_eq!(
            scheduled,
            [segment(4, 0), segment(14, 1), segment(24, 2), segment(34, 3)]
        );

        // Buffer reporting reuses the schedule's stored segment data.
        assert!(Arc::ptr_eq(
            &timed[2].0,
            schedule.part_data(media(2)).unwrap()
        ));
        let untimed = schedule_from(&Segments::new("init", ["a"]), 0, &store);
        assert!(
            untimed
                .timed_media(&store, SystemTime::UNIX_EPOCH)
                .is_empty()
        );

        // Time seeks use the full presentation timeline and may precede the join point.
        let target = |time| schedule.time_target(time, EPOCH).unwrap();
        assert_eq!(target(secs(2)), n(2));
        assert_eq!(target(Duration::from_millis(3_500)), n(3));
        assert_eq!(target(Duration::from_millis(1_500)), n(1));

        let at = |part| schedule.time_at(part, TimeAccuracy::Packet);
        assert_eq!(at(Part::Init), None);
        assert_eq!(at(media(3)).unwrap().timestamp, secs(3));
        assert_eq!(at(media(4)), None);
        assert_eq!(schedule.time_at_end().unwrap().timestamp, secs(4));
        assert_eq!(
            start_at(segments.index(), secs(3)).unwrap(),
            (n(3), secs(3))
        );
    }

    #[test]
    fn an_end_relative_target_needs_the_total() {
        assert_eq!(seek_target(SeekFrom::End(-2), None, Some(10)).unwrap(), 8);
        assert_eq!(seek_target(SeekFrom::Current(3), Some(4), None).unwrap(), 7);
        assert_eq!(seek_target(SeekFrom::Start(5), None, None).unwrap(), 5);
        assert!(seek_target(SeekFrom::End(0), Some(4), None).is_err());
        assert!(seek_target(SeekFrom::Current(0), None, Some(10)).is_err());
    }

    #[test]
    fn a_schedule_addresses_segments_by_number() {
        let store = Store::default();
        let schedule = schedule_from(&four_seconds(), 2, &store);
        assert_eq!(schedule.join, n(2));
        assert_eq!(
            added(&schedule),
            n(2)..n(4),
            "keeping everything adds it all"
        );
        assert_eq!(schedule.end(), Some(media(4)));
        assert!(schedule.part_data(media(1)).is_none());
        assert!(schedule.part_data(media(4)).is_none());
        let range = |part| schedule.part_data(part).unwrap().location.range.clone();
        assert_eq!(range(media(2)), Some(24..=33));
        assert_eq!(range(media(3)), Some(34..=43));

        // Bytes count the init, then the stream's own media.
        assert_eq!(schedule.prefix_len(media(2)), Some(4));
        assert_eq!(schedule.prefix_len(media(3)), Some(14));
        assert_eq!(schedule.total_len(), Some(24));
        assert_eq!(schedule.resolve_cursor(15), Some((media(3), 1)));
        assert_eq!(schedule.resolve_cursor(99), Some((media(3), 10)));
    }

    #[test]
    fn the_window_and_downloads_follow_numbered_segments() {
        let store = Store::default();
        let mut schedule = schedule_from(&four_seconds(), 2, &store);
        assert_eq!(schedule.window, n(2)..n(4));

        // Keep nothing behind and one second ahead of the last segment.
        schedule.set_retention(ahead(1), supply(&store));
        let change = schedule.move_cursor(media(3), supply(&store));
        assert_eq!(schedule.window, n(3)..n(4));
        assert_eq!(change.released.len(), 1);
        assert!(schedule.keeps(media(3)) && !schedule.keeps(media(2)));
        assert_eq!(schedule.kept().len(), 2);

        // The init comes first, then the window from the cursor.
        assert_eq!(schedule.next_get(EPOCH).unwrap().0, Part::Init);
        schedule.init.set_body(b"init");
        assert_eq!(schedule.next_get(EPOCH).unwrap().0, media(3));
    }

    #[test]
    fn a_request_is_settled_once_the_reader_takes_its_body() {
        let store = Store::default();
        let mut schedule = schedule_from(&four_seconds(), 0, &store);
        schedule.init.set_body(b"init");
        schedule.set_retention(ahead(0), supply(&store));

        // The reader asks for segment 0 while it is already downloading.
        schedule.move_cursor(media(0), supply(&store));
        schedule.request(media(0));
        schedule
            .part_data(media(0))
            .unwrap()
            .set_body(b"0123456789");

        // It takes the body and moves on, which drops it.
        assert!(schedule.checked_body(media(0)).is_some());
        let change = schedule.move_cursor(media(1), supply(&store));
        store.release(&change.released, true);
        assert!(schedule.missing_body(media(0)));

        // The GET worker fetches the new cursor's segment, not that one again.
        assert_eq!(schedule.next_get(EPOCH).unwrap().0, media(1));
    }

    #[test]
    fn an_untimed_schedule_keeps_everything() {
        let store = Store::default();
        let untimed = Segments::new("init", ["a", "b", "c"]);
        let mut schedule = retained_from(&untimed, 0, ahead(0), &store);
        assert_eq!(added(&schedule), n(0)..n(3));
        schedule.move_cursor(media(2), supply(&store));
        assert_eq!(schedule.window, n(0)..n(3));
    }

    #[test]
    fn a_bounded_window_adds_only_what_it_keeps() {
        let store = Store::default();
        let mut schedule = retained_from(&four_unsized(), 0, ahead(1), &store);
        assert_eq!(schedule.window, n(0)..n(1));
        assert_eq!(added(&schedule), n(0)..n(1));
        assert_eq!(store.len(), 2, "the init and the first segment");
        assert_eq!(schedule.kept().len(), 2);
        assert_eq!(schedule.end(), Some(media(4)));

        // Moving on adds what enters the window, and acquires it.
        let change = schedule.move_cursor(media(2), supply(&store));
        assert_eq!(schedule.window, n(2)..n(3));
        assert_eq!(added(&schedule), n(0)..n(3));
        let acquired: Vec<_> = change.acquired.iter().map(|data| &data.location).collect();
        assert_eq!(acquired, [&SegmentLocation::new("c")]);

        // Moving back adds nothing; segments stay added.
        schedule.move_cursor(media(0), supply(&store));
        assert_eq!(added(&schedule), n(0)..n(3));
    }

    #[test]
    fn byte_sizes_add_segments_to_the_end() {
        // Ranged segments have known sizes, but the end needs them all added.
        let store = Store::default();
        let mut schedule = retained_from(&four_seconds(), 0, ahead(0), &store);
        assert_eq!(added(&schedule), n(0)..n(1));
        assert_eq!(schedule.total_len(), None);
        assert_eq!(schedule.prefix_len(media(1)), Some(14));
        assert_eq!(schedule.prefix_len(media(2)), None);
        assert_eq!(schedule.resolve_cursor(30), None);
        assert_eq!(schedule.first_unknown_size(supply(&store)), None);
        assert_eq!(added(&schedule), n(0)..n(4));
        assert_eq!(schedule.total_len(), Some(44));
        assert_eq!(schedule.resolve_cursor(30), Some((media(2), 6)));
        assert_eq!(schedule.resolve_cursor(99), Some((media(3), 10)));

        // Unknown sizes are added one at a time, as HEADs learn them.
        let store = Store::default();
        let mut schedule = retained_from(&four_unsized(), 0, ahead(0), &store);
        let mut probed = Vec::new();
        schedule.init.set_body(b"init");
        while let Some((part, data)) = schedule.next_head(supply(&store)) {
            assert_eq!(added(&schedule).end, part.number().unwrap().next());
            store.accept_probe(&data, 5);
            probed.push(part);
        }
        assert_eq!(probed, [media(0), media(1), media(2), media(3)]);
        assert_eq!(schedule.total_len(), Some(24));
    }

    #[test]
    fn a_fetch_beyond_the_added_segments_adds_them() {
        let store = Store::default();
        let mut schedule = retained_from(&four_unsized(), 1, ahead(0), &store);
        assert_eq!(added(&schedule), n(1)..n(2));
        let location = |data: Option<&Arc<SegmentData>>| data.map(|data| data.location.url.clone());
        assert_eq!(
            location(schedule.materialise(media(3), supply(&store))),
            Some("d".to_owned())
        );
        assert_eq!(added(&schedule), n(1)..n(4));
        assert_eq!(
            location(schedule.materialise(media(4), supply(&store))),
            None
        );
        assert_eq!(
            location(schedule.materialise(Part::Init, supply(&store))),
            Some("init".to_owned())
        );

        // Materializing earlier media preserves representation numbering.
        assert_eq!(
            location(schedule.materialise(media(0), supply(&store))),
            Some("a".to_owned())
        );
        assert_eq!(added(&schedule), n(0)..n(4));
        assert_eq!(
            location(schedule.part_data(media(2))),
            Some("c".to_owned())
        );

        // Added segments outside the window are not kept.
        assert!(!schedule.keeps(media(3)) && !schedule.keeps(media(0)));
        assert_eq!(schedule.kept().len(), 2);
    }

    #[test]
    fn a_cursor_moves_from_the_init_to_the_first_segment() {
        let mut cursor = Cursor::start(n(2));
        assert_eq!(
            cursor.take_rest(Bytes::from_static(b"init")).unwrap(),
            "init"
        );
        assert_eq!(cursor.part, media(2));
        assert_eq!(cursor.take_rest(Bytes::from_static(b"c")).unwrap(), "c");
        assert_eq!(cursor.part, media(3));
        assert_eq!(cursor.byte_position, Some(5));
    }

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// The segments of a live representation available from the epoch,
    /// addressed by `template`.
    fn live(template: &str) -> Segments {
        live_with("", template)
    }

    /// Builds live segments addressed by `template`, with availability timed
    /// from the Unix epoch and extra MPD `attributes`.
    fn live_with(attributes: &str, template: &str) -> Segments {
        let xml = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                    availabilityStartTime="1970-01-01T00:00:00Z" {attributes}>
                 <Period start="PT0S"><AdaptationSet mimeType="audio/mp4">
                   <Representation id="a" bandwidth="1">{template}</Representation>
                 </AdaptationSet></Period>
               </MPD>"#
        );
        let manifest = crate::DashManifest::new(xml)
            .and_then(|manifest| manifest.with_base_url("https://cdn.test/"))
            .unwrap();
        Segments::indexed(SegmentLocation::new("init"), manifest.index_for_test("a"))
    }

    /// One-second segments without end, numbered from 1.
    fn live_template() -> Segments {
        live(r#"<SegmentTemplate initialization="i" media="$Number$" duration="1"/>"#)
    }

    /// The one-second segments a timeline from `start` seconds lists `count` of.
    fn live_timeline(start: u64, count: u64) -> Segments {
        live(&format!(
            r#"<SegmentTemplate initialization="i" media="$Time$">
                 <SegmentTimeline><S t="{start}" d="1" r="{}"/></SegmentTimeline>
               </SegmentTemplate>"#,
            count - 1
        ))
    }

    /// Inputs that add segments with data in `store`, at `secs` seconds.
    fn supply_at(store: &Store, secs: u64) -> Supply<'_> {
        Supply {
            store,
            now: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    /// A schedule of live `segments` from the media segment numbered
    /// `first`, keeping everything, at `secs` seconds.
    fn live_schedule(segments: &Segments, first: u64, store: &Store, secs: u64) -> Schedule {
        let init = store.data(segments.init().clone());
        let index = segments.index().clone();
        let (retention, role) = (Retention::default(), MediaRole::default());
        Schedule::new(
            init,
            index,
            n(first),
            retention,
            role,
            Duration::ZERO,
            supply_at(store, secs),
        )
    }

    /// Completes all downloads the schedule selects at second `secs`.
    fn fetch_all(schedule: &mut Schedule, secs: u64) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        while let Some((part, data)) = schedule.take_get(now) {
            data.set_body(b"body");
            schedule.fetched(part);
        }
    }

    #[test]
    fn a_live_schedule_adds_segments_as_they_become_available() {
        let store = Store::default();
        let mut schedule = live_schedule(&live_template(), 1, &store, 3);
        assert_eq!(added(&schedule), n(1)..n(4), "ended by three seconds");
        assert_eq!(schedule.end(), None);
        assert!(!schedule.past_end(media(1_000)));
        assert_eq!(schedule.total_len(), None);

        // With nothing to fetch, wait for the next to end.
        fetch_all(&mut schedule, 3);
        let (change, idle) = schedule.advance(supply_at(&store, 3));
        assert!(change.is_empty());
        assert_eq!(idle, Idle::Wait(Some(SystemTime::UNIX_EPOCH + secs(4))));

        // Later, the window takes in what has ended since.
        let (change, _) = schedule.advance(supply_at(&store, 5));
        assert_eq!(change.acquired.len(), 2);
        assert_eq!(added(&schedule), n(1)..n(6));
        assert!(schedule.next_get(SystemTime::UNIX_EPOCH + secs(5)).is_some());

        // A reader at a segment not yet available waits for it.
        fetch_all(&mut schedule, 5);
        assert!(schedule.read_step(media(6), supply_at(&store, 5)).is_none());
        assert!(matches!(
            schedule.read_step(media(5), supply_at(&store, 5)),
            Some(ReadStep::Ready(_))
        ));
    }

    #[test]
    fn sizes_and_offsets_stop_at_the_live_edge() {
        let store = Store::default();
        let mut schedule = live_schedule(&live_template(), 1, &store, 3);
        schedule.init.set_body(b"init");
        let mut probed = Vec::new();
        while let Some((part, data)) = schedule.next_head(supply_at(&store, 3)) {
            store.accept_probe(&data, 5);
            probed.push(part);
        }
        assert_eq!(probed, [media(1), media(2), media(3)]);
        assert_eq!(schedule.resolve_cursor(4 + 5 * 3), None, "past the edge");
        assert_eq!(schedule.resolve_cursor(4 + 5), Some((media(2), 0)));
    }

    #[test]
    fn a_refreshed_index_keeps_the_spans_of_added_segments() {
        let store = Store::default();
        let old = live_timeline(0, 5);
        let mut schedule = live_schedule(&old, 1, &store, 5);
        assert_eq!(added(&schedule), n(1)..n(6));

        // The refresh lists from three seconds on.
        let new = Arc::try_unwrap(live_timeline(3, 5).index)
            .unwrap()
            .aligned_to(old.index())
            .unwrap();
        assert_eq!(new.first(), n(4));
        schedule.replace_index(Arc::new(new), supply_at(&store, 7));
        assert_eq!(schedule.span(media(1)), Some(secs(0)..secs(1)));
        assert_eq!(added(&schedule), n(1)..n(8));
        assert_eq!(
            schedule.part_data(media(7)).unwrap().location,
            SegmentLocation::new("https://cdn.test/6")
        );
    }

    #[test]
    fn a_live_timeline_that_runs_out_asks_for_a_refresh_or_ends() {
        let store = Store::default();
        let mut schedule = live_schedule(&live_timeline(0, 5), 1, &store, 9);
        fetch_all(&mut schedule, 9);
        let (_, idle) = schedule.advance(supply_at(&store, 9));
        assert_eq!(idle, Idle::Refresh);
        assert!(schedule.read_step(media(6), supply_at(&store, 9)).is_none());

        // No refresh will list more: the stream ends after the last.
        schedule.finish_index();
        let (_, idle) = schedule.advance(supply_at(&store, 9));
        assert_eq!(idle, Idle::Ended);
        assert_eq!(schedule.end(), Some(media(6)));
        assert!(matches!(
            schedule.read_step(media(6), supply_at(&store, 9)),
            Some(ReadStep::End)
        ));
    }

    /// Builds unbounded one-second segments in a five-second live window.
    fn windowed_template() -> Segments {
        live_with(
            r#"timeShiftBufferDepth="PT5S""#,
            r#"<SegmentTemplate initialization="i" media="$Number$" duration="1"/>"#,
        )
    }

    #[test]
    fn live_segments_leave_the_front_with_their_byte_offsets() {
        let store = Store::default();
        let mut schedule = live_schedule(&windowed_template(), 6, &store, 10);
        schedule.init.set_body(b"init");
        fetch_all(&mut schedule, 10);
        assert_eq!(added(&schedule), n(6)..n(11));
        assert_eq!(schedule.prefix_len(media(8)), Some(4 + 4 + 4));

        // Advancing the cursor trims expired entries from the schedule front.
        let change = schedule.move_cursor(media(9), supply_at(&store, 13));
        assert_eq!(added(&schedule), n(9)..n(14));
        let trimmed: Vec<_> = change.trimmed.iter().map(|data| data.location.url.clone()).collect();
        assert_eq!(trimmed, ["https://cdn.test/6", "https://cdn.test/7", "https://cdn.test/8"]);

        // Later byte offsets retain the removed prefix, but offsets within it cannot resolve.
        assert_eq!(schedule.prefix_len(media(9)), Some(4 + 3 * 4));
        assert_eq!(schedule.prefix_len(media(8)), None);
        assert_eq!(schedule.resolve_cursor(4 + 3 * 4 + 1), Some((media(9), 1)));
        assert_eq!(schedule.resolve_cursor(5), None);
        assert_eq!(schedule.resolve_cursor(1), Some((Part::Init, 1)));
    }

    #[test]
    fn a_reader_behind_the_window_keeps_only_its_own_segment() {
        let store = Store::default();
        let mut schedule = live_schedule(&windowed_template(), 6, &store, 10);
        schedule.init.set_body(b"init");
        fetch_all(&mut schedule, 10);
        schedule.move_cursor(media(7), supply_at(&store, 10));

        // The cursor pins segment 7 after the live window has moved past it.
        let change = schedule.advance(supply_at(&store, 30)).0;
        store.release(&change.released, true);
        assert!(schedule.keeps(media(7)), "the segment it reads");
        assert!(!schedule.keeps(media(8)) && !schedule.keeps(media(26)));
        assert_eq!(change.released.len(), 4, "segments 6 and 8 to 10");
        assert!(change.trimmed.iter().any(|data| data.location.url.ends_with("/6")));
        assert_eq!(added(&schedule), n(7)..n(11), "none of the expired ones after");

        // The pinned body remains readable; its expired successor is not downloaded.
        assert!(matches!(
            schedule.read_step(media(7), supply_at(&store, 30)),
            Some(ReadStep::Ready(_))
        ));
        schedule.request(media(8));
        let fetch = schedule.take_get(SystemTime::UNIX_EPOCH + secs(30));
        assert_eq!(fetch.map(|(part, _)| part), None);
        assert!(matches!(
            schedule.read_step(media(8), supply_at(&store, 30)),
            Some(ReadStep::Behind { available }) if available == (secs(25)..secs(30))
        ));
    }

    #[test]
    fn a_failed_fetch_of_an_expired_segment_does_not_end_the_stream() {
        let store = Store::default();
        let mut schedule = live_schedule(&windowed_template(), 6, &store, 10);
        let failure = || Err(Error::Initialization("404".to_owned()));
        schedule.finish_get(media(6), failure(), SystemTime::UNIX_EPOCH + secs(30));
        assert!(schedule.error.is_none());
        schedule.finish_get(media(9), failure(), SystemTime::UNIX_EPOCH + secs(10));
        assert!(schedule.error.is_some(), "one still in the window does");
    }

    #[test]
    fn a_static_schedule_ends_when_fetched() {
        let store = Store::default();
        let mut schedule = schedule_from(&four_seconds(), 0, &store);
        fetch_all(&mut schedule, 0);
        let (change, idle) = schedule.advance(supply(&store));
        assert!(change.is_empty());
        assert_eq!(idle, Idle::Ended);
        assert!(matches!(
            schedule.read_step(media(4), supply(&store)),
            Some(ReadStep::End)
        ));
    }

    #[test]
    fn a_static_reader_ends_where_nothing_more_will_download() {
        // Once downloading is complete, a missing segment cannot become readable.
        let store = Store::default();
        let mut schedule = retained_from(&four_seconds(), 0, ahead(0), &store);
        schedule.init.set_body(b"init");
        schedule.complete = true;
        assert!(matches!(
            schedule.read_step(media(0), supply(&store)),
            Some(ReadStep::End)
        ));

        // While downloading may still provide the segment, the reader waits.
        schedule.complete = false;
        assert!(schedule.read_step(media(0), supply(&store)).is_none());
    }
}
