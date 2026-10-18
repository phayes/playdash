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
    /// Media duration to retain behind the current segment's start.
    ///
    /// Older segments are released once no other stream sharing the cache retains them;
    /// seeking back downloads them again. `None` retains everything behind the position.
    pub behind: Option<Duration>,

    /// Media duration to download ahead of the current segment's start.
    ///
    /// Downloading pauses when the retained window reaches this limit and resumes as
    /// playback advances. `None` allows downloading through the end.
    pub ahead: Option<Duration>,
}

/// The change from moving a stream's retention window: the segments it now
/// keeps, and the ones it let go.
#[derive(Debug, Default)]
pub(crate) struct WindowChange {
    pub(crate) acquired: Vec<Arc<SegmentData>>,
    pub(crate) released: Vec<Arc<SegmentData>>,
}

impl WindowChange {
    /// Whether the window kept the same segments.
    pub(crate) fn is_empty(&self) -> bool {
        self.acquired.is_empty() && self.released.is_empty()
    }
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
    /// The part after this one, in a stream whose first media segment is
    /// numbered `first`.
    fn next(self, first: SegmentNumber) -> Self {
        match self {
            Part::Init => Part::Media(first),
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

    /// The number of this media segment, or `first` for the initialization
    /// segment.
    fn number_or(self, first: SegmentNumber) -> SegmentNumber {
        match self {
            Part::Init => first,
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
    /// Number of the stream's first media segment, the first in `media`.
    first: SegmentNumber,
    /// Media segments added so far, in timeline order, numbered on from
    /// `first`.
    media: VecDeque<Scheduled>,
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
}

impl Schedule {
    /// A schedule for `init` and the media segments of `index` from the one
    /// numbered `first` on, keeping as much as `retention` says. Each media
    /// segment added gets `role`. Retention keeps everything of an untimed
    /// index.
    pub(crate) fn new(
        init: Arc<SegmentData>,
        index: Arc<MediaIndex>,
        first: SegmentNumber,
        retention: Retention,
        role: MediaRole,
        supply: Supply,
    ) -> Self {
        let mut schedule = Self {
            init,
            index,
            index_final: false,
            role,
            first,
            media: VecDeque::new(),
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
        };

        // Add the window's segments, and see whether any is still to fetch.
        let window = schedule.window_at(Part::Init, supply.now);
        schedule.extend_to(window.end, supply);
        schedule.complete = !schedule.missing_body(Part::Init)
            && each(window.clone()).all(|number| !schedule.missing_body(Part::Media(number)));
        schedule.window = window;
        schedule
    }

    /// The number of the first media segment.
    pub(crate) fn first(&self) -> SegmentNumber {
        self.first
    }

    /// The number just past the last media segment added.
    fn frontier(&self) -> SegmentNumber {
        self.first.plus(self.media.len())
    }

    /// Adds available index entries up to, but not including, `end`.
    fn extend_to(&mut self, end: SegmentNumber, supply: Supply) {
        let end = end.min(self.horizon(supply.now));
        while self.frontier() < end {
            let Some(segment) = self.index.segment(self.frontier()) else {
                break;
            };
            let data = supply.store.data(segment.location);
            self.role.assign(&data);
            self.media.push_back(Scheduled {
                data,
                span: segment.span,
            });
        }
    }

    /// Returns one past the last segment that may be added at `now`.
    fn horizon(&self, now: SystemTime) -> SegmentNumber {
        self.index.available(now).end.max(self.first)
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
        self.media.get(number.offset_from(self.first)?)
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

    /// Positions in `media` of the segments numbered in `range`.
    fn slots(&self, range: &Range<SegmentNumber>) -> Range<usize> {
        let slot = |number: SegmentNumber| {
            number
                .offset_from(self.first)
                .unwrap_or(0)
                .min(self.media.len())
        };
        slot(range.start)..slot(range.end)
    }

    /// The data of the segment at `part`, or `None` for a media segment not
    /// added.
    pub(crate) fn part_data(&self, part: Part) -> Option<&Arc<SegmentData>> {
        match part {
            Part::Init => Some(&self.init),
            Part::Media(number) => self.segment(number),
        }
    }

    /// The data of the segment at `part`, adding media segments up to it
    /// first. `None` before the first media segment or past the end.
    pub(crate) fn materialise(&mut self, part: Part, supply: Supply) -> Option<&Arc<SegmentData>> {
        if let Part::Media(number) = part {
            self.extend_to(number.next(), supply);
        }
        self.part_data(part)
    }

    /// Every segment added, in byte order, with its part.
    fn parts(&self) -> impl Iterator<Item = (Part, &Arc<SegmentData>)> {
        std::iter::once((Part::Init, &self.init)).chain(
            self.media
                .iter()
                .enumerate()
                .map(|(slot, scheduled)| (Part::Media(self.first.plus(slot)), &scheduled.data)),
        )
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
            false => Some(self.index.end()?.max(self.first)),
        }
    }

    /// Whether `part` is at or past the end of the stream.
    pub(crate) fn past_end(&self, part: Part) -> bool {
        self.end().is_some_and(|end| part >= end)
    }

    /// The segments the stream keeps now: the initialization segment and
    /// its window.
    pub(crate) fn kept(&self) -> Vec<Arc<SegmentData>> {
        let window = self
            .media
            .range(self.slots(&self.window))
            .map(|scheduled| &scheduled.data);
        std::iter::once(&self.init).chain(window).cloned().collect()
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
            Part::Media(number) => self.window.contains(&number),
        }
    }

    /// Recomputes the window for the cursor, adding the segments it reaches,
    /// and tells which segments entered and left it.
    fn rewindow(&mut self, supply: Supply) -> WindowChange {
        let window = self.window_at(self.cursor, supply.now);
        self.extend_to(window.end, supply);
        self.feed_buffers(supply);
        if window == self.window {
            return WindowChange::default();
        }
        let old = std::mem::replace(&mut self.window, window.clone());
        let pick = |range: &Range<SegmentNumber>, other: &Range<SegmentNumber>| -> Vec<_> {
            each(range.clone())
                .filter(|number| !other.contains(number))
                .filter_map(|number| self.segment(number).cloned())
                .collect()
        };
        let change = WindowChange {
            acquired: pick(&window, &old),
            released: pick(&old, &window),
        };
        if each(window).any(|number| self.missing_body(Part::Media(number))) {
            self.complete = false;
        }
        change
    }

    /// Computes the available retention window around `cursor` at `now`.
    ///
    /// The window always includes the cursor's media segment. An initialization
    /// cursor is treated as the first media segment.
    fn window_at(&self, cursor: Part, now: SystemTime) -> Range<SegmentNumber> {
        let end = self.horizon(now);
        let all = self.first..end;
        if self.index.timing().is_none() || self.retention == Retention::default() {
            return all;
        }

        // Past the last segment, the position is the end of the media.
        let cursor = cursor.number_or(self.first).max(self.first);
        let playhead = match self.index.span(cursor) {
            Some(span) => span.start,
            None => end
                .checked_prev()
                .filter(|&last| last >= self.first)
                .and_then(|last| self.index.span(last))
                .map_or(Duration::ZERO, |span| span.end),
        };

        // As far behind and ahead of the playhead as retention says.
        let start = match self.retention.behind {
            Some(behind) => self
                .index
                .ending_after(playhead.saturating_sub(behind))
                .map_or(all.start, |start| start.max(all.start)),
            None => all.start,
        };
        let until = match self.retention.ahead {
            Some(ahead) => self
                .index
                .starting_from(playhead.saturating_add(ahead))
                .map_or(end, |until| until.max(all.start)),
            None => end,
        };
        // Clamp a live cursor that is waiting beyond the current availability horizon.
        let until = until.max(cursor.next()).min(end);
        start.min(cursor).min(until)..until
    }

    pub(crate) fn all_sizes_known(&self) -> bool {
        self.total_len().is_some()
    }

    /// Whether the segment at `part` exists and its body is not held.
    pub(crate) fn missing_body(&self, part: Part) -> bool {
        self.part_data(part).is_some_and(|data| !data.has_body())
    }

    /// The first segment whose size is unknown, adding media segments until
    /// one is found.
    pub(crate) fn first_unknown_size(&mut self, supply: Supply) -> Option<Part> {
        // Among the segments added so far.
        if let Some((part, _)) = self.parts().find(|(_, data)| data.size().is_none()) {
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

    /// The next segment the GET worker should fetch: the priority, else the
    /// initialization segment if missing, else the first missing one in the
    /// window from the cursor on, else one in the window before the cursor.
    pub(crate) fn next_get(&mut self) -> Option<(Part, Arc<SegmentData>)> {
        let Range { start, end } = self.window;
        let cursor = self.cursor.number_or(self.first).clamp(start, end);
        let part = self
            .priority
            .take()
            .filter(|&priority| self.missing_body(priority))
            .or_else(|| {
                let media = each(cursor..end).chain(each(start..cursor));
                std::iter::once(Part::Init)
                    .chain(media.map(Part::Media))
                    .find(|&part| self.missing_body(part))
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

    /// Sum of the sizes of every segment before `part`, if all are added and
    /// known.
    pub(crate) fn prefix_len(&self, part: Part) -> Option<u64> {
        if part > Part::Media(self.frontier()) {
            return None;
        }
        self.parts()
            .take_while(|&(before, _)| before < part)
            .map(|(_, data)| data.size())
            .sum()
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
        let known: u64 = self.parts().filter_map(|(_, data)| data.size()).sum();
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

    /// Maps an absolute byte offset onto a part and an offset into it once
    /// contiguous sizes covering that offset are known. An offset past the
    /// end lands at the end of the last segment, once every segment is added.
    pub(crate) fn resolve_cursor(&self, target: u64) -> Option<(Part, usize)> {
        let mut acc = 0u64;
        for (part, data) in self.parts() {
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
            .media
            .len()
            .checked_sub(1)
            .map_or(Part::Init, |slot| Part::Media(self.first.plus(slot)));
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
}

/// A stream reader's position: a part, an offset into it, and the absolute
/// byte offset when the sizes before it are known.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    pub(crate) part: Part,
    pub(crate) offset_in_segment: usize,
    pub(crate) byte_position: Option<u64>,
    /// Number of the stream's first media segment, which follows the
    /// initialization segment.
    first: SegmentNumber,
}

impl Cursor {
    /// The start of the initialization segment of a stream whose first media
    /// segment is numbered `first`.
    pub(crate) fn start(first: SegmentNumber) -> Self {
        Self {
            part: Part::Init,
            offset_in_segment: 0,
            byte_position: Some(0),
            first,
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

    fn next_segment(&mut self) {
        self.part = self.part.next(self.first);
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

    /// The number of the media segment a seek to `time` lands in.
    pub(crate) fn time_target(&self, time: Duration) -> io::Result<SegmentNumber> {
        let invalid = |reason: &str| io::Error::new(io::ErrorKind::InvalidInput, reason);
        if self.index.timing().is_none() {
            return Err(invalid("MPEG stream has no media timeline"));
        }
        let number = self
            .index
            .number_at(time)
            .ok_or_else(|| invalid("MPEG stream has no media segments"))?;
        match number < self.first {
            true => Err(invalid("MPEG stream starts after the requested media time")),
            false => Ok(number),
        }
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

    /// Tells `feed` of the stream's timed media segments, with their data in
    /// `supply`'s store and the presentation time each spans: those from its
    /// first on that there are at `supply`'s time, and later the live ones as
    /// they become available.
    pub(crate) fn subscribe_buffer(&mut self, feed: BufferFeed, supply: Supply) {
        feed.push(self.timed_media(supply.store, supply.now));
        if self.index.is_live() {
            self.buffers.push((feed, self.horizon(supply.now)));
        }
    }

    /// The timed media segments from the stream's first on that there are at
    /// `now`, with their data in `store` and the presentation time each
    /// spans. Empty for untimed segments.
    fn timed_media(
        &self,
        store: &Store,
        now: SystemTime,
    ) -> Vec<(Arc<SegmentData>, Range<Duration>)> {
        timed_media(&self.index, self.first..self.horizon(now), store)
    }

    /// Publishes newly available live segments and removes dropped subscribers.
    fn feed_buffers(&mut self, supply: Supply) {
        if self.buffers.is_empty() {
            return;
        }
        let horizon = self.horizon(supply.now);
        let index = &self.index;
        self.buffers.retain_mut(|(feed, fed)| {
            if *fed >= horizon {
                return feed.is_alive();
            }
            let media = timed_media(index, *fed..horizon, supply.store);
            *fed = horizon;
            feed.push(media)
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
        let init_len = self.init.size().unwrap_or(0);
        self.index
            .estimate_len(self.first, init_len, bitrate_bps)
            .ok_or_else(|| {
                Error::Initialization(
                    "symphonia_compat requires a non-zero bitrate and media segments".to_owned(),
                )
            })
    }
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

    /// The next segment the GET worker should fetch. `None` when it has
    /// nothing to fetch for now, which marks the schedule complete.
    pub(crate) fn take_get(&mut self) -> Option<(Part, Arc<SegmentData>)> {
        let work = self.next_get();
        if work.is_none() {
            self.complete = true;
        }
        work
    }

    /// Records how the GET worker's fetch of the segment at `part` went: a
    /// failure ends the stream. Returns whether the stream keeps the segment.
    pub(crate) fn finish_get(&mut self, part: Part, result: Result<(), Error>) -> bool {
        match result {
            Ok(()) => self.fetched(part),
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

    /// Whether a reader waiting on the segment at `part` can stop: its body
    /// is here, or nothing more will download for it. A live segment not yet
    /// added may still become available.
    pub(crate) fn settled(&self, part: Part) -> bool {
        if let Part::Media(number) = part
            && self.index.is_live()
            && !self.past_end(part)
            && number >= self.frontier()
        {
            return false;
        }
        !self.missing_body(part) || self.complete
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
        let (part, offset_in_segment) = self.resolve_cursor(target).ok_or_else(|| {
            io::Error::other(
                "MPEG stream cannot place the requested byte offset: HEAD requests failed to learn the sizes of the segments before it, which are not yet downloaded",
            )
        })?;
        let change = self.move_cursor(part, supply);
        self.request(part);
        let position = Position {
            byte: Some(target),
            time: self.time_at(part, accuracy_at(offset_in_segment)),
        };
        Ok((part, offset_in_segment, position, change))
    }

    /// Starts a seek to the media segment covering `time`: moves the cursor
    /// and the window there and asks for it first. Returns the segment's part
    /// and the window change.
    pub(crate) fn begin_time_seek(
        &mut self,
        time: Duration,
        supply: Supply,
    ) -> io::Result<(Part, WindowChange)> {
        let target = Part::Media(self.time_target(time)?);
        if self.past_end(target) {
            return Err(io::Error::other(
                "MPEG stream ended before the requested media time",
            ));
        }
        let change = self.move_cursor(target, supply);
        self.request(target);
        Ok((target, change))
    }

    /// Finishes a time seek to `target` once [`Self::settled`]: the position
    /// it landed on, at the start of the segment.
    pub(crate) fn land_time_seek(&self, target: Part) -> io::Result<Position> {
        if self.missing_body(target) {
            return Err(io::Error::other(
                "MPEG stream ended before the requested media time",
            ));
        }
        Ok(Position {
            byte: self.prefix_len(target),
            time: self.time_at(target, TimeAccuracy::Packet),
        })
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
        Schedule::new(init, index, n(first), retention, role, supply(store))
    }

    /// The numbers of the media segments `schedule` has added.
    fn added(schedule: &Schedule) -> Range<SegmentNumber> {
        schedule.first()..schedule.frontier()
    }

    #[test]
    fn time_target_lands_on_reported_segment_starts() {
        // A third of a second does not land on a whole nanosecond.
        let store = Store::default();
        let segments = timed_for_test("init", ["a", "b"], 3, &[1, 1]);
        let schedule = schedule_from(&segments, 0, &store);
        let second_start = segments.span(1).unwrap().start;
        let target = |time| schedule.time_target(time).unwrap();
        assert_eq!(target(second_start), n(1));
        assert_eq!(target(second_start - Duration::from_nanos(1)), n(0));
        assert_eq!(target(Duration::from_secs(5)), n(1));

        let empty = timed_for_test("init", Vec::<SegmentLocation>::new(), 3, &[]);
        let untimed = Segments::new("init", ["a"]);
        for segments in [empty, untimed] {
            let schedule = schedule_from(&segments, 0, &store);
            let error = schedule.time_target(Duration::ZERO).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn a_stream_from_partway_keeps_the_representation_times() {
        // Four one-second segments; the stream starts at the third.
        let segments = four_seconds();
        let store = Store::default();
        let schedule = schedule_from(&segments, 2, &store);
        let timed = schedule.timed_media(&store, SystemTime::UNIX_EPOCH);
        let scheduled: Vec<_> = timed
            .iter()
            .map(|(data, span)| (data.location.clone(), span.clone()))
            .collect();
        assert_eq!(
            scheduled,
            [
                (
                    SegmentLocation::with_range("file", 24..=33),
                    secs(2)..secs(3)
                ),
                (
                    SegmentLocation::with_range("file", 34..=43),
                    secs(3)..secs(4)
                ),
            ]
        );

        // They share their data with the schedule's.
        assert!(Arc::ptr_eq(
            &timed[0].0,
            schedule.part_data(media(2)).unwrap()
        ));
        let untimed = schedule_from(&Segments::new("init", ["a"]), 0, &store);
        assert!(
            untimed
                .timed_media(&store, SystemTime::UNIX_EPOCH)
                .is_empty()
        );

        // Seeks name the representation's time, and land on the stream's own segments.
        let target = |time| schedule.time_target(time);
        assert_eq!(target(secs(2)).unwrap(), n(2));
        assert_eq!(target(Duration::from_millis(3_500)).unwrap(), n(3));
        assert_eq!(
            target(Duration::from_millis(1_500)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

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
        assert_eq!(schedule.first(), n(2));
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
        assert_eq!(schedule.next_get().unwrap().0, Part::Init);
        schedule.init.set_body(b"init");
        assert_eq!(schedule.next_get().unwrap().0, media(3));
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
        assert_eq!(schedule.next_get().unwrap().0, media(1));
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
    fn a_fetch_past_the_added_segments_adds_them() {
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
            location(schedule.materialise(media(0), supply(&store))),
            None
        );
        assert_eq!(
            location(schedule.materialise(Part::Init, supply(&store))),
            Some("init".to_owned())
        );

        // Added segments outside the window are not kept.
        assert!(!schedule.keeps(media(3)));
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
        let xml = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                    availabilityStartTime="1970-01-01T00:00:00Z">
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
            supply_at(store, secs),
        )
    }

    /// Takes every GET the schedule has, as if each finished.
    fn fetch_all(schedule: &mut Schedule) {
        while let Some((part, data)) = schedule.take_get() {
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
        fetch_all(&mut schedule);
        let (change, idle) = schedule.advance(supply_at(&store, 3));
        assert!(change.is_empty());
        assert_eq!(idle, Idle::Wait(Some(SystemTime::UNIX_EPOCH + secs(4))));

        // Later, the window takes in what has ended since.
        let (change, _) = schedule.advance(supply_at(&store, 5));
        assert_eq!(change.acquired.len(), 2);
        assert_eq!(added(&schedule), n(1)..n(6));
        assert!(schedule.next_get().is_some());

        // A reader at a segment not yet available waits for it.
        fetch_all(&mut schedule);
        assert!(!schedule.settled(media(6)));
        assert!(schedule.settled(media(5)));
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
        fetch_all(&mut schedule);
        let (_, idle) = schedule.advance(supply_at(&store, 9));
        assert_eq!(idle, Idle::Refresh);
        assert!(!schedule.settled(media(6)));

        // No refresh will list more: the stream ends after the last.
        schedule.finish_index();
        let (_, idle) = schedule.advance(supply_at(&store, 9));
        assert_eq!(idle, Idle::Ended);
        assert_eq!(schedule.end(), Some(media(6)));
        assert!(schedule.settled(media(6)));
    }

    #[test]
    fn a_static_schedule_ends_when_fetched() {
        let store = Store::default();
        let mut schedule = schedule_from(&four_seconds(), 0, &store);
        fetch_all(&mut schedule);
        let (change, idle) = schedule.advance(supply(&store));
        assert!(change.is_empty());
        assert_eq!(idle, Idle::Ended);
        assert!(schedule.settled(media(4)));
    }
}
