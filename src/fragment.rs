//! Fragments and media timing: what a representation is made of, independent of how it is
//! fetched.

use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Range, RangeInclusive};
use std::time::Duration;

/// Media-segment timing used to map playback positions to fragments.
///
/// Durations are expressed in `timescale` ticks. For example, a timescale of
/// `1_000` makes a duration of `2_500` equal to 2.5 seconds. Time zero is the
/// start of the first media fragment.
///
/// # Examples
///
/// ```
/// use playdash::MediaTimeline;
///
/// let timeline = MediaTimeline {
///     timescale: 1_000,
///     origin: 0,
///     media_durations: vec![2_500, 2_500],
/// };
///
/// // Estimate storage for a 128 kbit/s, five-second stream.
/// assert!(timeline.estimate_len(1_024, 128_000).is_some());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTimeline {
    /// DASH timescale: ticks per second.
    pub timescale: u32,
    /// Media time at the start of the first media fragment, in `timescale`
    /// ticks: the timestamp the media itself gives that instant, including any
    /// `@presentationTimeOffset`. Subtract it from a media timestamp, such as
    /// an MP4 decode time, to place that timestamp on this timeline.
    pub origin: u64,
    /// Duration of each media fragment, in timescale ticks, in playback order.
    pub media_durations: Vec<u64>,
}

/// Padding applied to advertised media bytes when guessing total length.
/// AAC `bandwidth` is near the real rate; FLAC's is often uncompressed PCM.
const SYMPHONIA_COMPAT_MEDIA_FACTOR: u128 = 4;

/// Extra bytes reserved per media fragment for `moof` / box overhead.
const SYMPHONIA_COMPAT_BYTES_PER_FRAGMENT: u128 = 64 * 1024;

impl MediaTimeline {
    fn ticks_to_duration(&self, ticks: u64) -> Option<Duration> {
        if self.timescale == 0 {
            return None;
        }
        let timescale = u64::from(self.timescale);
        let seconds = ticks / timescale;
        let nanos = (u128::from(ticks % timescale) * 1_000_000_000 / u128::from(timescale)) as u32;
        Some(Duration::new(seconds, nanos))
    }

    /// Media time spanned by each media fragment, in playback order. Empty
    /// if the timescale is zero.
    pub fn spans(&self) -> Vec<Range<Duration>> {
        let mut ticks = 0u64;
        self.media_durations
            .iter()
            .map_while(|&duration| {
                let start = self.ticks_to_duration(ticks)?;
                ticks = ticks.saturating_add(duration);
                Some(start..self.ticks_to_duration(ticks)?)
            })
            .collect()
    }

    /// Finds the media fragment whose media covers `time`, as an index into
    /// [`Fragments::media`]. A time at or past the end lands in the last
    /// fragment. `None` for an empty timeline.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use playdash::MediaTimeline;
    ///
    /// let timeline = MediaTimeline {
    ///     timescale: 1_000,
    ///     origin: 0,
    ///     media_durations: vec![2_000, 2_000],
    /// };
    /// assert_eq!(timeline.fragment_at(Duration::ZERO), Some(0));
    /// assert_eq!(timeline.fragment_at(Duration::from_millis(2_500)), Some(1));
    /// assert_eq!(timeline.fragment_at(Duration::from_secs(60)), Some(1));
    /// ```
    pub fn fragment_at(&self, time: Duration) -> Option<usize> {
        self.span_at(time).map(|(index, _)| index)
    }

    /// The media fragment covering `time`, as for [`Self::fragment_at`], and
    /// the media time it spans.
    pub(crate) fn span_at(&self, time: Duration) -> Option<(usize, Range<Duration>)> {
        let mut spans = self.spans();
        let last = spans.len().checked_sub(1)?;
        let index = spans.partition_point(|span| span.end <= time).min(last);
        Some((index, spans.swap_remove(index)))
    }

    /// Total media duration, or `None` if the timescale is zero.
    pub fn total_duration(&self) -> Option<Duration> {
        self.ticks_to_duration(self.media_durations.iter().copied().sum())
    }

    /// Returns a conservative stream-length estimate for a demuxer that needs
    /// a length before all fragments have been downloaded.
    ///
    /// Returns `None` when the bitrate, timescale, or media durations are empty.
    pub fn estimate_len(&self, init_len: u64, bitrate_bps: u32) -> Option<u64> {
        if bitrate_bps == 0 || self.timescale == 0 || self.media_durations.is_empty() {
            return None;
        }
        let ticks: u128 = self.media_durations.iter().copied().map(u128::from).sum();
        let fragments = u128::from(self.media_durations.len() as u64);
        let denom = u128::from(self.timescale) * 8;
        let media = (u128::from(bitrate_bps) * ticks).div_ceil(denom);
        let padded = media
            .saturating_mul(SYMPHONIA_COMPAT_MEDIA_FACTOR)
            .saturating_add(fragments.saturating_mul(SYMPHONIA_COMPAT_BYTES_PER_FRAGMENT))
            .saturating_add(u128::from(init_len));
        Some(u64::try_from(padded).unwrap_or(u64::MAX))
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

/// A media resource, optionally limited to an inclusive byte range.
///
/// # Examples
///
/// ```
/// use playdash::Fragment;
///
/// let whole = Fragment::new("https://media.example/init.mp4");
/// assert_eq!(whole.known_size(), None);
///
/// let range = Fragment::with_range("https://media.example/audio.mp4", 100..=599);
/// assert_eq!(range.known_size(), Some(500));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fragment {
    /// Absolute URL of the resource.
    pub url: String,
    /// Inclusive byte range within the resource, as in an HTTP `Range`
    /// header, or `None` for the whole body. Must not be empty.
    pub range: Option<RangeInclusive<u64>>,
}

impl Fragment {
    /// The whole body at `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            range: None,
        }
    }

    /// The bytes of `url` covered by `range`.
    pub fn with_range(url: impl Into<String>, range: RangeInclusive<u64>) -> Self {
        Self {
            url: url.into(),
            range: Some(range),
        }
    }

    /// Byte length, when the range makes it known in advance.
    pub fn known_size(&self) -> Option<u64> {
        self.range
            .as_ref()
            .and_then(|range| range.end().checked_sub(*range.start())?.checked_add(1))
    }
}

impl From<String> for Fragment {
    fn from(url: String) -> Self {
        Self::new(url)
    }
}

impl From<&str> for Fragment {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

impl fmt::Display for Fragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.range {
            Some(range) => write!(f, "{} (bytes {}-{})", self.url, range.start(), range.end()),
            None => f.write_str(&self.url),
        }
    }
}

/// A representation's fragments: its initialization fragment and its media
/// fragments. A stream of them reads the initialization fragment first, then
/// the media fragments in order.
///
/// # Examples
///
/// ```
/// use playdash::{Fragment, Fragments};
///
/// let fragments = Fragments::new(
///     "https://media.example/init.mp4",
///     ["https://media.example/1.m4s", "https://media.example/2.m4s"],
/// );
/// assert_eq!(fragments.init, Fragment::new("https://media.example/init.mp4"));
/// assert_eq!(fragments.media.len(), 2);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fragments {
    /// Initialization fragment, such as the `ftyp` and `moov` of fragmented MP4.
    pub init: Fragment,
    /// Media fragments in playback order, one per entry of the
    /// representation's [`MediaTimeline::media_durations`].
    pub media: Vec<Fragment>,
}

impl Fragments {
    /// Creates the fragments of a representation from its initialization
    /// fragment and its media fragments in playback order.
    pub fn new(
        init: impl Into<Fragment>,
        media: impl IntoIterator<Item = impl Into<Fragment>>,
    ) -> Self {
        Self {
            init: init.into(),
            media: media.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_second_timeline() -> MediaTimeline {
        MediaTimeline {
            timescale: 44_100,
            origin: 0,
            media_durations: vec![44_100, 44_100],
        }
    }

    #[test]
    fn estimate_len_errs_larger_than_advertised_media() {
        let timeline = two_second_timeline();
        let init_len = 4;
        let bitrate = 128_000;
        let raw_media = u64::from(bitrate) * 2 / 8;
        let estimate = timeline.estimate_len(init_len, bitrate).unwrap();
        assert!(
            estimate >= init_len + raw_media * 4 + 2 * 64 * 1024,
            "estimate {estimate} should pad raw media {raw_media}"
        );
        assert!(timeline.estimate_len(init_len, 0).is_none());
    }

    #[test]
    fn fragment_at_indexes_media_fragments() {
        let timeline = two_second_timeline();
        let at = |millis| timeline.fragment_at(Duration::from_millis(millis));
        assert_eq!(at(0), Some(0));
        assert_eq!(at(999), Some(0));
        assert_eq!(at(1_000), Some(1));
        assert_eq!(at(2_000), Some(1));
        assert_eq!(at(60_000), Some(1));
        assert_eq!(timeline.total_duration(), Some(Duration::from_secs(2)));

        let empty = MediaTimeline {
            timescale: 1_000,
            origin: 0,
            media_durations: Vec::new(),
        };
        assert_eq!(empty.fragment_at(Duration::ZERO), None);
        let unscaled = MediaTimeline {
            timescale: 0,
            ..two_second_timeline()
        };
        assert_eq!(unscaled.fragment_at(Duration::ZERO), None);
        assert_eq!(unscaled.total_duration(), None);
    }

    #[test]
    fn byte_range_parses_dash_form() {
        let range = parse_byte_range(" 100-199 ").unwrap();
        assert_eq!(range, 100..=199);
        assert_eq!(
            Fragment::with_range("a", range.clone()).known_size(),
            Some(100)
        );
        assert_eq!(Fragment::with_range("a", 5..=5).known_size(), Some(1));
        for invalid in ["", "5", "5-", "-5", "9-3", "a-b"] {
            assert!(
                matches!(parse_byte_range(invalid), Err(Error::InvalidByteRange(value)) if value == invalid),
                "{invalid:?} should be rejected"
            );
        }
        assert_eq!(
            Fragment::with_range("media.mp4", range).to_string(),
            "media.mp4 (bytes 100-199)"
        );
    }
}
