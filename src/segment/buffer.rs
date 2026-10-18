//! Download progress of a DASH stream, by segment, byte and media time.

use super::store::{CacheListener, SegmentData, Store};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// The stream's download progress: the portion of it that its cache holds.
///
/// # Examples
///
/// ```no_run
/// # #[cfg(feature = "rodio")]
/// # fn example() -> Result<(), playdash::Error> {
/// use playdash::{DashManifest, DashSource};
/// use std::time::Duration;
///
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
/// let source = DashSource::new(&manifest, "FLAC")?;
/// let status = source.buffer().status();
/// // A player's own position, such as rodio's `Player::get_pos`.
/// let position = Duration::from_secs(30);
/// println!(
///     "buffered to {:?} ({:.0}%), {} of {} segments downloaded",
///     status.buffered_until(position),
///     status.buffered_percent(position),
///     status.downloaded.segments,
///     status.total.segments,
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferStatus {
    /// Total size and duration of the representation's media segments.
    pub total: BufferTotal,
    /// Media currently present in the cache, including non-contiguous ranges.
    pub downloaded: BufferAmount,
    /// Cached ranges of the representation, in presentation time.
    /// Ranges are ordered, non-overlapping, and merge adjacent downloaded segments.
    pub ranges: Vec<Range<Duration>>,
}

/// An amount of downloaded media.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferAmount {
    /// Number of media segments represented by this amount.
    pub segments: usize,
    /// Bytes in those media-segment bodies. The initialization segment is not counted.
    pub bytes: u64,
    /// Sum of the media durations covered by those segments.
    pub time: Duration,
}

/// The size of a whole stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferTotal {
    /// Total number of media segments in the representation.
    pub segments: usize,
    /// Combined byte length of all media segments, excluding initialization.
    /// `None` until every segment's size is known from a byte range, HEAD, or download.
    pub bytes: Option<u64>,
    /// Presentation time at which the representation's first media segment starts.
    pub start: Duration,
    /// Presentation time at which the representation's last media segment ends.
    pub end: Duration,
}

impl BufferStatus {
    /// Presentation time up to which playback can continue from `position`
    /// without waiting on the network: the end of the downloaded range holding
    /// `position`, else `position` itself. A `position` outside the
    /// representation's media is first moved to its nearer end.
    pub fn buffered_until(&self, position: Duration) -> Duration {
        let position = position.clamp(self.total.start, self.total.end);
        self.ranges
            .iter()
            .find(|range| range.contains(&position))
            .map_or(position, |range| range.end)
    }

    /// Returns how far playback of several streams played together, such as
    /// a video and its audio, can continue from `position` without waiting on
    /// the network.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use playdash::{BufferAmount, BufferStatus, BufferTotal};
    ///
    /// let secs = Duration::from_secs;
    /// // Ten-second streams buffered from the start to `buffered` seconds.
    /// let buffered_to = |buffered| BufferStatus {
    ///     total: BufferTotal { segments: 5, bytes: None, start: secs(0), end: secs(10) },
    ///     downloaded: BufferAmount::default(),
    ///     ranges: vec![secs(0)..secs(buffered)],
    /// };
    /// let (video, audio) = (buffered_to(4), buffered_to(8));
    /// assert_eq!(BufferStatus::buffered_until_all([&video, &audio], secs(2)), secs(4));
    /// ```
    pub fn buffered_until_all<'a>(
        statuses: impl IntoIterator<Item = &'a BufferStatus>,
        position: Duration,
    ) -> Duration {
        let mut limit: Option<Duration> = None;
        let mut latest_end: Option<Duration> = None;
        for status in statuses {
            let until = status.buffered_until(position);
            latest_end = latest_end.max(Some(status.total.end));
            // Buffered to its end, a stream has nothing left to wait for.
            if until < status.total.end {
                limit = Some(limit.map_or(until, |limit| limit.min(until)));
            }
        }
        limit.or(latest_end).unwrap_or(position)
    }

    /// Returns [`Self::buffered_until`] as a percentage of the whole duration.
    ///
    /// The result is clamped to `0.0..=100.0`; an empty stream reports `100.0`.
    pub fn buffered_percent(&self, position: Duration) -> f64 {
        let until = self.buffered_until(position) - self.total.start;
        percent(until.as_secs_f64(), self.total_time().as_secs_f64())
    }

    /// Returns the percentage of media time cached anywhere in the stream.
    ///
    /// Unlike [`Self::buffered_percent`], this includes downloaded ranges that are not
    /// contiguous with the playback position. The result is clamped to `0.0..=100.0`;
    /// an empty stream reports `100.0`.
    pub fn downloaded_percent(&self) -> f64 {
        percent(
            self.downloaded.time.as_secs_f64(),
            self.total_time().as_secs_f64(),
        )
    }

    /// Returns the percentage of the stream's media bytes currently cached.
    ///
    /// Returns `None` until every media segment's byte length is known. The result is
    /// clamped to `0.0..=100.0`; a known total of zero bytes reports `100.0`.
    pub fn downloaded_bytes_percent(&self) -> Option<f64> {
        let total = self.total.bytes?;
        Some(percent(self.downloaded.bytes as f64, total as f64))
    }

    /// Returns whether every media segment is currently present in the cache.
    pub fn is_complete(&self) -> bool {
        self.downloaded.segments == self.total.segments
    }

    /// The duration of the representation's media, from its start to its end.
    fn total_time(&self) -> Duration {
        self.total.end.saturating_sub(self.total.start)
    }

    /// Measures media segments, each with the presentation time it spans.
    fn measure(media: &[(Arc<SegmentData>, Range<Duration>)]) -> Self {
        let mut status = Self {
            total: BufferTotal {
                segments: media.len(),
                bytes: Some(0),
                start: media.first().map_or(Duration::ZERO, |(_, span)| span.start),
                end: media.last().map_or(Duration::ZERO, |(_, span)| span.end),
            },
            downloaded: BufferAmount::default(),
            ranges: Vec::new(),
        };

        let mut previous_downloaded = false;
        for (data, span) in media {
            status.total.bytes = status
                .total
                .bytes
                .zip(data.size())
                .map(|(total, size)| total + size);
            let Some(body) = data.body() else {
                previous_downloaded = false;
                continue;
            };
            // Held now; a body a stream has forgotten no longer counts.
            status.downloaded.segments += 1;
            status.downloaded.bytes += body.len() as u64;
            status.downloaded.time += span.end - span.start;
            match status.ranges.last_mut() {
                Some(range) if previous_downloaded => range.end = span.end,
                _ => status.ranges.push(span.clone()),
            }
            previous_downloaded = true;
        }
        status
    }
}

fn percent(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        return 100.0;
    }
    (part / whole * 100.0).min(100.0)
}

/// Live report of how much of a stream's representation its cache holds.
///
/// Get one from [`SegmentStream::buffer`](crate::segment::SegmentStream::buffer),
/// its [async counterpart](crate::nonblocking::segment::SegmentStream::buffer), or from
/// [`DashSource::buffer`](crate::DashSource::buffer). It reads only the cache,
/// so it plays no part in playback and stays usable from any thread.
#[derive(Clone)]
pub struct BufferHandle(Arc<BufferShared>);

type ChangeCallback = dyn Fn(&BufferStatus) + Send + Sync;

struct BufferShared {
    /// Media segments, each with the presentation time it spans.
    media: Vec<(Arc<SegmentData>, Range<Duration>)>,
    callbacks: Mutex<Vec<Arc<ChangeCallback>>>,
}

impl BufferHandle {
    /// Tracks media segments of `cache`, each with the presentation time it
    /// spans.
    pub(crate) fn new(cache: &Store, media: Vec<(Arc<SegmentData>, Range<Duration>)>) -> Self {
        let shared = Arc::new(BufferShared {
            media,
            callbacks: Mutex::default(),
        });
        // Held weakly, so the subscription lasts as long as a handle does.
        cache.subscribe(Arc::downgrade(&shared) as Weak<dyn CacheListener>);
        Self(shared)
    }

    /// Takes a snapshot of the stream's current download progress.
    ///
    /// This reads shared cache state without performing network I/O. Call it again to see
    /// later progress, or register [`Self::on_change`] for notifications.
    pub fn status(&self) -> BufferStatus {
        self.0.status()
    }

    /// Registers a callback for subsequent changes to this stream's cached data.
    ///
    /// The callback receives a fresh [`BufferStatus`] after one of the stream's segments
    /// downloads, is released, or has its size learned by a HEAD request. Registering it
    /// does not immediately invoke it; call [`Self::status`] for the initial value.
    ///
    /// It runs on the download thread that changed the cache, so it should return quickly
    /// and must not assume a particular thread. Every registered callback remains active
    /// for as long as the shared buffer state exists; this method does not return an
    /// unsubscribe handle.
    pub fn on_change(&self, callback: impl Fn(&BufferStatus) + Send + Sync + 'static) {
        self.0
            .callbacks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::new(callback));
    }
}

impl BufferShared {
    fn status(&self) -> BufferStatus {
        BufferStatus::measure(&self.media)
    }

    fn changed(&self) {
        let callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if callbacks.is_empty() {
            return;
        }
        let status = self.status();
        for callback in callbacks {
            callback(&status);
        }
    }
}

impl CacheListener for BufferShared {
    fn segment_changed(&self, data: &SegmentData) {
        if self
            .media
            .iter()
            .any(|(media, _)| std::ptr::eq(&**media, data))
        {
            self.changed();
        }
    }
}

impl fmt::Debug for BufferHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BufferHandle").field(&self.status()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentLocation;

    type Media = Vec<(Arc<SegmentData>, Range<Duration>)>;

    /// Two-second segments from `start` seconds, with bodies of `bodies` bytes.
    fn media_from<const N: usize>(
        start: u64,
        locations: [SegmentLocation; N],
        bodies: [Option<usize>; N],
    ) -> Media {
        locations
            .into_iter()
            .zip(bodies)
            .enumerate()
            .map(|(index, (location, body))| {
                let data = SegmentData::new(location);
                if let Some(len) = body {
                    data.set_body(&vec![0; len]);
                }
                let begin = Duration::from_secs(start + 2 * index as u64);
                (Arc::new(data), begin..begin + Duration::from_secs(2))
            })
            .collect()
    }

    /// Three two-second segments from time zero.
    fn media(locations: [SegmentLocation; 3], bodies: [Option<usize>; 3]) -> Media {
        media_from(0, locations, bodies)
    }

    fn unranged(bodies: [Option<usize>; 3]) -> Media {
        media(["a".into(), "b".into(), "c".into()], bodies)
    }

    fn secs(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    #[test]
    fn nothing_downloaded_buffers_nothing() {
        let status = BufferStatus::measure(&unranged([None; 3]));
        assert_eq!(status.total.segments, 3);
        assert_eq!(status.total.bytes, None);
        assert_eq!(status.total.start, secs(0.0));
        assert_eq!(status.total.end, secs(6.0));
        assert_eq!(status.downloaded, BufferAmount::default());
        assert!(status.ranges.is_empty());
        assert_eq!(status.buffered_until(secs(3.0)), secs(3.0));
        assert_eq!(status.buffered_percent(secs(3.0)), 50.0);
        assert_eq!(status.downloaded_bytes_percent(), None);
        assert!(!status.is_complete());
    }

    #[test]
    fn buffered_until_runs_to_the_end_of_the_range_holding_the_position() {
        let status = BufferStatus::measure(&unranged([Some(100), None, Some(300)]));
        assert_eq!(
            status.downloaded,
            BufferAmount {
                segments: 2,
                bytes: 400,
                time: secs(4.0)
            }
        );
        assert_eq!(status.ranges, [secs(0.0)..secs(2.0), secs(4.0)..secs(6.0)]);
        assert!((status.downloaded_percent() - 200.0 / 3.0).abs() < 1e-9);

        assert_eq!(status.buffered_until(secs(1.0)), secs(2.0));
        assert!((status.buffered_percent(secs(1.0)) - 100.0 / 3.0).abs() < 1e-9);
        // In the gap, nothing is buffered.
        assert_eq!(status.buffered_until(secs(3.0)), secs(3.0));
        // Past the gap, the range reaches the end without the whole stream.
        assert_eq!(status.buffered_until(secs(5.0)), secs(6.0));
        assert_eq!(status.buffered_percent(secs(5.0)), 100.0);
        assert!(!status.is_complete());
    }

    #[test]
    fn adjacent_segments_merge_into_one_range() {
        let status = BufferStatus::measure(&unranged([Some(10), Some(10), Some(10)]));
        assert_eq!(status.ranges, [secs(0.0)..secs(6.0)]);
        assert_eq!(status.buffered_until(Duration::ZERO), secs(6.0));
        assert_eq!(status.total.bytes, Some(30));
        assert_eq!(status.downloaded_bytes_percent(), Some(100.0));
        assert!(status.is_complete());
    }

    #[test]
    fn byte_ranges_give_total_bytes_before_download() {
        let media = media(
            [
                SegmentLocation::with_range("f", 0..=99),
                SegmentLocation::with_range("f", 100..=199),
                SegmentLocation::with_range("f", 200..=499),
            ],
            [Some(100), None, None],
        );
        let status = BufferStatus::measure(&media);
        assert_eq!(status.total.bytes, Some(500));
        assert_eq!(status.downloaded_bytes_percent(), Some(20.0));
    }

    #[test]
    fn position_past_the_end_is_clamped() {
        let status = BufferStatus::measure(&unranged([None; 3]));
        assert_eq!(status.buffered_until(secs(60.0)), secs(6.0));
        assert_eq!(status.buffered_percent(secs(60.0)), 100.0);
    }

    #[test]
    fn media_starting_after_zero_measures_from_its_start() {
        // Three two-second segments from 10 seconds, the first downloaded.
        let media = media_from(
            10,
            ["a".into(), "b".into(), "c".into()],
            [Some(1), None, None],
        );
        let status = BufferStatus::measure(&media);
        assert_eq!(
            (status.total.start, status.total.end),
            (secs(10.0), secs(16.0))
        );
        assert_eq!(status.ranges, [secs(10.0)..secs(12.0)]);
        assert!((status.buffered_percent(secs(11.0)) - 100.0 / 3.0).abs() < 1e-9);
        assert!((status.downloaded_percent() - 100.0 / 3.0).abs() < 1e-9);
        // Before the media, the position moves to its start.
        assert_eq!(status.buffered_until(secs(1.0)), secs(12.0));
    }

    #[test]
    fn buffered_until_all_waits_on_the_least_buffered_stream() {
        let video = BufferStatus::measure(&unranged([Some(1), Some(1), None]));
        let audio = BufferStatus::measure(&unranged([Some(1), None, None]));
        let until = |statuses: &[&BufferStatus], position| {
            BufferStatus::buffered_until_all(statuses.iter().copied(), position)
        };
        assert_eq!(until(&[&video, &audio], secs(1.0)), secs(2.0));
        assert_eq!(until(&[&video], secs(1.0)), secs(4.0));
        // Past audio's buffer, playback waits right where it is.
        assert_eq!(until(&[&video, &audio], secs(3.0)), secs(3.0));
        assert_eq!(until(&[], secs(3.0)), secs(3.0));
    }

    #[test]
    fn buffered_until_all_ignores_streams_buffered_to_their_end() {
        // Audio ends at 4 seconds and is fully buffered; video runs to 6.
        let audio = BufferStatus::measure(&unranged([Some(1), Some(1), None])[..2]);
        let video = BufferStatus::measure(&unranged([Some(1), Some(1), None]));
        assert_eq!(
            BufferStatus::buffered_until_all([&audio, &video], secs(4.5)),
            secs(4.5)
        );
        assert_eq!(
            BufferStatus::buffered_until_all([&audio, &video], secs(1.0)),
            secs(4.0)
        );

        // Everything buffered to its end: the latest end.
        let complete = BufferStatus::measure(&unranged([Some(1); 3]));
        assert_eq!(
            BufferStatus::buffered_until_all([&audio, &complete], secs(1.0)),
            secs(6.0)
        );
        assert_eq!(
            BufferStatus::buffered_until_all([&audio, &complete], secs(60.0)),
            secs(6.0)
        );
    }
}
