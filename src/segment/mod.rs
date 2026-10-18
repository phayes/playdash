//! Lower-level access to the segments that make up a DASH representation.
//!
//! Most applications should use [`MediaStream`](crate::MediaStream). Use this module
//! when you need complete downloaded segments, a seekable byte stream for another MP4
//! demuxer, a shared cache, or a custom source of segment bytes.

#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod buffer;
#[cfg(feature = "sync")]
mod cache;
pub(crate) mod clock;
pub(crate) mod index;
#[cfg(all(test, feature = "sync"))]
mod live_tests;
#[cfg(feature = "sync")]
mod manifest;
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod schedule;
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) mod store;
#[cfg(feature = "sync")]
mod stream;
#[cfg(feature = "sync")]
pub(crate) mod transport;

#[cfg(feature = "sync")]
pub use cache::SegmentCache;
#[cfg(any(feature = "sync", feature = "async"))]
pub use schedule::{Position, TimePosition};
#[cfg(feature = "sync")]
pub use stream::SegmentStream;
#[cfg(feature = "sync")]
pub use transport::Transport;

use crate::error::Error;
use index::MediaIndex;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroU32;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A representation's media clock: how the media timestamps of its segments map onto
/// presentation time.
///
/// Media timestamps count [`Self::timescale`] ticks on the representation's own
/// timeline, as DASH `$Time$` and MP4 decode times do. Presentation time is shared by
/// every representation of a presentation: a media timestamp equal to
/// [`Self::presentation_time_offset`] presents at [`Self::period_start`].
///
/// # Examples
///
/// ```
/// use std::num::NonZeroU32;
/// use std::time::Duration;
/// use playdash::segment::MediaTiming;
///
/// let timing = MediaTiming {
///     timescale: NonZeroU32::new(1_000).unwrap(),
///     presentation_time_offset: 5_000,
///     period_start: Duration::from_secs(60),
/// };
/// assert_eq!(timing.presentation_time(7_500), Duration::from_millis(62_500));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaTiming {
    /// Number of media ticks per second.
    pub timescale: NonZeroU32,
    /// Media timestamp, in [`Self::timescale`] ticks, that presents at
    /// [`Self::period_start`]: DASH's `@presentationTimeOffset`.
    pub presentation_time_offset: u64,
    /// Presentation time at which the representation's period starts: DASH's
    /// `Period@start`.
    pub period_start: Duration,
}

impl MediaTiming {
    /// Converts a media timestamp in [`Self::timescale`] ticks, such as a segment start
    /// or an MP4 decode time, to presentation time.
    ///
    /// A timestamp before [`Self::presentation_time_offset`] converts to
    /// [`Self::period_start`].
    pub fn presentation_time(&self, ticks: u64) -> Duration {
        let offset = ticks.saturating_sub(self.presentation_time_offset);
        self.period_start.saturating_add(self.duration(offset))
    }

    /// `ticks` as a duration.
    pub(crate) fn duration(&self, ticks: u64) -> Duration {
        let timescale = u64::from(self.timescale.get());
        let nanos = u128::from(ticks % timescale) * 1_000_000_000 / u128::from(timescale);
        Duration::new(ticks / timescale, nanos as u32)
    }
}

/// A media segment's number in its representation: consecutive segments have
/// consecutive numbers, and a number keeps naming the same segment while a
/// stream reads it. For static segments it is the segment's position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SegmentNumber(pub(crate) u64);

impl SegmentNumber {
    /// The number of the segment after this one.
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// The number of the segment before this one, or `None` before zero.
    pub(crate) fn checked_prev(self) -> Option<Self> {
        self.0.checked_sub(1).map(Self)
    }

    /// How many segments this one is after `first`, or `None` before it.
    pub(crate) fn offset_from(self, first: Self) -> Option<usize> {
        usize::try_from(self.0.checked_sub(first.0)?).ok()
    }

    /// The segment `count` after this one.
    pub(crate) fn plus(self, count: usize) -> Self {
        Self(self.0 + count as u64)
    }
}

/// The numbers from the start of `range` to just before its end.
pub(crate) fn each(range: Range<SegmentNumber>) -> impl DoubleEndedIterator<Item = SegmentNumber> {
    (range.start.0..range.end.0).map(SegmentNumber)
}

impl fmt::Display for SegmentNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A timed media segment: its location, and the media time it covers.
///
/// [`Self::start`] and [`Self::duration`] count [`MediaTiming::timescale`] ticks on the
/// representation's media timeline, as DASH `$Time$` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaSegment {
    /// The segment's media resource.
    pub location: SegmentLocation,
    /// Media timestamp of the segment's start, including any
    /// [`MediaTiming::presentation_time_offset`].
    pub start: u64,
    /// Length of the segment's media, in ticks.
    pub duration: u64,
}

/// A fetched manifest and its final URL after redirects.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedManifest {
    /// UTF-8 XML text of the manifest.
    pub body: String,
    /// The URL the manifest was served from, after any redirects. Relative
    /// URLs in the manifest resolve against it.
    pub url: String,
}

#[cfg(any(feature = "sync", feature = "async"))]
impl FetchedManifest {
    /// Decodes manifest bytes fetched from `url` as UTF-8.
    ///
    /// Fails with [`Error::DataUrlUtf8`] if `body` is not UTF-8.
    pub(crate) fn from_bytes(body: bytes::Bytes, url: &str) -> Result<Self, Error> {
        Ok(Self {
            body: String::from_utf8(body.to_vec())?,
            url: url.to_owned(),
        })
    }
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

/// The `range` bytes of a full body, for servers that answer a Range request with 200.
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) fn slice_range<'a>(body: &'a [u8], range: &RangeInclusive<u64>) -> Option<&'a [u8]> {
    let start = usize::try_from(*range.start()).ok()?;
    let end = usize::try_from(*range.end()).ok()?;
    body.get(start..=end)
}

/// A segment's location: a URL, optionally limited to an inclusive byte range.
///
/// # Examples
///
/// ```
/// use playdash::segment::SegmentLocation;
///
/// let whole = SegmentLocation::new("https://media.example/init.mp4");
/// assert_eq!(whole.known_size(), None);
///
/// let range = SegmentLocation::with_range("https://media.example/audio.mp4", 100..=599);
/// assert_eq!(range.known_size(), Some(500));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentLocation {
    /// Absolute URL of the resource containing this segment.
    pub url: String,
    /// Inclusive byte range within [`Self::url`], matching HTTP `Range` semantics.
    /// `None` means the entire response body. Callers constructing this directly must
    /// keep the start less than or equal to the end.
    pub range: Option<RangeInclusive<u64>>,
}

impl SegmentLocation {
    /// Creates a location covering the entire response body at `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            range: None,
        }
    }

    /// Creates a location covering the inclusive byte `range` within `url`.
    pub fn with_range(url: impl Into<String>, range: RangeInclusive<u64>) -> Self {
        Self {
            url: url.into(),
            range: Some(range),
        }
    }

    /// Returns the byte length implied by [`Self::range`].
    /// Returns `None` for a whole-resource location or an invalid/overflowing range.
    pub fn known_size(&self) -> Option<u64> {
        self.range
            .as_ref()
            .and_then(|range| range.end().checked_sub(*range.start())?.checked_add(1))
    }
}

impl From<String> for SegmentLocation {
    fn from(url: String) -> Self {
        Self::new(url)
    }
}

impl From<&str> for SegmentLocation {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

impl fmt::Display for SegmentLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.range {
            Some(range) => write!(f, "{} (bytes {}-{})", self.url, range.start(), range.end()),
            None => f.write_str(&self.url),
        }
    }
}

/// A representation's segments: its initialization segment and its media
/// segments, which may be timed. A stream of them reads the initialization
/// segment first, then the media segments in order.
///
/// Segments from a [`DashManifest`](crate::DashManifest) are timed: each media
/// segment carries the presentation time it covers, which time seeks, retention
/// and buffer status need. Segments from [`Self::new`] are untimed.
///
/// # Examples
///
/// ```
/// use std::num::NonZeroU32;
/// use std::time::Duration;
/// use playdash::segment::{MediaSegment, MediaTiming, SegmentLocation, Segments};
///
/// let untimed = Segments::new(
///     "https://media.example/init.mp4",
///     ["https://media.example/1.m4s", "https://media.example/2.m4s"],
/// );
/// assert_eq!(untimed.init(), &SegmentLocation::new("https://media.example/init.mp4"));
/// assert_eq!(untimed.media_len(), 2);
/// assert_eq!(untimed.span(0), None);
///
/// let timing = MediaTiming {
///     timescale: NonZeroU32::new(1_000).unwrap(),
///     presentation_time_offset: 0,
///     period_start: Duration::ZERO,
/// };
/// let segment = |url: &str, start| MediaSegment {
///     location: SegmentLocation::new(url),
///     start,
///     duration: 2_000,
/// };
/// let timed = Segments::timed(
///     "https://media.example/init.mp4",
///     timing,
///     [
///         segment("https://media.example/1.m4s", 0),
///         segment("https://media.example/2.m4s", 2_000),
///     ],
/// )?;
/// let secs = Duration::from_secs;
/// assert_eq!(timed.span(1), Some(secs(2)..secs(4)));
/// assert_eq!(timed.segment_at(Duration::from_millis(2_500)), Some(1));
/// # Ok::<(), playdash::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segments {
    init: SegmentLocation,
    index: Arc<MediaIndex>,
}

impl Segments {
    /// Creates untimed segments of a representation from the locations of its
    /// initialization segment and its media segments in playback order.
    pub fn new(
        init: impl Into<SegmentLocation>,
        media: impl IntoIterator<Item = impl Into<SegmentLocation>>,
    ) -> Self {
        let media = media.into_iter().map(Into::into).collect();
        Self::indexed(init.into(), MediaIndex::Untimed(media))
    }

    /// Segments of a representation from its initialization segment's
    /// location and the index of its media segments.
    pub(crate) fn indexed(init: SegmentLocation, index: MediaIndex) -> Self {
        Self::from_index(init, Arc::new(index))
    }

    /// [`Self::indexed`], sharing `index`.
    pub(crate) fn from_index(init: SegmentLocation, index: Arc<MediaIndex>) -> Self {
        Self { init, index }
    }

    /// Creates timed segments of a representation from its initialization
    /// segment's location and its media segments in playback order, on
    /// `timing`'s clock.
    ///
    /// Fails with [`Error::InvalidSegmentTiming`] if a segment has a zero
    /// duration, starts before the segment ahead of it ends, or ends past the
    /// largest media timestamp.
    pub fn timed(
        init: impl Into<SegmentLocation>,
        timing: MediaTiming,
        segments: impl IntoIterator<Item = MediaSegment>,
    ) -> Result<Self, Error> {
        let segments: Vec<MediaSegment> = segments.into_iter().collect();

        // Each segment must follow the one before it.
        let mut end = 0u64;
        for (index, segment) in segments.iter().enumerate() {
            check_segment(
                index as u64,
                || segment.location.clone(),
                segment.start,
                segment.duration,
                &mut end,
            )?;
        }

        Ok(Self::indexed(
            init.into(),
            MediaIndex::List { timing, segments },
        ))
    }

    /// Returns the initialization segment's location.
    pub fn init(&self) -> &SegmentLocation {
        &self.init
    }

    /// Returns `true` if these segments belong to a live representation.
    ///
    /// Live segment numbers need not start at zero; see [`Self::available`].
    /// This snapshot does not refresh itself. Streams opened from the parent
    /// [`DashManifest`](crate::DashManifest) share automatic refreshes.
    pub fn is_live(&self) -> bool {
        self.index.is_live()
    }

    /// Returns the segment-number range available according to the system clock.
    ///
    /// Static media returns its full range. Live media returns segments that
    /// have been produced and remain inside the time-shift window.
    ///
    /// Use these indexes with [`Self::span`]. The range is empty when no live
    /// segment is available yet.
    pub fn available(&self) -> Range<usize> {
        self.available_when(SystemTime::now())
    }

    /// [`Self::available`] at wall-clock time `now`.
    pub(crate) fn available_when(&self, now: SystemTime) -> Range<usize> {
        let numbers = self.index.available(now);
        let index = |number: SegmentNumber| usize::try_from(number.0).unwrap_or(usize::MAX);
        index(numbers.start)..index(numbers.end)
    }

    /// Returns the current media-segment count.
    ///
    /// For live media, this is the number just past the newest available segment.
    pub fn media_len(&self) -> usize {
        self.available().end
    }

    /// Returns currently available media locations in playback order.
    pub fn media(&self) -> impl Iterator<Item = SegmentLocation> + '_ {
        each(self.index.available(SystemTime::now()))
            .filter_map(|number| self.index.location(number))
    }

    /// Returns the clock the media segments are timed on, or `None` for untimed
    /// segments.
    pub fn timing(&self) -> Option<&MediaTiming> {
        self.index.timing()
    }

    /// Returns the presentation time covered by media segment `index`.
    /// Returns `None` for untimed segments or an index past the last.
    pub fn span(&self, index: usize) -> Option<Range<Duration>> {
        self.index.span(SegmentNumber(index as u64))
    }

    /// Finds the media segment covering presentation time `time`, as an index
    /// usable with [`Self::span`].
    ///
    /// A time before the first segment lands in the first, and a time at or
    /// past the end lands in the last; for live segments, the first and last
    /// available now. Returns `None` for untimed segments, segments without
    /// media, and live segments while none is available.
    pub fn segment_at(&self, time: Duration) -> Option<usize> {
        let number = self.index.number_at(time)?;
        let available = self.index.available(SystemTime::now());
        let last = available.end.checked_prev()?;
        if last < available.start {
            return None;
        }
        usize::try_from(number.clamp(available.start, last).0).ok()
    }

    /// Returns the presentation time at which the last media segment ends.
    /// Returns `None` for untimed segments, segments without media, and live
    /// segments.
    pub fn end(&self) -> Option<Duration> {
        match self.is_live() {
            true => None,
            false => self.index.end_time(),
        }
    }

    /// Returns a conservative stream-length estimate for a demuxer that needs
    /// a length before all segments have been downloaded.
    ///
    /// Returns `None` for untimed segments, segments without media, live
    /// segments, or a zero `bitrate_bps`.
    pub fn estimate_len(&self, init_len: u64, bitrate_bps: u32) -> Option<u64> {
        if self.is_live() {
            return None;
        }
        let first = self.index.first();
        self.index.estimate_len(first, init_len, bitrate_bps)
    }

    /// The index of the media segments.
    #[cfg(any(feature = "sync", feature = "async"))]
    pub(crate) fn index(&self) -> &Arc<MediaIndex> {
        &self.index
    }
}

/// Fails with [`Error::DashManifestUnsupportedLive`] for live `segments`,
/// which only a stream opened from their manifest keeps refreshed.
#[cfg(any(feature = "sync", feature = "async"))]
pub(crate) fn refuse_live(segments: &Segments) -> Result<(), Error> {
    match segments.is_live() {
        true => Err(Error::DashManifestUnsupportedLive(
            "live segments stream only from their DashManifest, which keeps them refreshed"
                .to_owned(),
        )),
        false => Ok(()),
    }
}

/// Checks that media segment `index`, starting at media time `start` and
/// lasting `duration` ticks, follows the segment ahead of it, which ends at
/// `end`, and moves `end` to this segment's end.
///
/// Fails with [`Error::InvalidSegmentTiming`] for a zero duration, a start
/// before `end`, or an end past the largest media timestamp, naming the
/// segment by the location `location` gives.
pub(crate) fn check_segment(
    index: u64,
    location: impl FnOnce() -> SegmentLocation,
    start: u64,
    duration: u64,
    end: &mut u64,
) -> Result<(), Error> {
    let invalid = |reason: String| {
        let location = location();
        Error::InvalidSegmentTiming(format!("media segment {index} ({location}) {reason}"))
    };
    if duration == 0 {
        return Err(invalid("has a zero duration".to_owned()));
    }
    if start < *end {
        return Err(invalid(format!(
            "starts at {start} before the previous segment ends at {end}"
        )));
    }
    *end = start
        .checked_add(duration)
        .ok_or_else(|| invalid("ends past the largest media timestamp".to_owned()))?;
    Ok(())
}

/// The timed media segments of `segments` in playback order, for tests, or
/// `None` for untimed segments.
#[cfg(test)]
pub(crate) fn timed_media_for_test(segments: &Segments) -> Option<Vec<MediaSegment>> {
    let index = &segments.index;
    index.timing()?;
    let media = each(index.numbers()).filter_map(|number| {
        let (start, duration) = index.ticks(number)?;
        Some(MediaSegment {
            location: index.location(number)?,
            start,
            duration,
        })
    });
    Some(media.collect())
}

/// Timed segments for tests: `media` back to back from media time zero, each
/// lasting the same-index entry of `durations` in `timescale` ticks.
#[cfg(test)]
pub(crate) fn timed_for_test(
    init: impl Into<SegmentLocation>,
    media: impl IntoIterator<Item = impl Into<SegmentLocation>>,
    timescale: u32,
    durations: &[u64],
) -> Segments {
    let media: Vec<SegmentLocation> = media.into_iter().map(Into::into).collect();
    assert_eq!(
        media.len(),
        durations.len(),
        "one duration per media segment"
    );
    let timing = MediaTiming {
        timescale: NonZeroU32::new(timescale).expect("a non-zero timescale"),
        presentation_time_offset: 0,
        period_start: Duration::ZERO,
    };
    let mut start = 0;
    let segments = media
        .into_iter()
        .zip(durations)
        .map(|(location, &duration)| {
            let segment = MediaSegment {
                location,
                start,
                duration,
            };
            start += duration;
            segment
        });
    Segments::timed(init, timing, segments.collect::<Vec<_>>()).expect("valid test timing")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(timescale: u32) -> MediaTiming {
        MediaTiming {
            timescale: NonZeroU32::new(timescale).unwrap(),
            presentation_time_offset: 0,
            period_start: Duration::ZERO,
        }
    }

    /// Segments of `durations` back to back from media time `start`.
    fn timed(timing: MediaTiming, start: u64, durations: &[u64]) -> Segments {
        let mut time = start;
        let segments = durations.iter().enumerate().map(|(index, &duration)| {
            let segment = MediaSegment {
                location: SegmentLocation::new(format!("{index}.m4s")),
                start: time,
                duration,
            };
            time += duration;
            segment
        });
        Segments::timed("init.mp4", timing, segments.collect::<Vec<_>>()).unwrap()
    }

    fn two_seconds() -> Segments {
        timed(timing(44_100), 0, &[44_100, 44_100])
    }

    #[test]
    fn estimate_len_errs_larger_than_advertised_media() {
        let segments = two_seconds();
        let init_len = 4;
        let bitrate = 128_000;
        let raw_media = u64::from(bitrate) * 2 / 8;
        let estimate = segments.estimate_len(init_len, bitrate).unwrap();
        assert!(
            estimate >= init_len + raw_media * 4 + 2 * 64 * 1024,
            "estimate {estimate} should pad raw media {raw_media}"
        );
        assert!(segments.estimate_len(init_len, 0).is_none());
        let untimed = Segments::new("init.mp4", ["0.m4s"]);
        assert!(untimed.estimate_len(init_len, bitrate).is_none());
    }

    #[test]
    fn segment_at_indexes_media_segments() {
        let segments = two_seconds();
        let at = |millis| segments.segment_at(Duration::from_millis(millis));
        assert_eq!(at(0), Some(0));
        assert_eq!(at(999), Some(0));
        assert_eq!(at(1_000), Some(1));
        assert_eq!(at(2_000), Some(1));
        assert_eq!(at(60_000), Some(1));
        assert_eq!(segments.end(), Some(Duration::from_secs(2)));

        let empty = timed(timing(1_000), 0, &[]);
        assert_eq!(empty.segment_at(Duration::ZERO), None);
        assert_eq!(empty.end(), None);
        let untimed = Segments::new("init.mp4", ["0.m4s"]);
        assert_eq!(untimed.segment_at(Duration::ZERO), None);
        assert_eq!(untimed.end(), None);
    }

    #[test]
    fn spans_are_presentation_time() {
        // Media starts 2 s after the offset, in a period starting at 60 s.
        let clock = MediaTiming {
            presentation_time_offset: 10_000,
            period_start: Duration::from_secs(60),
            ..timing(1_000)
        };
        let segments = timed(clock, 12_000, &[2_000, 2_000]);
        let secs = Duration::from_secs;
        assert_eq!(segments.span(0), Some(secs(62)..secs(64)));
        assert_eq!(segments.span(1), Some(secs(64)..secs(66)));
        assert_eq!(segments.span(2), None);
        assert_eq!(segments.end(), Some(secs(66)));

        // Before the first segment lands in it.
        assert_eq!(segments.segment_at(secs(61)), Some(0));
        assert_eq!(segments.segment_at(secs(64)), Some(1));

        // Before the offset presents at the period start.
        assert_eq!(clock.presentation_time(9_000), secs(60));
    }

    #[test]
    fn timed_rejects_overlapping_and_empty_segments() {
        let segment = |start, duration| MediaSegment {
            location: SegmentLocation::new("a.m4s"),
            start,
            duration,
        };
        let build = |segments: Vec<MediaSegment>| Segments::timed("init", timing(1), segments);
        assert!(
            build(vec![segment(0, 2), segment(3, 2)]).is_ok(),
            "a gap is allowed"
        );
        for invalid in [
            vec![segment(0, 0)],
            vec![segment(0, 2), segment(1, 2)],
            vec![segment(u64::MAX, 1)],
        ] {
            assert!(
                matches!(build(invalid.clone()), Err(Error::InvalidSegmentTiming(_))),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn an_estimate_from_partway_counts_only_later_media() {
        let segments = timed(timing(1_000), 0, &[1_000, 2_000, 3_000]);
        let tail = timed(timing(1_000), 1_000, &[2_000, 3_000]);
        let (init_len, bitrate) = (4, 128_000);
        let from = |first| {
            segments
                .index
                .estimate_len(SegmentNumber(first), init_len, bitrate)
        };
        assert_eq!(from(1), tail.estimate_len(init_len, bitrate));
        assert_eq!(from(3), None);
        assert_eq!(from(5), None);
    }

    #[test]
    fn segments_are_numbered_by_position() {
        let n = SegmentNumber;
        let segments = timed(timing(1_000), 0, &[1_000, 1_000]);
        let index = &segments.index;
        assert_eq!(index.numbers(), n(0)..n(2));
        assert_eq!(index.number_at(Duration::from_millis(1_500)), Some(n(1)));
        assert_eq!(
            index.span(n(1)),
            Some(Duration::from_secs(1)..Duration::from_secs(2))
        );
        assert_eq!(index.span(n(2)), None);
        assert_eq!(
            segments.index.location(n(1)),
            Some(SegmentLocation::new("1.m4s"))
        );
        assert_eq!(segments.index.location(n(2)), None);

        // Untimed segments are numbered too, but have no times.
        let untimed = Segments::new("init.mp4", ["a", "b"]);
        assert_eq!(untimed.index.numbers(), n(0)..n(2));
        assert_eq!(
            untimed.index.location(n(1)),
            Some(SegmentLocation::new("b"))
        );
        assert_eq!(
            untimed.media().collect::<Vec<_>>(),
            [SegmentLocation::new("a"), SegmentLocation::new("b")]
        );
        assert_eq!(untimed.index.span(n(1)), None);
        assert_eq!(untimed.index.number_at(Duration::ZERO), None);
    }

    #[test]
    fn segment_numbers_step_and_offset() {
        let n = SegmentNumber;
        assert_eq!(n(4).next(), n(5));
        assert_eq!(n(4).checked_prev(), Some(n(3)));
        assert_eq!(n(0).checked_prev(), None);
        assert_eq!(n(4).offset_from(n(2)), Some(2));
        assert_eq!(n(1).offset_from(n(2)), None);
        assert_eq!(n(2).plus(3), n(5));
        assert_eq!(n(7).to_string(), "7");
    }

    #[test]
    fn byte_range_parses_dash_form() {
        let range = parse_byte_range(" 100-199 ").unwrap();
        assert_eq!(range, 100..=199);
        assert_eq!(
            SegmentLocation::with_range("a", range.clone()).known_size(),
            Some(100)
        );
        assert_eq!(
            SegmentLocation::with_range("a", 5..=5).known_size(),
            Some(1)
        );
        for invalid in ["", "5", "5-", "-5", "9-3", "a-b"] {
            assert!(
                matches!(parse_byte_range(invalid), Err(Error::InvalidByteRange(value)) if value == invalid),
                "{invalid:?} should be rejected"
            );
        }
        assert_eq!(
            SegmentLocation::with_range("media.mp4", range).to_string(),
            "media.mp4 (bytes 100-199)"
        );
    }
}
