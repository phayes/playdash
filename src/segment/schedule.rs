//! State shared by the blocking and async `SegmentStream`s: positions, the download schedule
//! their workers follow, and the arithmetic of reading and seeking. Nothing here does I/O or
//! waits.

use super::store::SegmentData;
use super::{SegmentLocation, SegmentNumber, Segments};
use crate::error::Error;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, SeekFrom};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

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

/// A media segment in a stream's schedule: its data, and the presentation
/// time it spans when timed.
#[derive(Debug)]
pub(crate) struct Scheduled {
    pub(crate) data: Arc<SegmentData>,
    pub(crate) span: Option<Range<Duration>>,
}

/// The numbers from the start of `range` to just before its end.
fn each(range: Range<SegmentNumber>) -> impl DoubleEndedIterator<Item = SegmentNumber> {
    (range.start.0..range.end.0).map(SegmentNumber)
}

/// A stream's download schedule: the segments it wants, and how its workers
/// fetch them. Shared
/// between a stream and its GET and HEAD workers, under one lock.
#[derive(Debug)]
pub(crate) struct Schedule {
    /// Initialization segment, always kept.
    pub(crate) init: Arc<SegmentData>,
    /// Number of the first media segment in `media`.
    first: SegmentNumber,
    /// Media segments in timeline order, numbered on from `first`.
    media: VecDeque<Scheduled>,
    /// Every media segment has a span, so retention can place the window.
    timed: bool,
    /// When false, byte seeks wait on downloaded bodies instead of HEAD.
    pub(crate) head_available: bool,
    /// Background HEAD prefetch is wanted. Does not gate on-demand seek HEADs.
    pub(crate) eager_head: bool,
    /// A HEAD worker is alive, possibly finishing an in-flight request.
    pub(crate) head_worker_running: bool,
    /// Read/seek playhead. The GET worker fills at or after it first.
    pub(crate) cursor: Part,
    pub(crate) priority: Option<Part>,
    /// The GET worker has nothing left to fetch for now.
    pub(crate) complete: bool,
    pub(crate) error: Option<String>,
    retention: Retention,
    /// The media segments the stream keeps now: its retention window.
    pub(crate) window: Range<SegmentNumber>,
}

impl Schedule {
    /// A schedule for `init` and `media`, the media segments numbered on from
    /// `first`, each with the presentation time it spans when timed. Unless
    /// every one is timed, retention keeps everything.
    pub(crate) fn new(
        init: Arc<SegmentData>,
        first: SegmentNumber,
        media: Vec<(Arc<SegmentData>, Option<Range<Duration>>)>,
    ) -> Self {
        let media: VecDeque<Scheduled> = media
            .into_iter()
            .map(|(data, span)| Scheduled { data, span })
            .collect();
        let complete = init.has_body() && media.iter().all(|segment| segment.data.has_body());
        let timed = media.iter().all(|segment| segment.span.is_some());
        let window = first..first.plus(media.len());
        Self {
            init,
            first,
            media,
            timed,
            head_available: true,
            eager_head: false,
            head_worker_running: false,
            cursor: Part::Init,
            priority: None,
            complete,
            error: None,
            retention: Retention::default(),
            window,
        }
    }

    /// The number of the first media segment.
    pub(crate) fn first(&self) -> SegmentNumber {
        self.first
    }

    /// The scheduled media segment numbered `number`.
    fn segment(&self, number: SegmentNumber) -> Option<&Scheduled> {
        self.media.get(number.offset_from(self.first)?)
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

    /// The data of the segment at `part`, or `None` past the end.
    pub(crate) fn part_data(&self, part: Part) -> Option<&Arc<SegmentData>> {
        match part {
            Part::Init => Some(&self.init),
            Part::Media(number) => Some(&self.segment(number)?.data),
        }
    }

    /// Every segment in byte order, with its part.
    fn parts(&self) -> impl Iterator<Item = (Part, &Arc<SegmentData>)> {
        std::iter::once((Part::Init, &self.init)).chain(
            self.media
                .iter()
                .enumerate()
                .map(|(slot, segment)| (Part::Media(self.first.plus(slot)), &segment.data)),
        )
    }

    /// The media segments, in byte order.
    #[cfg(test)]
    pub(crate) fn media_data(&self) -> impl Iterator<Item = &Arc<SegmentData>> {
        self.media.iter().map(|segment| &segment.data)
    }

    /// The media segments, each with the presentation time it spans, if every
    /// one is timed.
    pub(crate) fn timed_media(&self) -> Option<Vec<(Arc<SegmentData>, Range<Duration>)>> {
        self.media
            .iter()
            .map(|segment| Some((segment.data.clone(), segment.span.clone()?)))
            .collect()
    }

    /// The part just past the last segment.
    pub(crate) fn end(&self) -> Part {
        Part::Media(self.first.plus(self.media.len()))
    }

    /// Whether `part` is at or past the end of the stream.
    pub(crate) fn past_end(&self, part: Part) -> bool {
        part >= self.end()
    }

    /// The segments the stream keeps now: the initialization segment and
    /// its window.
    pub(crate) fn kept(&self) -> Vec<Arc<SegmentData>> {
        let window = self.media.range(self.slots(&self.window));
        std::iter::once(&self.init)
            .chain(window.map(|segment| &segment.data))
            .cloned()
            .collect()
    }

    /// Changes the retention, and the window with it.
    pub(crate) fn set_retention(&mut self, retention: Retention) -> WindowChange {
        self.retention = retention;
        self.rewindow()
    }

    /// Moves the cursor to `part`, and the window with it.
    pub(crate) fn move_cursor(&mut self, part: Part) -> WindowChange {
        self.cursor = part;
        self.rewindow()
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

    /// Recomputes the window for the cursor, and tells which segments
    /// entered and left it.
    fn rewindow(&mut self) -> WindowChange {
        let window = self.window_at(self.cursor);
        if window == self.window {
            return WindowChange::default();
        }
        let old = std::mem::replace(&mut self.window, window.clone());
        let pick = |range: &Range<SegmentNumber>, other: &Range<SegmentNumber>| -> Vec<_> {
            each(range.clone())
                .filter(|number| !other.contains(number))
                .filter_map(|number| Some(self.segment(number)?.data.clone()))
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

    /// The window retention gives a cursor at `cursor`: as far behind and
    /// ahead of its media segment's start as `retention` says, and at least
    /// that segment itself. A cursor in the initialization segment counts
    /// as the first media segment.
    fn window_at(&self, cursor: Part) -> Range<SegmentNumber> {
        let len = self.media.len();
        let all = self.first..self.first.plus(len);
        if !self.timed || self.retention == Retention::default() {
            return all;
        }
        let span = |segment: &Scheduled| segment.span.clone().unwrap_or_default();

        // Past the last segment, the position is the end of the media.
        let cursor = cursor
            .number_or(self.first)
            .offset_from(self.first)
            .unwrap_or(0);
        let playhead = match self.media.get(cursor) {
            Some(segment) => span(segment).start,
            None => self
                .media
                .back()
                .map_or(Duration::ZERO, |last| span(last).end),
        };
        let start = match self.retention.behind {
            Some(behind) => {
                let from = playhead.saturating_sub(behind);
                self.media
                    .partition_point(|segment| span(segment).end <= from)
            }
            None => 0,
        };
        let end = match self.retention.ahead {
            Some(ahead) => {
                let until = playhead.saturating_add(ahead);
                self.media
                    .partition_point(|segment| span(segment).start < until)
            }
            None => len,
        };
        self.first.plus(start.min(cursor))..self.first.plus(end.max(cursor + 1).min(len))
    }

    pub(crate) fn all_sizes_known(&self) -> bool {
        self.total_len().is_some()
    }

    /// Whether the segment at `part` exists and its body is not held.
    pub(crate) fn missing_body(&self, part: Part) -> bool {
        self.part_data(part).is_some_and(|data| !data.has_body())
    }

    /// The first segment whose size is unknown.
    pub(crate) fn first_unknown_size(&self) -> Option<Part> {
        self.parts()
            .find(|(_, data)| data.size().is_none())
            .map(|(part, _)| part)
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
    pub(crate) fn next_head(&self) -> Option<(Part, Arc<SegmentData>)> {
        if !self.head_available {
            return None;
        }
        let part = self.first_unknown_size()?;
        Some((part, self.part_data(part)?.clone()))
    }

    /// Sum of the sizes of every segment before `part`, if all are known.
    pub(crate) fn prefix_len(&self, part: Part) -> Option<u64> {
        self.parts()
            .take_while(|&(before, _)| before < part)
            .map(|(_, data)| data.size())
            .sum()
    }

    pub(crate) fn total_len(&self) -> Option<u64> {
        self.prefix_len(self.end())
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
    /// end lands at the end of the last segment.
    pub(crate) fn resolve_cursor(&self, target: u64) -> Option<(Part, usize)> {
        let mut acc = 0u64;
        for (part, data) in self.parts() {
            let len = data.size()?;
            if target < acc + len {
                return Some((part, (target - acc) as usize));
            }
            acc += len;
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
/// [`SeekFrom::End`], the total length.
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
        SeekFrom::End(delta) => (total.unwrap_or(0), delta),
    };
    Ok(match delta >= 0 {
        true => base.saturating_add(delta as u64),
        false => base.saturating_sub(delta.unsigned_abs()),
    })
}

/// The media segments of `segments` numbered from `first` on, each with the
/// presentation time it spans when timed: what a stream from `first` schedules.
pub(crate) fn media_from(
    segments: &Segments,
    first: SegmentNumber,
) -> impl Iterator<Item = (&SegmentLocation, Option<Range<Duration>>)> {
    each(first..segments.numbers().end)
        .filter_map(|number| Some((segments.location_of(number)?, segments.span_of(number))))
}

/// The number of the media segment of `segments` covering `time`, where a
/// stream starting at a time begins, and that segment's start. A time at or
/// past the end starts at the last media segment.
pub(crate) fn start_at(
    segments: &Segments,
    time: Duration,
) -> Result<(SegmentNumber, Duration), Error> {
    segments
        .number_at(time)
        .and_then(|first| Some((first, segments.span_of(first)?.start)))
        .ok_or_else(|| {
            Error::Initialization(
                "MPEG stream needs timed media segments to start at a time".to_owned(),
            )
        })
}

/// The number of the media segment a seek to `time` lands in, for a stream
/// of `segments` whose first media segment is numbered `first`.
pub(crate) fn time_target(
    segments: &Segments,
    first: SegmentNumber,
    time: Duration,
) -> io::Result<SegmentNumber> {
    let invalid = |reason: &str| io::Error::new(io::ErrorKind::InvalidInput, reason);
    if segments.timing().is_none() {
        return Err(invalid("MPEG stream has no media timeline"));
    }
    let number = segments
        .number_at(time)
        .ok_or_else(|| invalid("MPEG stream has no media segments"))?;
    match number < first {
        true => Err(invalid("MPEG stream starts after the requested media time")),
        false => Ok(number),
    }
}

/// The start of the media segment at `part`, as a position of `accuracy`.
/// `None` for the initialization segment, which has no media time, and for
/// untimed segments.
pub(crate) fn time_at_segment(
    segments: &Segments,
    part: Part,
    accuracy: TimeAccuracy,
) -> Option<TimePosition> {
    let Part::Media(number) = part else {
        return None;
    };
    Some(TimePosition {
        timestamp: segments.span_of(number)?.start,
        accuracy,
    })
}

/// The end of the representation's media, as a packet-accurate position.
pub(crate) fn time_at_end(segments: &Segments) -> Option<TimePosition> {
    segments.end().map(|timestamp| TimePosition {
        timestamp,
        accuracy: TimeAccuracy::Packet,
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

/// The length a symphonia-compat stream reports before every size is known.
pub(crate) fn compat_estimate(
    segments: &Segments,
    first: SegmentNumber,
    init_len: u64,
    bitrate_bps: u32,
) -> Result<u64, Error> {
    if segments.timing().is_none() {
        return Err(Error::Initialization(
            "symphonia_compat requires a media timeline".to_owned(),
        ));
    }
    segments
        .from(first)
        .estimate_len(init_len, bitrate_bps)
        .ok_or_else(|| {
            Error::Initialization(
                "symphonia_compat requires a non-zero bitrate and media segments".to_owned(),
            )
        })
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

    /// A schedule of `segments` from the media segment numbered `first`.
    fn schedule_from(segments: &Segments, first: u64) -> Schedule {
        let init = Arc::new(SegmentData::new(segments.init().clone()));
        let media = media_from(segments, n(first))
            .map(|(location, span)| (Arc::new(SegmentData::new(location.clone())), span))
            .collect();
        Schedule::new(init, n(first), media)
    }

    #[test]
    fn time_target_lands_on_reported_segment_starts() {
        // A third of a second does not land on a whole nanosecond.
        let segments = timed_for_test("init", ["a", "b"], 3, &[1, 1]);
        let second_start = segments.span(1).unwrap().start;
        let target = |time| time_target(&segments, n(0), time).unwrap();
        assert_eq!(target(second_start), n(1));
        assert_eq!(target(second_start - Duration::from_nanos(1)), n(0));
        assert_eq!(target(Duration::from_secs(5)), n(1));

        let empty = timed_for_test("init", Vec::<SegmentLocation>::new(), 3, &[]);
        let error = time_target(&empty, n(0), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let untimed = Segments::new("init", ["a"]);
        let error = time_target(&untimed, n(0), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_stream_from_partway_keeps_the_representation_times() {
        // Four one-second segments; the stream starts at the third.
        let segments = four_seconds();
        let scheduled: Vec<_> = media_from(&segments, n(2)).collect();
        assert_eq!(
            scheduled,
            [
                (
                    &SegmentLocation::with_range("file", 24..=33),
                    Some(secs(2)..secs(3))
                ),
                (
                    &SegmentLocation::with_range("file", 34..=43),
                    Some(secs(3)..secs(4))
                ),
            ]
        );

        // Seeks name the representation's time, and land on the stream's own segments.
        let target = |time| time_target(&segments, n(2), time);
        assert_eq!(target(secs(2)).unwrap(), n(2));
        assert_eq!(target(Duration::from_millis(3_500)).unwrap(), n(3));
        assert_eq!(
            target(Duration::from_millis(1_500)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let at = |part| time_at_segment(&segments, part, TimeAccuracy::Packet);
        assert_eq!(at(Part::Init), None);
        assert_eq!(at(media(3)).unwrap().timestamp, secs(3));
        assert_eq!(at(media(4)), None);
        assert_eq!(time_at_end(&segments).unwrap().timestamp, secs(4));
        assert_eq!(start_at(&segments, secs(3)).unwrap(), (n(3), secs(3)));
    }

    #[test]
    fn a_schedule_addresses_segments_by_number() {
        let schedule = schedule_from(&four_seconds(), 2);
        assert_eq!(schedule.first(), n(2));
        assert_eq!(schedule.end(), media(4));
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
        let mut schedule = schedule_from(&four_seconds(), 2);
        assert_eq!(schedule.window, n(2)..n(4));

        // Keep nothing behind and one second ahead of the last segment.
        schedule.set_retention(Retention {
            behind: Some(Duration::ZERO),
            ahead: Some(secs(1)),
        });
        let change = schedule.move_cursor(media(3));
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
        let mut schedule = schedule_from(&four_seconds(), 0);
        schedule.init.set_body(b"init");
        schedule.set_retention(Retention {
            behind: Some(Duration::ZERO),
            ahead: Some(Duration::ZERO),
        });

        // The reader asks for segment 0 while it is already downloading.
        schedule.move_cursor(media(0));
        schedule.request(media(0));
        schedule
            .part_data(media(0))
            .unwrap()
            .set_body(b"0123456789");

        // It takes the body and moves on, which drops it.
        assert!(schedule.checked_body(media(0)).is_some());
        let change = schedule.move_cursor(media(1));
        store.release(&change.released, true);
        assert!(schedule.missing_body(media(0)));

        // The GET worker fetches the new cursor's segment, not that one again.
        assert_eq!(schedule.next_get().unwrap().0, media(1));
    }

    #[test]
    fn an_untimed_schedule_keeps_everything() {
        let mut schedule = schedule_from(&Segments::new("init", ["a", "b", "c"]), 0);
        schedule.set_retention(Retention {
            behind: Some(Duration::ZERO),
            ahead: Some(Duration::ZERO),
        });
        schedule.move_cursor(media(2));
        assert_eq!(schedule.window, n(0)..n(3));
        assert!(schedule.timed_media().is_none());
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
}
