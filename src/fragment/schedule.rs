//! State shared by the blocking and async `FragmentStream`s: positions, the download schedule
//! their workers follow, and the arithmetic of reading and seeking. Nothing here does I/O or
//! waits.

use super::store::FragmentData;
use super::{Fragment, Fragments, MediaTimeline};
use crate::error::Error;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, SeekFrom};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

/// Retention windows for a `FragmentStream`: the media time of a
/// representation it keeps around its position. The default keeps everything and downloads to
/// the end, which suits audio; long video wants both bounded.
///
/// Both windows are measured from the start of the fragment under the
/// stream's position, and that fragment is always kept. The initialization
/// segment is always kept too. Retention needs the stream's
/// [`MediaTimeline`]; a stream without one keeps everything.
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
    /// Media time to keep behind the position. Fragments that end before it
    /// are dropped from the cache once no other stream sharing it keeps them,
    /// and seeking back to one downloads it again. `None` keeps everything.
    pub behind: Option<Duration>,

    /// Media time to download ahead of the position. The stream stops
    /// downloading once it is this far ahead, and resumes as the position
    /// moves. `None` downloads to the end.
    pub ahead: Option<Duration>,
}

/// The change from moving a stream's retention window: the fragments it now
/// keeps, and the ones it let go.
#[derive(Debug, Default)]
pub(crate) struct WindowChange {
    pub(crate) acquired: Vec<Arc<FragmentData>>,
    pub(crate) released: Vec<Arc<FragmentData>>,
}

/// Byte and media-time information for a stream position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Absolute offset in the concatenated init + media byte stream.
    ///
    /// This is `None` when preceding fragment sizes are not yet known.
    pub byte: Option<u64>,

    /// Media timestamp corresponding to the position, when a timeline applies.
    pub time: Option<TimePosition>,
}

/// Media timestamp of a stream position, and how precisely it describes that position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimePosition {
    /// Media timestamp of the position. It may differ from a time a seek asked for;
    /// compare the two to see by how much.
    pub timestamp: Duration,

    /// Precision of `timestamp` for the position.
    pub accuracy: TimeAccuracy,
}

/// Granularity of a reported media timestamp: how precisely it describes a stream
/// position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeAccuracy {
    /// To the media fragment: the timestamp is the start of the fragment the position
    /// is inside.
    Fragment,

    /// To the packet: the timestamp is that of the packet at the position.
    Packet,
}

/// A place in a stream's bytes: the initialization fragment, or a media
/// fragment by its index in playback order. `Media(len)` is the end of the
/// stream. Ordered as the bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Part {
    Init,
    Media(usize),
}

impl Part {
    /// The part after this one.
    fn next(self) -> Self {
        match self {
            Part::Init => Part::Media(0),
            Part::Media(index) => Part::Media(index + 1),
        }
    }

    /// The index of this media fragment, or of the first one for the
    /// initialization fragment.
    fn media_index(self) -> usize {
        match self {
            Part::Init => 0,
            Part::Media(index) => index,
        }
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Part::Init => f.write_str("the initialization fragment"),
            Part::Media(index) => write!(f, "media fragment {index}"),
        }
    }
}

/// A stream's download schedule: the fragments it wants, and how its workers
/// fetch them. Shared
/// between a stream and its GET and HEAD workers, under one lock.
#[derive(Debug)]
pub(crate) struct Schedule {
    /// Initialization fragment, always kept.
    pub(crate) init: Arc<FragmentData>,
    /// Media fragments in timeline order.
    pub(crate) media: Vec<Arc<FragmentData>>,
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
    /// Media time spanned by each media fragment. Empty without a usable
    /// timeline, which keeps everything.
    spans: Vec<Range<Duration>>,
    /// The media fragments the stream keeps now: its retention window, as
    /// indexes into `media`.
    pub(crate) window: Range<usize>,
}

impl Schedule {
    /// A schedule for `init` and `media`, each media fragment spanning the
    /// same-index entry of `spans`. Without `spans`, retention keeps
    /// everything.
    pub(crate) fn new(
        init: Arc<FragmentData>,
        media: Vec<Arc<FragmentData>>,
        mut spans: Vec<Range<Duration>>,
    ) -> Self {
        let complete = init.has_body() && media.iter().all(|data| data.has_body());
        // A timeline that does not line up with the media can't place the
        // window, so the stream keeps everything instead.
        if !spans.is_empty() && spans.len() != media.len() {
            #[cfg(feature = "log")]
            log::warn!(
                "MPEG stream timeline has {} media spans for {} media fragments; \
                 ignoring retention and keeping every fragment",
                spans.len(),
                media.len()
            );
            spans.clear();
        }
        let window = 0..media.len();
        Self {
            init,
            media,
            head_available: true,
            eager_head: false,
            head_worker_running: false,
            cursor: Part::Init,
            priority: None,
            complete,
            error: None,
            retention: Retention::default(),
            spans,
            window,
        }
    }

    /// The fragment at `part`, or `None` past the end.
    pub(crate) fn fragment(&self, part: Part) -> Option<&Arc<FragmentData>> {
        match part {
            Part::Init => Some(&self.init),
            Part::Media(index) => self.media.get(index),
        }
    }

    /// Every fragment in byte order, with its part.
    fn parts(&self) -> impl Iterator<Item = (Part, &Arc<FragmentData>)> {
        std::iter::once((Part::Init, &self.init)).chain(
            self.media
                .iter()
                .enumerate()
                .map(|(index, data)| (Part::Media(index), data)),
        )
    }

    /// The part just past the last fragment.
    pub(crate) fn end(&self) -> Part {
        Part::Media(self.media.len())
    }

    /// Whether `part` is at or past the end of the stream.
    pub(crate) fn past_end(&self, part: Part) -> bool {
        part >= self.end()
    }

    /// The fragments the stream keeps now: the initialization fragment and
    /// its window.
    pub(crate) fn kept(&self) -> Vec<Arc<FragmentData>> {
        std::iter::once(&self.init)
            .chain(&self.media[self.window.clone()])
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

    /// Asks the GET worker for the fragment at `part` before anything else,
    /// even outside the window.
    pub(crate) fn request(&mut self, part: Part) {
        if self.missing_body(part) {
            self.priority = Some(part);
            self.complete = false;
        }
    }

    /// Whether the stream keeps the fragment at `part`.
    pub(crate) fn keeps(&self, part: Part) -> bool {
        match part {
            Part::Init => true,
            Part::Media(index) => self.window.contains(&index),
        }
    }

    /// Recomputes the window for the cursor, and tells which fragments
    /// entered and left it.
    fn rewindow(&mut self) -> WindowChange {
        let window = self.window_at(self.cursor);
        if window == self.window {
            return WindowChange::default();
        }
        let old = std::mem::replace(&mut self.window, window.clone());
        let pick = |range: &Range<usize>, other: &Range<usize>| -> Vec<_> {
            range
                .clone()
                .filter(|index| !other.contains(index))
                .map(|index| self.media[index].clone())
                .collect()
        };
        let change = WindowChange {
            acquired: pick(&window, &old),
            released: pick(&old, &window),
        };
        if window
            .clone()
            .any(|index| self.missing_body(Part::Media(index)))
        {
            self.complete = false;
        }
        change
    }

    /// The window retention gives a cursor at `cursor`: as far behind and
    /// ahead of its media fragment's start as `retention` says, and at least
    /// that fragment itself. A cursor in the initialization fragment counts
    /// as the first media fragment.
    fn window_at(&self, cursor: Part) -> Range<usize> {
        let len = self.media.len();
        if self.spans.is_empty() || self.retention == Retention::default() {
            return 0..len;
        }
        // Past the last fragment, the position is the end of the media.
        let cursor = cursor.media_index();
        let playhead = match self.spans.get(cursor) {
            Some(span) => span.start,
            None => self.spans.last().map_or(Duration::ZERO, |span| span.end),
        };
        let start = match self.retention.behind {
            Some(behind) => {
                let from = playhead.saturating_sub(behind);
                self.spans.partition_point(|span| span.end <= from)
            }
            None => 0,
        };
        let end = match self.retention.ahead {
            Some(ahead) => {
                let until = playhead.saturating_add(ahead);
                self.spans.partition_point(|span| span.start < until)
            }
            None => len,
        };
        start.min(cursor)..end.max(cursor + 1).min(len)
    }

    pub(crate) fn all_sizes_known(&self) -> bool {
        self.total_len().is_some()
    }

    /// Whether the fragment at `part` exists and its body is not held.
    pub(crate) fn missing_body(&self, part: Part) -> bool {
        self.fragment(part).is_some_and(|data| !data.has_body())
    }

    /// The first fragment whose size is unknown.
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

    /// The next fragment the GET worker should fetch: the priority, else the
    /// initialization fragment if missing, else the first missing one in the
    /// window from the cursor on, else one in the window before the cursor.
    pub(crate) fn next_get(&mut self) -> Option<(Part, Arc<FragmentData>)> {
        let Range { start, end } = self.window;
        let cursor = self.cursor.media_index().clamp(start, end);
        let part = self
            .priority
            .take()
            .filter(|&priority| self.missing_body(priority))
            .or_else(|| {
                std::iter::once(Part::Init)
                    .chain((cursor..end).chain(start..cursor).map(Part::Media))
                    .find(|&part| self.missing_body(part))
            })?;
        Some((part, self.fragment(part)?.clone()))
    }

    /// The next fragment whose size a HEAD should learn.
    pub(crate) fn next_head(&self) -> Option<(Part, Arc<FragmentData>)> {
        if !self.head_available {
            return None;
        }
        let part = self.first_unknown_size()?;
        Some((part, self.fragment(part)?.clone()))
    }

    /// Sum of the sizes of every fragment before `part`, if all are known.
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

    // TODO: Publish a media fragment's leading `moof` as soon as those bytes
    // have arrived, instead of waiting for the trailing `mdat`. IsoMp4 open
    // and the first packet only need the `moof`; `Read` currently blocks until
    // the whole GET has arrived. Stream the body, parse top-level box sizes,
    // and wake readers once the `moof` is complete.
    /// Stops trusting HEAD if the body of the fragment at `part` disagrees
    /// with its probe.
    pub(crate) fn check_probe(&mut self, part: Part) {
        let Some(data) = self.fragment(part) else {
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

    /// The body of the fragment at `part`, after checking it against its HEAD
    /// probe. Readers take bodies through this, not straight from the store:
    /// the body is published before the fetching worker checks it, and may
    /// have been fetched by another stream sharing the cache.
    pub(crate) fn checked_body(&mut self, part: Part) -> Option<Bytes> {
        self.check_probe(part);
        self.fragment(part)?.body()
    }

    /// Maps an absolute byte offset onto a part and an offset into it once
    /// contiguous sizes covering that offset are known. An offset past the
    /// end lands at the end of the last fragment.
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
            .map_or(Part::Init, Part::Media);
        let last_len = self.fragment(last)?.size().unwrap_or(0);
        Some((last, last_len as usize))
    }
}

/// The rest of a stream's fragment, with where it sits.
#[derive(Clone, Debug)]
pub(crate) struct PartBody {
    pub(crate) part: Part,
    /// Index in the representation, for a media fragment.
    pub(crate) index: Option<usize>,
    pub(crate) fragment: Fragment,
    pub(crate) body: Bytes,
}

/// A stream reader's position: a part, an offset into it, and the absolute
/// byte offset when the sizes before it are known.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    pub(crate) part: Part,
    pub(crate) offset_in_fragment: usize,
    pub(crate) byte_position: Option<u64>,
}

impl Cursor {
    /// The start of the initialization fragment.
    pub(crate) fn start() -> Self {
        Self {
            part: Part::Init,
            offset_in_fragment: 0,
            byte_position: Some(0),
        }
    }

    /// Copies from `body`, the fragment under the cursor, into `buf` as far as
    /// it fits, and moves past what was copied. Zero means nothing was left of
    /// the fragment, and the cursor has moved on to the next one.
    pub(crate) fn read(&mut self, body: &[u8], buf: &mut [u8]) -> usize {
        let offset = self.offset_in_fragment;
        if offset >= body.len() {
            self.next_fragment();
            return 0;
        }
        let copied = (body.len() - offset).min(buf.len());
        buf[..copied].copy_from_slice(&body[offset..offset + copied]);
        self.offset_in_fragment += copied;
        if self.offset_in_fragment >= body.len() {
            self.next_fragment();
        }
        self.advance_bytes(copied);
        copied
    }

    /// The rest of `body`, the fragment under the cursor, moving on to the
    /// next fragment. `None` when nothing was left of it.
    pub(crate) fn take_rest(&mut self, body: Bytes) -> Option<Bytes> {
        let offset = self.offset_in_fragment.min(body.len());
        let rest = body.len() - offset;
        self.next_fragment();
        self.advance_bytes(rest);
        (rest > 0).then(|| body.slice(offset..))
    }

    /// Moves to `offset_in_fragment` bytes into `part`, at absolute byte
    /// `byte_position` when known.
    pub(crate) fn place(
        &mut self,
        part: Part,
        offset_in_fragment: usize,
        byte_position: Option<u64>,
    ) {
        self.part = part;
        self.offset_in_fragment = offset_in_fragment;
        self.byte_position = byte_position;
    }

    fn next_fragment(&mut self) {
        self.part = self.part.next();
        self.offset_in_fragment = 0;
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

/// A stream's media timing: its representation's whole timeline, and the
/// index on it of the stream's first media fragment. A stream that starts
/// partway through a representation still reports and seeks by the
/// representation's media time.
#[derive(Clone, Debug)]
pub(crate) struct StreamTimeline {
    timeline: MediaTimeline,
    first: usize,
}

impl StreamTimeline {
    /// The timing of a stream whose media fragments are those of `timeline`
    /// from index `first` on.
    pub(crate) fn new(timeline: MediaTimeline, first: usize) -> Self {
        Self { timeline, first }
    }

    /// The representation's whole timeline.
    pub(crate) fn timeline(&self) -> &MediaTimeline {
        &self.timeline
    }

    /// Index in the representation of the stream's first media fragment.
    pub(crate) fn first(&self) -> usize {
        self.first
    }

    /// Media time spanned by each of the stream's media fragments.
    pub(crate) fn spans(&self) -> Vec<Range<Duration>> {
        let mut spans = self.timeline.spans();
        spans.drain(..self.first.min(spans.len()));
        spans
    }

    /// The durations of the stream's own media fragments.
    fn media(&self) -> MediaTimeline {
        MediaTimeline {
            timescale: self.timeline.timescale,
            origin: self
                .timeline
                .origin
                .saturating_add(self.timeline.media_durations.iter().take(self.first).sum()),
            media_durations: self
                .timeline
                .media_durations
                .get(self.first..)
                .unwrap_or_default()
                .to_vec(),
        }
    }
}

/// The fragments of a stream starting at the media fragment of `fragments`
/// covering `time` on `timeline`, the stream's timing, and that fragment's
/// start. A time at or past the end starts at the last media fragment.
pub(crate) fn start_at(
    mut fragments: Fragments,
    timeline: MediaTimeline,
    time: Duration,
) -> Result<(Fragments, StreamTimeline, Duration), Error> {
    let (first, span) = timeline.span_at(time).ok_or_else(|| {
        Error::Initialization(
            "MPEG stream timeline is missing a timescale or media durations".to_owned(),
        )
    })?;
    if first >= fragments.media.len() {
        return Err(Error::Initialization(format!(
            "MPEG stream has {} media fragments, too few to start at {}s",
            fragments.media.len(),
            time.as_secs_f64()
        )));
    }
    fragments.media.drain(..first);
    Ok((fragments, StreamTimeline::new(timeline, first), span.start))
}

/// The index of the stream's media fragment a seek to `time` lands in.
pub(crate) fn time_target(timeline: Option<&StreamTimeline>, time: Duration) -> io::Result<usize> {
    let timeline = timeline.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "MPEG stream has no SegmentTimeline",
        )
    })?;
    let index = timeline.timeline.fragment_at(time).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "MPEG stream timeline is missing a timescale or media durations",
        )
    })?;
    index.checked_sub(timeline.first).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "MPEG stream starts after the requested media time",
        )
    })
}

/// The stream's part for media fragment `index` of the representation, given the
/// stream's `timeline`. A stream without one starts at the first fragment. `None`
/// for a fragment before the stream's first.
pub(crate) fn media_part(timeline: Option<&StreamTimeline>, index: usize) -> Option<Part> {
    let first = timeline.map_or(0, |timeline| timeline.first);
    index.checked_sub(first).map(Part::Media)
}

/// The index in the representation of the stream's `part`, given the stream's
/// `timeline`. `None` for the initialization fragment.
pub(crate) fn representation_index(timeline: Option<&StreamTimeline>, part: Part) -> Option<usize> {
    let Part::Media(index) = part else {
        return None;
    };
    Some(timeline.map_or(0, |timeline| timeline.first) + index)
}

/// The start of the media fragment at `part` on `timeline`, as a position of
/// `accuracy`. `None` for the initialization fragment, which has no media
/// time.
pub(crate) fn time_at_fragment(
    timeline: Option<&StreamTimeline>,
    part: Part,
    accuracy: TimeAccuracy,
) -> Option<TimePosition> {
    let Part::Media(index) = part else {
        return None;
    };
    let timeline = timeline?;
    let span = timeline
        .timeline
        .spans()
        .get(timeline.first + index)?
        .clone();
    Some(TimePosition {
        timestamp: span.start,
        accuracy,
    })
}

/// The end of `timeline`, as a packet-accurate position.
pub(crate) fn time_at_end(timeline: Option<&StreamTimeline>) -> Option<TimePosition> {
    timeline?
        .timeline
        .total_duration()
        .map(|timestamp| TimePosition {
            timestamp,
            accuracy: TimeAccuracy::Packet,
        })
}

/// The accuracy of a fragment's start time for a byte position
/// `offset_in_fragment` bytes into that fragment: exact to the packet at its
/// start, and only to the fragment past it.
pub(crate) fn accuracy_at(offset_in_fragment: usize) -> TimeAccuracy {
    match offset_in_fragment == 0 {
        true => TimeAccuracy::Packet,
        false => TimeAccuracy::Fragment,
    }
}

/// The length a symphonia-compat stream reports before every size is known.
pub(crate) fn compat_estimate(
    timeline: Option<&StreamTimeline>,
    init_len: u64,
    bitrate_bps: u32,
) -> Result<u64, Error> {
    let timeline = timeline.ok_or_else(|| {
        Error::Initialization("symphonia_compat requires a SegmentTimeline".to_owned())
    })?;
    timeline
        .media()
        .estimate_len(init_len, bitrate_bps)
        .ok_or_else(|| {
            Error::Initialization(
                "symphonia_compat requires a non-zero bitrate and timescale".to_owned(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_target_lands_on_reported_fragment_starts() {
        // A third of a second does not land on a whole nanosecond.
        let timeline = MediaTimeline {
            timescale: 3,
            origin: 0,
            media_durations: vec![1, 1],
        };
        let second_start = timeline.spans()[1].start;
        let whole = StreamTimeline::new(timeline, 0);
        let target = |time| time_target(Some(&whole), time).unwrap();
        assert_eq!(target(second_start), 1);
        assert_eq!(target(second_start - Duration::from_nanos(1)), 0);
        assert_eq!(target(Duration::from_secs(5)), 1);

        let empty = StreamTimeline::new(
            MediaTimeline {
                timescale: 3,
                origin: 0,
                media_durations: Vec::new(),
            },
            0,
        );
        let error = time_target(Some(&empty), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = time_target(None, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_stream_from_partway_keeps_the_representation_times() {
        // Four one-second fragments; the stream starts at the third.
        let timeline = StreamTimeline::new(
            MediaTimeline {
                timescale: 1,
                origin: 0,
                media_durations: vec![1; 4],
            },
            2,
        );
        assert_eq!(timeline.spans(), [secs(2)..secs(3), secs(3)..secs(4)]);
        assert_eq!(timeline.media().media_durations, [1, 1]);

        // Seeks name the representation's time, and land on the stream's own fragments.
        let target = |time| time_target(Some(&timeline), time);
        assert_eq!(target(secs(2)).unwrap(), 0);
        assert_eq!(target(Duration::from_millis(3_500)).unwrap(), 1);
        assert_eq!(
            target(Duration::from_millis(1_500)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let at = |part| time_at_fragment(Some(&timeline), part, TimeAccuracy::Packet);
        assert_eq!(at(Part::Init), None);
        assert_eq!(at(Part::Media(1)).unwrap().timestamp, secs(3));
        assert_eq!(at(Part::Media(2)), None);
        assert_eq!(time_at_end(Some(&timeline)).unwrap().timestamp, secs(4));
    }

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }
}
