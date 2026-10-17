//! Packets: a track's coded frames with their timing, as a `PacketStream` yields them.

use bytes::Bytes;
use std::time::Duration;

/// One coded frame of a track, with its timing.
///
/// Timestamps count `timescale` ticks on the same media clock as
/// [`MediaTimeline`](crate::MediaTimeline) and `seek_time`: zero is the start of the
/// representation's first media fragment. Packets come in decode order.
///
/// `duration` is the packet's duration as the container records it. Of that, the first
/// `trim_start` and last `trim_end` ticks are encoder priming and padding to drop from
/// the decoded output. A packet that is all priming has a negative `pts` and a
/// `trim_start` equal to its `duration`; it must still be decoded. A stream's last
/// packet may record less than the codec decodes from it, the rest being padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// The coded frame. It shares memory with the fragment it came from, which stays
    /// allocated while the packet does.
    pub data: Bytes,
    /// Presentation timestamp in `timescale` ticks.
    pub pts: i64,
    /// Decode timestamp in `timescale` ticks.
    pub dts: i64,
    /// Duration in `timescale` ticks, including trimmed ticks.
    pub duration: u64,
    /// Ticks per second of `pts`, `dts`, `duration` and the trims.
    pub timescale: u32,
    /// Whether decoding can start at this packet. Every audio packet is a keyframe.
    pub keyframe: bool,
    /// Ticks of decoded output to drop from the start.
    pub trim_start: u64,
    /// Ticks of decoded output to drop from the end.
    pub trim_end: u64,
}

impl Packet {
    /// The presentation timestamp as a time, or `None` if it is negative.
    pub fn presentation_time(&self) -> Option<Duration> {
        ticks_to_duration(self.pts, self.timescale)
    }
}

/// A `PacketStream` seek's landing point, and the packets to decode before continuing.
///
/// Feed `preroll` to a reset decoder in order and discard its output, then decode the
/// stream's packets as usual, discarding any output presented before
/// `discard_before`. Video packets after the landing point can still present earlier,
/// as B-frames do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seeked {
    /// The time the seek asked for.
    pub requested: Duration,
    /// Presentation timestamp of the packet the stream yields next, in its timescale.
    pub landed: i64,
    /// Presentation timestamp before which decoded output is discarded, in the
    /// stream's timescale: the requested time for a packet-accurate seek, `landed` for
    /// a fragment-accurate one.
    pub discard_before: i64,
    /// Packets to decode, in order, before the stream's next packet. None of their
    /// output is presented.
    pub preroll: Vec<Packet>,
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
}
