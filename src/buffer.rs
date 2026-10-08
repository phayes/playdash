//! Download progress of a DASH stream, by fragment, byte and media time.

use crate::stream::{CacheListener, FragmentCache, FragmentData, MediaTimeline};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// How much of a stream its cache holds.
///
/// Only media fragments count; the initialization segment is not media.
///
/// # Examples
///
/// ```no_run
/// use playdash::{DashManifest, DashSource};
/// use std::time::Duration;
///
/// # fn example() -> Result<(), playdash::Error> {
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferStatus {
    /// The whole stream.
    pub total: BufferTotal,
    /// Downloaded fragments anywhere in the stream.
    pub downloaded: BufferAmount,
    /// Downloaded spans of media time, in order, with adjacent fragments
    /// merged into one span.
    pub ranges: Vec<Range<Duration>>,
}

/// An amount of downloaded media.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferAmount {
    /// Media fragments.
    pub fragments: usize,
    /// Bytes of fragment bodies.
    pub bytes: u64,
    /// Media time.
    pub time: Duration,
}

/// The size of a whole stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferTotal {
    /// Media fragments.
    pub fragments: usize,
    /// Bytes of all media fragments, once every fragment's size is known
    /// from its byte range, a HEAD or its download.
    pub bytes: Option<u64>,
    /// Media time.
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

    /// [`Self::buffered_until`] as a percentage (0 to 100) of the stream's
    /// duration, as drawn by a player's buffer bar.
    pub fn buffered_percent(&self, position: Duration) -> f64 {
        percent(
            self.buffered_until(position).as_secs_f64(),
            self.total.time.as_secs_f64(),
        )
    }

    /// Percentage (0 to 100) of the stream's media time downloaded,
    /// anywhere in the stream.
    pub fn downloaded_percent(&self) -> f64 {
        percent(
            self.downloaded.time.as_secs_f64(),
            self.total.time.as_secs_f64(),
        )
    }

    /// Percentage (0 to 100) of the stream's bytes downloaded, once
    /// [`BufferTotal::bytes`] is known.
    pub fn downloaded_bytes_percent(&self) -> Option<f64> {
        let total = self.total.bytes?;
        Some(percent(self.downloaded.bytes as f64, total as f64))
    }

    /// Whether every media fragment is downloaded.
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

/// Live report of how much of a `DashSource`'s or
/// [`StreamReader`](crate::StreamReader)'s stream its cache holds.
///
/// Get one from `DashSource::buffer` (with the `rodio` feature) or
/// [`StreamReader::buffer`](crate::StreamReader::buffer). It reads only the
/// cache, so it plays no part in playback and stays usable from any thread.
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
    /// Tracks `media` fragments of `cache`, timed by `timeline`.
    pub(crate) fn new(
        cache: &FragmentCache,
        media: Vec<Arc<FragmentData>>,
        timeline: &MediaTimeline,
    ) -> Self {
        // Fragments the timeline lacks take no time, at its end.
        let mut spans = timeline.spans();
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

    /// How much of the stream the cache holds now.
    pub fn status(&self) -> BufferStatus {
        self.0.status()
    }

    /// Calls `callback` with fresh progress after each change to the cache:
    /// one of the stream's fragments downloads, or has its size learned by a
    /// HEAD request.
    ///
    /// It runs on the download thread that changed the cache, so keep it
    /// quick. It is not called for changes before it was added; call
    /// [`Self::status`] for the current progress.
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
    use crate::Fragment;

    /// Three two-second fragments.
    fn spans() -> Vec<Range<Duration>> {
        MediaTimeline {
            timescale: 10,
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
}
