//! Download progress of a DASH stream, by fragment, byte and media time.

use super::store::{CacheListener, FragmentData, Store};
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
///     "buffered to {:?} ({:.0}%), {} of {} fragments downloaded",
///     status.buffered_until(position),
///     status.buffered_percent(position),
///     status.downloaded.fragments,
///     status.total.fragments,
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
    /// Cached ranges on the representation's media timeline.
    /// Ranges are ordered, non-overlapping, and merge adjacent downloaded segments.
    pub ranges: Vec<Range<Duration>>,
}

/// An amount of downloaded media.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferAmount {
    /// Number of media segments represented by this amount.
    pub fragments: usize,
    /// Bytes in those media-segment bodies. The initialization segment is not counted.
    pub bytes: u64,
    /// Sum of the media durations covered by those segments.
    pub time: Duration,
}

/// The size of a whole stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferTotal {
    /// Total number of media segments in the representation.
    pub fragments: usize,
    /// Combined byte length of all media segments, excluding initialization.
    /// `None` until every segment's size is known from a byte range, HEAD, or download.
    pub bytes: Option<u64>,
    /// Total duration of the representation's media timeline.
    pub time: Duration,
}

impl BufferStatus {
    /// Media time up to which playback can continue from `position` without
    /// waiting on the network: the end of the downloaded range holding
    /// `position`, else `position` itself.
    pub fn buffered_until(&self, position: Duration) -> Duration {
        let position = position.min(self.total.time);
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
    ///     total: BufferTotal { fragments: 5, bytes: None, time: secs(10) },
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
            latest_end = latest_end.max(Some(status.total.time));
            // Buffered to its end, a stream has nothing left to wait for.
            if until < status.total.time {
                limit = Some(limit.map_or(until, |limit| limit.min(until)));
            }
        }
        limit.or(latest_end).unwrap_or(position)
    }

    /// Returns [`Self::buffered_until`] as a percentage of the whole duration.
    ///
    /// The result is clamped to `0.0..=100.0`; an empty stream reports `100.0`.
    pub fn buffered_percent(&self, position: Duration) -> f64 {
        percent(
            self.buffered_until(position).as_secs_f64(),
            self.total.time.as_secs_f64(),
        )
    }

    /// Returns the percentage of media time cached anywhere in the stream.
    ///
    /// Unlike [`Self::buffered_percent`], this includes downloaded ranges that are not
    /// contiguous with the playback position. The result is clamped to `0.0..=100.0`;
    /// an empty stream reports `100.0`.
    pub fn downloaded_percent(&self) -> f64 {
        percent(
            self.downloaded.time.as_secs_f64(),
            self.total.time.as_secs_f64(),
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
        self.downloaded.fragments == self.total.fragments
    }

    /// Measures `media` fragments, each spanning the same-index entry of
    /// `spans`.
    fn measure(media: &[Arc<FragmentData>], spans: &[Range<Duration>]) -> Self {
        let mut status = Self {
            total: BufferTotal {
                fragments: media.len(),
                bytes: Some(0),
                time: spans.last().map_or(Duration::ZERO, |span| span.end),
            },
            downloaded: BufferAmount::default(),
            ranges: Vec::new(),
        };

        let mut previous_downloaded = false;
        for (data, span) in media.iter().zip(spans) {
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
            status.downloaded.fragments += 1;
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
/// Get one from [`FragmentStream::buffer`](crate::fragment::FragmentStream::buffer),
/// its [async counterpart](crate::nonblocking::fragment::FragmentStream::buffer), or from
/// [`DashSource::buffer`](crate::DashSource::buffer). It reads only the cache,
/// so it plays no part in playback and stays usable from any thread.
#[derive(Clone)]
pub struct BufferHandle(Arc<BufferShared>);

type ChangeCallback = dyn Fn(&BufferStatus) + Send + Sync;

struct BufferShared {
    media: Vec<Arc<FragmentData>>,
    /// Media time spanned by each of `media`.
    spans: Vec<Range<Duration>>,
    callbacks: Mutex<Vec<Arc<ChangeCallback>>>,
}

impl BufferHandle {
    /// Tracks `media` fragments of `cache`, each spanning the same-index
    /// entry of `spans`.
    pub(crate) fn new(
        cache: &Store,
        media: Vec<Arc<FragmentData>>,
        mut spans: Vec<Range<Duration>>,
    ) -> Self {
        // Fragments the timeline lacks take no time, at its end.
        let end = spans.last().map_or(Duration::ZERO, |span| span.end);
        spans.resize(media.len(), end..end);

        let shared = Arc::new(BufferShared {
            media,
            spans,
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
        BufferStatus::measure(&self.media, &self.spans)
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
    fn fragment_changed(&self, data: &FragmentData) {
        if self.media.iter().any(|media| std::ptr::eq(&**media, data)) {
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
    use crate::fragment::{Fragment, MediaTimeline};

    /// Three two-second fragments.
    fn spans() -> Vec<Range<Duration>> {
        MediaTimeline {
            timescale: 10,
            origin: 0,
            media_durations: vec![20, 20, 20],
        }
        .spans()
    }

    fn media(fragments: [Fragment; 3], bodies: [Option<usize>; 3]) -> Vec<Arc<FragmentData>> {
        fragments
            .into_iter()
            .zip(bodies)
            .map(|(fragment, body)| {
                let data = FragmentData::new(fragment);
                if let Some(len) = body {
                    data.set_body(&vec![0; len]);
                }
                Arc::new(data)
            })
            .collect()
    }

    fn unranged(bodies: [Option<usize>; 3]) -> Vec<Arc<FragmentData>> {
        media(["a".into(), "b".into(), "c".into()], bodies)
    }

    fn secs(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    #[test]
    fn nothing_downloaded_buffers_nothing() {
        let status = BufferStatus::measure(&unranged([None; 3]), &spans());
        assert_eq!(status.total.fragments, 3);
        assert_eq!(status.total.bytes, None);
        assert_eq!(status.total.time, secs(6.0));
        assert_eq!(status.downloaded, BufferAmount::default());
        assert!(status.ranges.is_empty());
        assert_eq!(status.buffered_until(secs(3.0)), secs(3.0));
        assert_eq!(status.buffered_percent(secs(3.0)), 50.0);
        assert_eq!(status.downloaded_bytes_percent(), None);
        assert!(!status.is_complete());
    }

    #[test]
    fn buffered_until_runs_to_the_end_of_the_range_holding_the_position() {
        let status = BufferStatus::measure(&unranged([Some(100), None, Some(300)]), &spans());
        assert_eq!(
            status.downloaded,
            BufferAmount {
                fragments: 2,
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
    fn adjacent_fragments_merge_into_one_range() {
        let status = BufferStatus::measure(&unranged([Some(10), Some(10), Some(10)]), &spans());
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
                Fragment::with_range("f", 0..=99),
                Fragment::with_range("f", 100..=199),
                Fragment::with_range("f", 200..=499),
            ],
            [Some(100), None, None],
        );
        let status = BufferStatus::measure(&media, &spans());
        assert_eq!(status.total.bytes, Some(500));
        assert_eq!(status.downloaded_bytes_percent(), Some(20.0));
    }

    #[test]
    fn position_past_the_end_is_clamped() {
        let status = BufferStatus::measure(&unranged([None; 3]), &spans());
        assert_eq!(status.buffered_until(secs(60.0)), secs(6.0));
        assert_eq!(status.buffered_percent(secs(60.0)), 100.0);
    }

    #[test]
    fn buffered_until_all_waits_on_the_least_buffered_stream() {
        let video = BufferStatus::measure(&unranged([Some(1), Some(1), None]), &spans());
        let audio = BufferStatus::measure(&unranged([Some(1), None, None]), &spans());
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
        let short = MediaTimeline {
            timescale: 10,
            origin: 0,
            media_durations: vec![20, 20],
        }
        .spans();
        let audio = BufferStatus::measure(&unranged([Some(1), Some(1), None])[..2], &short);
        let video = BufferStatus::measure(&unranged([Some(1), Some(1), None]), &spans());
        assert_eq!(
            BufferStatus::buffered_until_all([&audio, &video], secs(4.5)),
            secs(4.5)
        );
        assert_eq!(
            BufferStatus::buffered_until_all([&audio, &video], secs(1.0)),
            secs(4.0)
        );

        // Everything buffered to its end: the latest end.
        let complete = BufferStatus::measure(&unranged([Some(1); 3]), &spans());
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
