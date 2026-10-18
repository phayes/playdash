//! Packets: a track's coded frames with their timing, as a
//! [`PacketStream`](crate::packet::PacketStream) yields them.

use bytes::Bytes;
use std::time::Duration;

/// One coded frame of a track, with its timing.
///
/// Timestamps count `timescale` ticks of presentation time, the clock of
/// [`Fragments::span`](crate::fragment::Fragments::span) and `seek_time`, shared by every
/// representation of a manifest. Packets come in decode order.
///
/// `duration` is the packet's duration as the container records it. Of that, the first
/// `trim_start` and last `trim_end` ticks are encoder priming and padding to drop from
/// the decoded output. A packet that is all priming has a negative `pts` and a
/// `trim_start` equal to its `duration`; it must still be decoded. A stream's last
/// packet may record less than the codec decodes from it, the rest being padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Compressed sample exactly as MP4 stores it.
    /// It shares memory with its source fragment, which remains allocated with the packet.
    pub data: Bytes,
    /// When decoded output should be presented, in [`Self::timescale`] ticks.
    /// This can be negative for codec priming before presentation time zero.
    pub pts: i64,
    /// When this sample should be decoded, in [`Self::timescale`] ticks.
    /// Packets are yielded in this order.
    pub dts: i64,
    /// Full decoded duration in [`Self::timescale`] ticks, before applying the trims.
    pub duration: u64,
    /// Number of timestamp ticks per second for every timing field in this packet.
    pub timescale: u32,
    /// Whether decoding can begin here without an earlier video frame.
    /// Audio packets are always marked as keyframes.
    pub keyframe: bool,
    /// Ticks of decoded output to remove from the start, usually encoder priming.
    pub trim_start: u64,
    /// Ticks of decoded output to remove from the end, usually encoder padding.
    pub trim_end: u64,
}

impl Packet {
    /// Converts [`Self::pts`] to presentation time.
    /// Returns `None` for a negative timestamp or zero timescale.
    pub fn presentation_time(&self) -> Option<Duration> {
        ticks_to_duration(self.pts, self.timescale)
    }
}

/// A [`PacketStream`](crate::packet::PacketStream) seek's landing point, and the packets
/// to decode before continuing.
///
/// Feed `preroll` to a reset decoder in order and discard its output, then decode the
/// stream's packets as usual, discarding any output presented before
/// `discard_before`: drop output for which [`Self::discards`] is true, and for audio,
/// [`Self::trim_start_of`] ticks from the start of each packet's output. Video packets
/// after the landing point can still present earlier, as B-frames do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketSeeked {
    /// Original wall-clock position passed to `seek_time` or `seek_keyframe`.
    pub requested: Duration,
    /// Presentation timestamp of the stream's next packet, in [`Self::timescale`] ticks.
    /// It may be earlier than [`Self::requested`] for a keyframe seek.
    pub landed: i64,
    /// Presentation timestamp before which decoded output must not be shown.
    ///
    /// In [`Self::timescale`] ticks. This is the requested time for an exact seek and the
    /// landing timestamp for a keyframe seek.
    pub discard_before: i64,
    /// Number of ticks per second for [`Self::landed`] and [`Self::discard_before`].
    pub timescale: u32,
    /// MP4 packets that must be decoded before the stream's next packet.
    /// Discard all output produced from this list.
    pub preroll: Vec<Packet>,
}

impl PacketSeeked {
    /// Converts [`Self::landed`] to presentation time.
    /// Returns `None` for a negative timestamp or zero timescale.
    pub fn landed_time(&self) -> Option<Duration> {
        ticks_to_duration(self.landed, self.timescale)
    }

    /// Converts [`Self::discard_before`] to presentation time.
    /// Returns `None` for a negative timestamp or zero timescale.
    pub fn discard_before_time(&self) -> Option<Duration> {
        ticks_to_duration(self.discard_before, self.timescale)
    }

    /// Whether decoded output presented from `pts` for `duration` ticks is discarded
    /// whole: it starts before `discard_before` and ends at or before it. Output that
    /// spans `discard_before` is kept; for audio, trim it with [`Self::trim_start_of`].
    pub fn discards(&self, pts: i64, duration: u64) -> bool {
        discards(self.discard_before, pts, duration)
    }

    /// Ticks to drop from the start of `packet`'s decoded output: its own `trim_start`,
    /// or more to discard what presents before `discard_before`, up to its whole
    /// `duration`. For audio, where output is decoded a packet at a time.
    pub fn trim_start_of(&self, packet: &Packet) -> u64 {
        trim_start_of(
            self.discard_before,
            packet.pts,
            packet.duration,
            packet.trim_start,
        )
    }
}

/// Whether output presented from `pts` for `duration` ticks lies wholly before
/// `discard_before`.
pub(crate) fn discards(discard_before: i64, pts: i64, duration: u64) -> bool {
    let end = i128::from(pts) + i128::from(duration);
    pts < discard_before && end <= i128::from(discard_before)
}

/// Ticks to drop from the start of output presented from `pts` for `duration` ticks:
/// `trim_start`, or the part before `discard_before` if that is longer.
pub(crate) fn trim_start_of(discard_before: i64, pts: i64, duration: u64, trim_start: u64) -> u64 {
    let before = (i128::from(discard_before) - i128::from(pts)).clamp(0, i128::from(duration));
    trim_start.max(before as u64)
}

/// `ticks` of a `timescale` clock as a time, or `None` if negative or the timescale
/// is zero.
pub(crate) fn ticks_to_duration(ticks: i64, timescale: u32) -> Option<Duration> {
    let ticks = u64::try_from(ticks).ok()?;
    let timescale = u64::from(timescale);
    if timescale == 0 {
        return None;
    }
    let nanos = (u128::from(ticks % timescale) * 1_000_000_000 / u128::from(timescale)) as u32;
    Some(Duration::new(ticks / timescale, nanos))
}

/// `time` in ticks of a `timescale` clock, rounded to the nearest tick, so a time
/// from [`ticks_to_duration`] converts back to the same ticks.
pub(crate) fn duration_to_ticks(time: Duration, timescale: u32) -> i64 {
    let ticks = (time.as_nanos() * u128::from(timescale) + 500_000_000) / 1_000_000_000;
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_and_times_round_trip() {
        assert_eq!(
            ticks_to_duration(48_000, 48_000),
            Some(Duration::from_secs(1))
        );
        assert_eq!(ticks_to_duration(-1, 48_000), None);
        assert_eq!(ticks_to_duration(1, 0), None);
        assert_eq!(duration_to_ticks(Duration::from_millis(1_500), 1000), 1500);
        // Times round to the nearest tick.
        assert_eq!(duration_to_ticks(Duration::from_nanos(1), 3), 0);
        assert_eq!(duration_to_ticks(Duration::from_millis(400), 3), 1);
        for ticks in [0, 1, 2, 3, 29_999, 90_001] {
            let time = ticks_to_duration(ticks, 30_000).unwrap();
            assert_eq!(duration_to_ticks(time, 30_000), ticks);
        }
    }

    /// A seek landing at `landed` that discards output before `discard_before`, at 1000
    /// ticks a second.
    fn seeked(landed: i64, discard_before: i64) -> PacketSeeked {
        PacketSeeked {
            requested: Duration::ZERO,
            landed,
            discard_before,
            timescale: 1000,
            preroll: Vec::new(),
        }
    }

    #[test]
    fn seek_times_are_durations_unless_negative() {
        let seeked = seeked(1_500, 1_520);
        assert_eq!(seeked.landed_time(), Some(Duration::from_millis(1_500)));
        assert_eq!(
            seeked.discard_before_time(),
            Some(Duration::from_millis(1_520))
        );
        assert_eq!(self::seeked(-10, 0).landed_time(), None);
    }

    #[test]
    fn output_wholly_before_discard_before_is_discarded() {
        let seeked = seeked(90, 100);
        assert!(seeked.discards(80, 10));
        // Ending at discard_before, and an instant before it.
        assert!(seeked.discards(90, 10));
        assert!(seeked.discards(99, 0));
        // Spanning it, and from it on.
        assert!(!seeked.discards(95, 10));
        assert!(!seeked.discards(100, 0));
        assert!(!seeked.discards(100, 10));
    }

    #[test]
    fn trims_cover_the_output_before_discard_before_and_the_priming() {
        let seeked = seeked(90, 100);
        let packet = |pts, duration, trim_start| Packet {
            data: Bytes::new(),
            pts,
            dts: pts,
            duration,
            timescale: 1000,
            keyframe: true,
            trim_start,
            trim_end: 0,
        };
        assert_eq!(seeked.trim_start_of(&packet(90, 20, 0)), 10);
        assert_eq!(seeked.trim_start_of(&packet(50, 20, 0)), 20);
        assert_eq!(seeked.trim_start_of(&packet(100, 20, 5)), 5);
        assert_eq!(seeked.trim_start_of(&packet(95, 20, 2)), 5);
        assert_eq!(seeked.trim_start_of(&packet(95, 20, 8)), 8);
    }
}
