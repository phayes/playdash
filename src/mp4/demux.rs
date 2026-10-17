//! The demuxer behind both `PacketStream`s: one track's packets from its initialization
//! and media fragments, and where a time seek lands. Nothing here does I/O.

use super::codec::{self, Codec};
use super::fragment;
use super::init::{self, RollGroups, SampleEntry, TrackDefaults};
use super::packet::{Packet, PacketSeeked, duration_to_ticks, ticks_to_duration};
use super::reader::ParseError;
use crate::error::Error;
use crate::fragment::{Fragment, MediaTimeline};
use bytes::Bytes;
use std::collections::VecDeque;
use std::io;
use std::ops::RangeInclusive;
use std::time::Duration;

/// A packet, and what seeking needs to know about it.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    packet: Packet,
    /// Whether it is a leading sample that cannot be decoded from the sync sample
    /// before it.
    leading: bool,
}

/// The packets of one media fragment, in decode order.
#[derive(Debug)]
pub(crate) struct Loaded {
    /// The fragment's index among the representation's media fragments.
    index: usize,
    entries: Vec<Entry>,
    /// Pre-roll in samples that the fragment's own `roll` or `prol` group signals.
    roll: Option<u32>,
    /// Decode time just past the fragment's last sample, in media ticks.
    end_decode_time: Option<u64>,
}

impl Loaded {
    /// The fragment's index among the representation's media fragments.
    pub(crate) fn index(&self) -> usize {
        self.index
    }
}

/// Where a seek lands relative to its time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    /// On the first packet in decode order that presents at or after the time.
    Exact,
    /// Past the pre-roll of the last keyframe it can decode from to reach the time.
    Keyframe,
}

/// A seek's next step.
#[derive(Debug)]
pub(crate) enum Split {
    /// The fragment before the earliest one loaded: none has a sync sample early
    /// enough to start decoding at.
    Earlier,
    /// The fragment after the last one loaded: none of their packets presents at or after
    /// the time to land at, as when a timeline that only approximates fragment boundaries
    /// puts the time in a fragment ending before it.
    Later,
    /// The seek is done.
    Done(PacketSeeked),
}

/// One track's packets from its fragments, in decode order.
#[derive(Debug)]
pub(crate) struct Demuxer {
    track_id: u32,
    timescale: u32,
    audio: bool,
    codec: Codec,
    init: Bytes,
    /// The sample entries, which each sample names by 1-based index.
    entries: Vec<SampleEntry>,
    /// The 1-based index of the entry `codec` describes.
    description_index: u32,
    /// Every track's `trex` defaults, which lay out fragments.
    defaults: Vec<(u32, TrackDefaults)>,
    /// `trex` default sample duration, in media ticks.
    sample_duration: u32,
    /// Pre-roll in samples that the initialization's `roll` or `prol` group signals.
    init_roll: Option<u32>,
    /// The representation's timeline, which places fragments in time.
    timeline: Option<MediaTimeline>,
    /// Media time of time zero, in media ticks: the edit-list start and the
    /// timeline's origin.
    shift: i64,
    /// Presentation end from the edit list, past which audio is trimmed.
    end: Option<i64>,
    /// Pre-roll set by the caller, overriding what the media signals.
    preroll: Option<Duration>,
    queue: VecDeque<Entry>,
    /// The next media fragment's index and decode time, for one without `tfdt`.
    next_decode: Option<(usize, u64)>,
    /// After a seek to a sync sample at this presentation time, leading samples
    /// before it are dropped until the next sync sample.
    leading_before: Option<i64>,
    /// For HEVC, the bytes of each NAL unit's length in a sample, to find RASL pictures.
    hevc_length_size: Option<usize>,
}

impl Demuxer {
    /// A demuxer of track `track_id` of `init`, the body of `init_fragment`, or of its
    /// only track without one. `timeline` places its media fragments in time.
    pub(crate) fn new(
        init: Bytes,
        init_fragment: &Fragment,
        timeline: Option<MediaTimeline>,
        track_id: Option<u32>,
    ) -> Result<Self, Error> {
        let error = |error| media_error(init_fragment, error);
        let parsed = init::parse(&init).map_err(error)?;
        let track = match (track_id, &parsed.tracks[..]) {
            (None, [track]) => track,
            (None, tracks) => {
                return Err(Error::UnsupportedMedia {
                    fragment: init_fragment.to_string(),
                    reason: format!(
                        "{} tracks; choose one from FragmentStream::tracks with \
                         PacketStream::with_track",
                        tracks.len()
                    ),
                });
            }
            (Some(id), tracks) => tracks.iter().find(|track| track.id == id).ok_or_else(|| {
                Error::Initialization(format!(
                    "{init_fragment} has no track {id}; it has {:?}",
                    tracks.iter().map(|track| track.id).collect::<Vec<_>>()
                ))
            })?,
        };

        // The sample description fragments use unless they say otherwise.
        let description_index = default_description_index(track);
        let entry = description(&track.entries, description_index).map_err(error)?;
        if entry.is_encrypted() {
            return Err(encrypted(init_fragment));
        }
        let codec = codec::codec(&init, track, &entry).map_err(error)?;

        // Time zero: the edit list's start, then the timeline's origin.
        let edit = match &track.edits[..] {
            [] => None,
            [edit] if edit.media_time >= 0 => Some(*edit),
            _edits => {
                #[cfg(feature = "log")]
                log::warn!(
                    "MP4 track {} has an edit list this crate does not apply: {_edits:?}",
                    track.id
                );
                None
            }
        };
        let origin = timeline.as_ref().map_or(0, |timeline| {
            rescale(timeline.origin, timeline.timescale, track.timescale)
        });
        let shift = edit.map_or(0, |edit| edit.media_time) + origin;
        // A presentation end, only when the movie's duration is known too.
        let end = match (parsed.fragment_duration, edit) {
            (Some(_), Some(edit)) if edit.segment_duration > 0 => Some(
                rescale(
                    edit.segment_duration,
                    parsed.movie_timescale,
                    track.timescale,
                ) - origin,
            ),
            _ => None,
        };

        let audio = &track.handler == b"soun";
        let hevc_length_size = match &codec {
            Codec::Hevc(video) => hevc_length_size(&video.config),
            _ => None,
        };
        Ok(Self {
            track_id: track.id,
            timescale: track.timescale,
            audio,
            codec,
            init: init.clone(),
            entries: track.entries.clone(),
            description_index,
            defaults: parsed
                .tracks
                .iter()
                .map(|track| (track.id, track.defaults))
                .collect(),
            sample_duration: track.defaults.sample_duration,
            init_roll: track.roll.as_ref().filter(|_| audio).and_then(roll_samples),
            timeline,
            shift,
            end,
            preroll: None,
            queue: VecDeque::new(),
            next_decode: None,
            leading_before: None,
            hevc_length_size,
        })
    }

    pub(crate) fn codec(&self) -> &Codec {
        &self.codec
    }

    pub(crate) fn timescale(&self) -> u32 {
        self.timescale
    }

    pub(crate) fn track_id(&self) -> u32 {
        self.track_id
    }

    /// Overrides the pre-roll the media and codec signal.
    pub(crate) fn set_preroll(&mut self, preroll: Duration) {
        self.preroll = Some(preroll);
    }

    /// The pre-roll seeks use: the caller's, else the initialization's `roll` or
    /// `prol` group, else the codec's default.
    pub(crate) fn preroll(&self) -> Duration {
        if let Some(preroll) = self.preroll {
            return preroll;
        }
        match self.init_roll {
            Some(samples) if self.sample_duration > 0 => ticks_to_duration(
                i64::from(samples) * i64::from(self.sample_duration),
                self.timescale,
            )
            .unwrap_or_default(),
            _ => self.codec.default_preroll(),
        }
    }

    /// Parses media fragment `index` of the representation, the body of `fragment`.
    pub(crate) fn load(
        &mut self,
        index: usize,
        body: &Bytes,
        fragment: &Fragment,
    ) -> Result<Loaded, Error> {
        let body_start = fragment.range.as_ref().map_or(0, |range| *range.start());
        let trafs = fragment::parse(body, body_start, &self.defaults)
            .map_err(|error| media_error(fragment, error))?;

        // A fragment without `tfdt` continues from the one before, or starts where
        // the timeline says.
        let mut decode_time = match self.next_decode {
            Some((next, time)) if next == index => Some(time),
            _ => None,
        };
        let mut entries = Vec::new();
        let mut roll = None;
        for traf in trafs.iter().filter(|traf| traf.track_id == self.track_id) {
            let start = match traf.decode_time.or(decode_time) {
                Some(start) => start,
                None => self.timeline_decode_time(index),
            };
            if let Some(groups) = &traf.roll {
                roll = roll_samples(groups).filter(|_| self.audio);
            }
            for sample in &traf.samples {
                self.check_description(sample.description_index, fragment)?;
                entries.push(self.entry(start, sample, body));
            }
            let duration: u64 = traf
                .samples
                .iter()
                .map(|sample| u64::from(sample.duration))
                .sum();
            decode_time = Some(start + duration);
        }
        self.next_decode = decode_time.map(|time| (index + 1, time));
        Ok(Loaded {
            index,
            entries,
            roll,
            end_decode_time: decode_time,
        })
    }

    /// Queues the packets of `loaded`, the fragment after the last one queued.
    pub(crate) fn push(&mut self, loaded: Loaded) {
        for entry in loaded.entries {
            self.enqueue(entry);
        }
    }

    /// The next packet, if one is queued.
    pub(crate) fn pop(&mut self) -> Option<Packet> {
        self.queue.pop_front().map(|entry| entry.packet)
    }

    /// The media fragments a seek to `time` loads first: from where decoding may
    /// have to start, to the one containing `time`.
    pub(crate) fn seek_range(&self, time: Duration) -> io::Result<RangeInclusive<usize>> {
        let timeline = self.timeline.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "MPEG stream has no SegmentTimeline",
            )
        })?;
        let fragment_at = |time| {
            timeline.fragment_at(time).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "MPEG stream timeline is missing a timescale or media durations",
                )
            })
        };
        Ok(fragment_at(time.saturating_sub(self.preroll()))?..=fragment_at(time)?)
    }

    /// Completes a seek to `time` from `loaded`, consecutive fragments ending with the
    /// one containing `time`, landing as `landing` says. Asks for the fragment before
    /// them when none has a sync sample early enough, unless `at_first`: they start at the
    /// stream's first. Asks for the fragment after them when none of their packets
    /// presents at or after the time to land at, unless `at_last`: they end at the
    /// stream's last.
    /// When done, the queue holds the packets from where the seek landed.
    pub(crate) fn seek(
        &mut self,
        time: Duration,
        landing: Landing,
        loaded: &[Loaded],
        at_first: bool,
        at_last: bool,
    ) -> Split {
        let Some(target) = loaded.last() else {
            return Split::Earlier;
        };
        let all: Vec<&Entry> = loaded.iter().flat_map(|loaded| &loaded.entries).collect();
        let preroll = self.preroll_ticks(target);

        // Where decoding starts: the last sync sample a pre-roll before the time, or
        // the stream's first when there is none.
        let time_ticks = duration_to_ticks(time, self.timescale);
        let decode_target = time_ticks - preroll;
        let start = all
            .iter()
            .rposition(|entry| entry.packet.keyframe && entry.packet.pts <= decode_target);
        let start = match start {
            Some(start) => start,
            None if !at_first => return Split::Earlier,
            None => all
                .iter()
                .position(|entry| entry.packet.keyframe)
                .unwrap_or(0),
        };

        // The time to land at: the time itself, or past the start's pre-roll, but never
        // after the time.
        let at = match landing {
            Landing::Exact => time_ticks,
            Landing::Keyframe => all.get(start).map_or(time_ticks, |entry| {
                (entry.packet.pts + preroll).min(time_ticks)
            }),
        };

        // Leading samples of a start, up to the next sync sample, cannot be decoded from
        // it. When one would be shown, start at the sync sample before.
        let next_sync = |start: usize| {
            (start + 1..all.len())
                .find(|&index| all[index].packet.keyframe)
                .unwrap_or(all.len())
        };
        let leading_of = |start: usize, index: usize| {
            let start_pts = all.get(start).map_or(at, |entry| entry.packet.pts);
            (all[index].leading && index < next_sync(start))
                || (!self.audio && all[index].packet.pts < start_pts)
        };
        let mut start = start;
        loop {
            let shown_leading = (start + 1..next_sync(start)).any(|index| {
                let packet = &all[index].packet;
                leading_of(start, index) && packet.pts + packet.duration as i64 > at
            });
            if !shown_leading {
                break;
            }
            match all[..start].iter().rposition(|entry| entry.packet.keyframe) {
                Some(earlier) => start = earlier,
                None if !at_first => return Split::Earlier,
                None => break,
            }
        }

        // The cursor: the first decodable packet from the start that presents at
        // or after the landing time. Leading samples of the start are dropped.
        let start_pts = all.get(start).map_or(at, |entry| entry.packet.pts);
        let leading = |index: usize| leading_of(start, index);
        let cursor = (start..all.len())
            .find(|&index| {
                let packet = &all[index].packet;
                !leading(index) && packet.pts + packet.duration as i64 > at
            })
            .unwrap_or(all.len());
        if cursor == all.len() && !at_last {
            return Split::Later;
        }
        let preroll = (start.min(cursor)..cursor)
            .filter(|&index| index == start || !leading(index))
            .map(|index| all[index].packet.clone())
            .collect();
        let landed = match all.get(cursor) {
            Some(entry) => entry.packet.pts,
            None => all
                .last()
                .map_or(at, |entry| entry.packet.pts + entry.packet.duration as i64),
        };

        // Queue the rest, dropping leading samples of the start until the next sync sample.
        self.queue.clear();
        self.leading_before = (!self.audio && cursor < next_sync(start)).then_some(start_pts);
        for entry in &all[cursor.min(all.len())..] {
            self.enqueue((*entry).clone());
        }
        self.next_decode = target.end_decode_time.map(|time| (target.index + 1, time));

        let discard_before = match landing {
            Landing::Exact => at,
            Landing::Keyframe => landed,
        };
        Split::Done(PacketSeeked {
            requested: time,
            landed,
            discard_before,
            timescale: self.timescale,
            preroll,
        })
    }

    /// Queues `entry`, unless it is a leading sample of the sync sample a seek
    /// started at. The next sync sample ends them.
    fn enqueue(&mut self, entry: Entry) {
        if let Some(before) = self.leading_before {
            if entry.packet.keyframe && entry.packet.pts > before {
                self.leading_before = None;
            } else if entry.leading || entry.packet.pts < before {
                return;
            }
        }
        self.queue.push_back(entry);
    }

    /// The packet for `sample` of a run decoding from `start`, in `body`.
    fn entry(&self, start: u64, sample: &fragment::Sample, body: &Bytes) -> Entry {
        let decode = i128::from(start) + i128::from(sample.decode_offset);
        let dts = clamp(decode - i128::from(self.shift));
        let pts = clamp(decode + i128::from(sample.composition_offset) - i128::from(self.shift));
        let duration = u64::from(sample.duration);
        let (trim_start, trim_end) = self.trims(pts, duration);
        let data = body.slice(sample.range.clone());
        // The sample flags may say so, and an HEVC picture's NAL unit type does.
        let leading = sample.is_undecodable_leading()
            || self
                .hevc_length_size
                .is_some_and(|length_size| is_rasl(&data, length_size));
        Entry {
            packet: Packet {
                data,
                pts,
                dts,
                duration,
                timescale: self.timescale,
                keyframe: self.audio || sample.is_sync(),
                trim_start,
                trim_end,
            },
            leading,
        }
    }

    /// Audio ticks to drop from a packet at `pts`: before time zero, and past the
    /// edit list's end.
    fn trims(&self, pts: i64, duration: u64) -> (u64, u64) {
        if !self.audio {
            return (0, 0);
        }
        let trim_start = match pts {
            pts if pts < 0 => pts.unsigned_abs().min(duration),
            _ => 0,
        };
        let trim_end = match self.end {
            Some(end) => {
                let past = (pts + duration as i64 - end).max(0) as u64;
                past.min(duration - trim_start)
            }
            None => 0,
        };
        (trim_start, trim_end)
    }

    /// Checks that samples described by `index` are of the codec this demuxer reports.
    fn check_description(&self, index: u32, fragment: &Fragment) -> Result<(), Error> {
        if index == self.description_index {
            return Ok(());
        }
        let error = |error| media_error(fragment, error);
        let entry = description(&self.entries, index).map_err(error)?;
        let main = description(&self.entries, self.description_index).map_err(error)?;
        if entry.is_encrypted() {
            return Err(encrypted(fragment));
        }
        if entry.format(&self.init).map_err(error)? != main.format(&self.init).map_err(error)? {
            return Err(Error::UnsupportedMedia {
                fragment: fragment.to_string(),
                reason: "samples change codec between sample descriptions".to_owned(),
            });
        }
        Ok(())
    }

    /// Pre-roll in ticks for a seek landing in `target`: the caller's, else the
    /// fragment's or initialization's roll group, else the codec's default.
    fn preroll_ticks(&self, target: &Loaded) -> i64 {
        if let Some(preroll) = self.preroll {
            return duration_to_ticks(preroll, self.timescale);
        }
        let samples = target.roll.or(self.init_roll);
        let sample_duration = target
            .entries
            .first()
            .map_or(u64::from(self.sample_duration), |entry| {
                entry.packet.duration
            });
        match samples {
            Some(samples) if sample_duration > 0 => i64::from(samples) * sample_duration as i64,
            _ => duration_to_ticks(self.codec.default_preroll(), self.timescale),
        }
    }

    /// The timeline's start of media fragment `index`.
    fn fragment_start(&self, index: usize) -> Duration {
        self.timeline
            .as_ref()
            .and_then(|timeline| timeline.spans().get(index).map(|span| span.start))
            .unwrap_or_default()
    }

    /// The decode time the timeline puts media fragment `index` at, for a fragment
    /// without `tfdt`.
    fn timeline_decode_time(&self, index: usize) -> u64 {
        let start = duration_to_ticks(self.fragment_start(index), self.timescale);
        u64::try_from(start + self.shift).unwrap_or(0)
    }
}

/// The 1-based index of the sample description `track`'s fragments use unless they
/// say otherwise.
pub(crate) fn default_description_index(track: &init::Track) -> u32 {
    track.defaults.sample_description_index.max(1)
}

/// The 1-based sample description `index` of `entries`.
pub(crate) fn description(entries: &[SampleEntry], index: u32) -> Result<SampleEntry, ParseError> {
    index
        .checked_sub(1)
        .and_then(|index| entries.get(index as usize))
        .copied()
        .ok_or_else(|| ParseError::Malformed(format!("no sample description {index}")))
}

/// Pre-roll in samples that `groups` signals: a negative roll distance.
fn roll_samples(groups: &RollGroups) -> Option<u32> {
    let distance = groups
        .default_index
        .and_then(|index| groups.distances.get(index.checked_sub(1)? as usize))
        .or_else(|| groups.distances.first())?;
    (*distance < 0).then(|| u32::from(distance.unsigned_abs()))
}

/// `value` ticks of a `from` clock in ticks of a `to` clock, rounded down.
fn rescale(value: u64, from: u32, to: u32) -> i64 {
    match from {
        0 => 0,
        from => clamp(i128::from(value) * i128::from(to) / i128::from(from)),
    }
}

fn clamp(value: i128) -> i64 {
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// The NAL unit length size an `hvcC` payload gives: 1, 2 or 4 bytes, or `None` for a
/// truncated or invalid one.
fn hevc_length_size(hvcc: &[u8]) -> Option<usize> {
    match hvcc.get(21)? & 0x03 {
        2 => None,
        size => Some(usize::from(size) + 1),
    }
}

/// Whether `sample`, HEVC NAL units each after a `length_size`-byte length, is a RASL
/// picture: a leading picture that references pictures before the CRA it follows. Its
/// first slice's NAL unit type says, 8 or 9. A malformed sample is not one.
fn is_rasl(sample: &[u8], length_size: usize) -> bool {
    let mut pos = 0;
    while let Some(prefix) = sample.get(pos..pos + length_size) {
        let len = prefix
            .iter()
            .fold(0usize, |len, &byte| len << 8 | usize::from(byte));
        let Some(&header) = sample.get(pos + length_size) else {
            return false;
        };
        // Types up to 31 are slices; parameter sets and SEI before them are skipped.
        let nal_type = header >> 1 & 0x3f;
        if nal_type <= 31 {
            return matches!(nal_type, 8 | 9);
        }
        pos += length_size + len;
    }
    false
}

/// `error`, from parsing the body of `fragment`, as a public error.
pub(crate) fn media_error(fragment: &Fragment, error: ParseError) -> Error {
    let fragment = fragment.to_string();
    match error {
        ParseError::Malformed(reason) => Error::MalformedMedia { fragment, reason },
        ParseError::Unsupported(reason) => Error::UnsupportedMedia { fragment, reason },
    }
}

fn encrypted(fragment: &Fragment) -> Error {
    Error::UnsupportedMedia {
        fragment: fragment.to_string(),
        reason: "samples are encrypted; read them through a FragmentCache with content keys"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::fragment::tests::{RunSample, segment_from};
    use crate::mp4::init::tests::{SOUND, TrackSpec, audio_entry, bx, full, init, video_entry};

    const SYNC: u32 = 0x0200_0000;
    const NON_SYNC: u32 = 0x0101_0000;
    /// A non-sync sample that cannot be decoded from the sync sample before it.
    const RASL: u32 = 0x0401_0000;

    /// An edit list starting at `media_time`.
    fn edts(media_time: u32) -> Vec<u8> {
        let entry = [
            0u32.to_be_bytes(),
            media_time.to_be_bytes(),
            0x0001_0000u32.to_be_bytes(),
        ]
        .concat();
        bx(
            b"edts",
            &[&full(b"elst", 0, 0, &[&1u32.to_be_bytes(), &entry])],
        )
    }

    /// A one-track init at 1000 ticks per second, with ten-tick samples by default.
    fn one_track(handler: &[u8; 4], edit: Option<u32>, stbl_extra: &[Vec<u8>]) -> Bytes {
        let entries = match handler {
            b"soun" => [audio_entry(b"abcd", 1, 1000, &[])],
            _ => [video_entry(b"abcd", 16, 16, &[])],
        };
        let trak_extra: Vec<Vec<u8>> = edit.into_iter().map(edts).collect();
        Bytes::from(init(&[TrackSpec {
            handler,
            timescale: 1000,
            entries: &entries,
            trak_extra: &trak_extra,
            stbl_extra,
            trex: TrackDefaults {
                sample_description_index: 1,
                sample_duration: 10,
                sample_size: 1,
                sample_flags: SYNC,
            },
            ..SOUND
        }]))
    }

    fn timeline(durations: &[u64]) -> MediaTimeline {
        MediaTimeline {
            timescale: 1000,
            origin: 0,
            media_durations: durations.to_vec(),
        }
    }

    fn demuxer(init: Bytes, durations: &[u64]) -> Demuxer {
        Demuxer::new(
            init,
            &Fragment::new("init"),
            Some(timeline(durations)),
            None,
        )
        .unwrap()
    }

    /// `count` one-byte audio samples.
    fn audio_samples(count: usize) -> Vec<RunSample> {
        vec![RunSample::default(); count]
    }

    /// Video samples as `(flags, composition offset)`.
    fn video_samples(samples: &[(u32, i32)]) -> Vec<RunSample> {
        samples
            .iter()
            .map(|&(flags, offset)| RunSample {
                flags: Some(flags),
                composition_offset: Some(offset),
                ..RunSample::default()
            })
            .collect()
    }

    /// Media fragment `index` holding `samples` from `decode_time`, its bytes counting
    /// up from `first_byte`.
    fn load(
        demuxer: &mut Demuxer,
        index: usize,
        decode_time: Option<u64>,
        samples: &[RunSample],
        first_byte: u8,
    ) -> Loaded {
        let data: Vec<u8> = (0..samples.len() as u8).map(|n| first_byte + n).collect();
        let body = Bytes::from(segment_from(1, decode_time, samples, &[], &data));
        demuxer
            .load(index, &body, &Fragment::new(format!("media{index}")))
            .unwrap()
    }

    fn pts(packets: impl IntoIterator<Item = Packet>) -> Vec<i64> {
        packets.into_iter().map(|packet| packet.pts).collect()
    }

    fn drain(demuxer: &mut Demuxer) -> Vec<Packet> {
        std::iter::from_fn(|| demuxer.pop()).collect()
    }

    fn done(split: Split) -> PacketSeeked {
        match split {
            Split::Done(seeked) => seeked,
            Split::Earlier => panic!("seek wanted an earlier fragment"),
            Split::Later => panic!("seek wanted a later fragment"),
        }
    }

    #[test]
    fn edit_list_priming_becomes_negative_pts_and_trims() {
        // A ten-tick priming packet: the first fragment presents 30 ticks.
        let mut demuxer = demuxer(one_track(b"soun", Some(10), &[]), &[30, 40]);
        let first = load(&mut demuxer, 0, Some(0), &audio_samples(4), 0);
        demuxer.push(first);
        let second = load(&mut demuxer, 1, Some(40), &audio_samples(4), 4);
        demuxer.push(second);

        let packets = drain(&mut demuxer);
        assert_eq!(pts(packets.clone()), [-10, 0, 10, 20, 30, 40, 50, 60]);
        let first = &packets[0];
        assert_eq!(
            (first.dts, first.duration, first.timescale),
            (-10, 10, 1000)
        );
        assert_eq!((first.trim_start, first.trim_end), (10, 0));
        assert_eq!(first.presentation_time(), None);
        assert!(packets.iter().all(|packet| packet.keyframe));
        assert!(packets[1..].iter().all(|packet| packet.trim_start == 0));
        let data: Vec<u8> = packets.iter().map(|packet| packet.data[0]).collect();
        assert_eq!(data, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn origin_moves_time_zero_to_the_first_fragment() {
        let mut demuxer = Demuxer::new(
            one_track(b"soun", None, &[]),
            &Fragment::new("init"),
            Some(MediaTimeline {
                timescale: 500,
                origin: 450,
                media_durations: vec![20],
            }),
            None,
        )
        .unwrap();
        // Origin 450 at 500 ticks per second is 900 media ticks.
        let loaded = load(&mut demuxer, 0, Some(900), &audio_samples(2), 0);
        demuxer.push(loaded);
        assert_eq!(pts(drain(&mut demuxer)), [0, 10]);
    }

    #[test]
    fn fragments_without_tfdt_continue_or_follow_the_timeline() {
        let mut demuxer = demuxer(one_track(b"soun", None, &[]), &[20, 20, 20]);
        let loaded = load(&mut demuxer, 0, Some(0), &audio_samples(2), 0);
        demuxer.push(loaded);
        let loaded = load(&mut demuxer, 1, None, &audio_samples(2), 2);
        demuxer.push(loaded);
        assert_eq!(pts(drain(&mut demuxer)), [0, 10, 20, 30]);

        // Out of order, the timeline places it.
        let loaded = load(&mut demuxer, 2, None, &audio_samples(1), 4);
        let loaded_again = load(&mut demuxer, 0, None, &audio_samples(1), 5);
        demuxer.push(loaded);
        demuxer.push(loaded_again);
        assert_eq!(pts(drain(&mut demuxer)), [40, 0]);
    }

    #[test]
    fn packet_seeks_land_on_the_packet_containing_the_time() {
        let mut demuxer = demuxer(one_track(b"soun", None, &[]), &[40, 40]);
        demuxer.set_preroll(Duration::from_millis(10));
        let time = Duration::from_millis(45);
        assert_eq!(demuxer.seek_range(time).unwrap(), 0..=1);
        let loaded = [
            load(&mut demuxer, 0, Some(0), &audio_samples(4), 0),
            load(&mut demuxer, 1, Some(40), &audio_samples(4), 4),
        ];
        let seeked = done(demuxer.seek(time, Landing::Exact, &loaded, false, false));
        assert_eq!(seeked.requested, time);
        assert_eq!((seeked.landed, seeked.discard_before), (40, 45));
        assert_eq!(pts(seeked.preroll), [30]);
        assert_eq!(pts(drain(&mut demuxer)), [40, 50, 60, 70]);

        // The next fragment carries on from there.
        let next = load(&mut demuxer, 2, None, &audio_samples(1), 8);
        demuxer.push(next);
        assert_eq!(pts(drain(&mut demuxer)), [80]);
    }

    #[test]
    fn keyframe_seeks_land_past_the_keyframes_preroll_before_the_time() {
        let mut demuxer = demuxer(one_track(b"soun", None, &[]), &[40, 40]);
        demuxer.set_preroll(Duration::from_millis(20));
        let time = Duration::from_millis(65);
        assert_eq!(demuxer.seek_range(time).unwrap(), 1..=1);
        let loaded = [load(&mut demuxer, 1, Some(40), &audio_samples(4), 4)];
        let seeked = done(demuxer.seek(time, Landing::Keyframe, &loaded, false, false));
        assert_eq!(seeked.requested, time);
        assert_eq!((seeked.landed, seeked.discard_before), (60, 60));
        assert_eq!(pts(seeked.preroll), [40, 50]);
        assert_eq!(pts(drain(&mut demuxer)), [60, 70]);
    }

    #[test]
    fn keyframe_seeks_within_the_first_preroll_land_on_the_packet_at_the_time() {
        let mut demuxer = demuxer(one_track(b"soun", None, &[]), &[40, 40]);
        demuxer.set_preroll(Duration::from_millis(20));
        let loaded = [load(&mut demuxer, 0, Some(0), &audio_samples(4), 0)];
        let time = Duration::from_millis(15);
        let seeked = done(demuxer.seek(time, Landing::Keyframe, &loaded, true, false));
        assert_eq!((seeked.landed, seeked.discard_before), (10, 10));
        assert_eq!(pts(seeked.preroll), [0]);
        assert_eq!(pts(drain(&mut demuxer)), [10, 20, 30]);
    }

    /// A GOP of I, P, B, B, P, B, B decoded from `decode_time`, presenting 70 ticks
    /// from `decode_time` after a 20-tick composition delay.
    fn gop() -> Vec<RunSample> {
        video_samples(&[
            (SYNC, 20),
            (NON_SYNC, 40),
            (NON_SYNC, 10),
            (NON_SYNC, 10),
            (NON_SYNC, 40),
            (NON_SYNC, 10),
            (NON_SYNC, 10),
        ])
    }

    #[test]
    fn video_seeks_start_at_a_keyframe_and_keep_b_frames_after_the_cursor() {
        let mut demuxer = demuxer(one_track(b"vide", Some(20), &[]), &[70, 70]);
        assert_eq!(demuxer.preroll(), Duration::ZERO);
        let first = load(&mut demuxer, 0, Some(0), &gop(), 0);
        let packets = &first.entries;
        assert_eq!(
            packets
                .iter()
                .map(|entry| entry.packet.pts)
                .collect::<Vec<_>>(),
            [0, 30, 10, 20, 60, 40, 50]
        );
        assert_eq!(
            packets
                .iter()
                .map(|entry| entry.packet.dts)
                .collect::<Vec<_>>(),
            [-20, -10, 0, 10, 20, 30, 40]
        );
        assert!(packets.iter().all(|entry| entry.packet.trim_start == 0));

        // 25 is shown by the B-frame at 20, which decodes after the P-frame at 30.
        let time = Duration::from_millis(25);
        let seeked = done(demuxer.seek(time, Landing::Exact, &[first], false, false));
        assert_eq!((seeked.landed, seeked.discard_before), (30, 25));
        assert_eq!(pts(seeked.preroll), [0]);
        assert_eq!(pts(drain(&mut demuxer)), [30, 10, 20, 60, 40, 50]);

        // A keyframe seek there lands on the keyframe itself, with nothing to pre-roll.
        let first = load(&mut demuxer, 0, Some(0), &gop(), 0);
        let seeked = done(demuxer.seek(time, Landing::Keyframe, &[first], false, false));
        assert_eq!((seeked.landed, seeked.discard_before), (0, 0));
        assert!(seeked.preroll.is_empty());
        assert_eq!(pts(drain(&mut demuxer)), [0, 30, 10, 20, 60, 40, 50]);
    }

    #[test]
    fn video_seeks_step_back_to_an_earlier_keyframe() {
        let mut demuxer = demuxer(one_track(b"vide", Some(20), &[]), &[70, 70]);
        // The second fragment has no sync sample.
        let mut open = gop();
        open[0].flags = Some(NON_SYNC);
        let second = load(&mut demuxer, 1, Some(70), &open, 7);
        let time = Duration::from_millis(75);
        assert!(matches!(
            demuxer.seek(
                time,
                Landing::Exact,
                std::slice::from_ref(&second),
                false,
                false
            ),
            Split::Earlier
        ));

        let first = load(&mut demuxer, 0, Some(0), &gop(), 0);
        let loaded = [first, second];
        let seeked = done(demuxer.seek(time, Landing::Exact, &loaded, false, false));
        assert_eq!(seeked.landed, 70);
        assert_eq!(pts(seeked.preroll), [0, 30, 10, 20, 60, 40, 50]);

        // A keyframe seek there lands on that earlier keyframe.
        let first = load(&mut demuxer, 0, Some(0), &gop(), 0);
        let second = load(&mut demuxer, 1, Some(70), &open, 7);
        let loaded = [first, second];
        let seeked = done(demuxer.seek(time, Landing::Keyframe, &loaded, false, false));
        assert_eq!((seeked.requested, seeked.landed), (time, 0));
        assert!(seeked.preroll.is_empty());
    }

    #[test]
    fn seeks_past_the_named_fragments_end_move_on_to_the_next() {
        // The timeline says 70 ticks a fragment, but the first holds only 60.
        let mut demuxer = demuxer(one_track(b"vide", Some(20), &[]), &[70, 70]);
        let short = video_samples(&[(SYNC, 20), (NON_SYNC, 20), (NON_SYNC, 20)]);
        let first = load(&mut demuxer, 0, Some(0), &short, 0);
        assert_eq!(
            first
                .entries
                .iter()
                .map(|entry| entry.packet.pts)
                .collect::<Vec<_>>(),
            [0, 10, 20]
        );
        // 30 is in the first fragment by the timeline, but after its last packet.
        let time = Duration::from_millis(30);
        assert!(matches!(
            demuxer.seek(
                time,
                Landing::Exact,
                std::slice::from_ref(&first),
                true,
                false
            ),
            Split::Later
        ));

        // With the next fragment, decoding starts at its keyframe, where the stream lands.
        let second = load(&mut demuxer, 1, Some(30), &gop(), 3);
        let loaded = [first, second];
        let seeked = done(demuxer.seek(time, Landing::Exact, &loaded, true, false));
        assert_eq!((seeked.landed, seeked.discard_before), (30, 30));
        assert!(seeked.preroll.is_empty());
        assert_eq!(drain(&mut demuxer)[0].pts, 30);

        // At the stream's end, it lands there.
        let last = load(&mut demuxer, 0, Some(0), &short, 0);
        let seeked = done(demuxer.seek(time, Landing::Exact, &[last], true, true));
        assert_eq!(seeked.landed, 30);
    }

    #[test]
    fn leading_samples_of_the_start_are_dropped() {
        let mut demuxer = demuxer(one_track(b"vide", Some(20), &[]), &[70, 70]);
        // An open GOP: two RASL pictures presenting before the CRA at 70.
        let samples = video_samples(&[(SYNC, 40), (RASL, 10), (RASL, 10), (NON_SYNC, 40)]);
        let open = load(&mut demuxer, 1, Some(50), &samples, 0);
        assert_eq!(
            open.entries
                .iter()
                .map(|entry| entry.packet.pts)
                .collect::<Vec<_>>(),
            [70, 50, 60, 100]
        );
        let seeked = done(demuxer.seek(
            Duration::from_millis(72),
            Landing::Exact,
            &[open],
            true,
            false,
        ));
        assert_eq!(seeked.landed, 70);
        assert!(seeked.preroll.is_empty());
        assert_eq!(pts(drain(&mut demuxer)), [70, 100]);
    }

    #[test]
    fn seeks_start_earlier_when_a_leading_sample_would_be_shown() {
        let mut demuxer = demuxer(one_track(b"vide", Some(20), &[]), &[70, 70]);
        let first = load(&mut demuxer, 0, Some(0), &gop(), 0);
        // A CRA at 70 whose RASL picture is timed after it, as some muxers of open GOPs
        // write it.
        let samples = video_samples(&[(SYNC, 20), (RASL, 30), (NON_SYNC, 30)]);
        let second = load(&mut demuxer, 1, Some(70), &samples, 7);
        assert_eq!(
            second
                .entries
                .iter()
                .map(|entry| entry.packet.pts)
                .collect::<Vec<_>>(),
            [70, 90, 100]
        );

        // Decoding from the CRA would lose the RASL picture at 90: start a GOP earlier.
        let time = Duration::from_millis(75);
        let only_second = std::slice::from_ref(&second);
        assert!(matches!(
            demuxer.seek(time, Landing::Exact, only_second, false, false),
            Split::Earlier
        ));
        let loaded = [first, second];
        let seeked = done(demuxer.seek(time, Landing::Exact, &loaded, false, false));
        assert_eq!(seeked.landed, 70);
        assert_eq!(pts(seeked.preroll), [0, 30, 10, 20, 60, 40, 50]);
        assert_eq!(pts(drain(&mut demuxer)), [70, 90, 100]);
    }

    #[test]
    fn rasl_pictures_are_found_by_their_nal_unit_type() {
        // A VPS (type 32), then a RASL_N slice (type 8).
        let rasl = [0, 0, 0, 3, 0x40, 0x01, 0x0c, 0, 0, 0, 2, 0x10, 0x01];
        assert!(is_rasl(&rasl, 4));
        // A trailing picture's slice (type 1), with two-byte lengths.
        assert!(!is_rasl(&[0, 2, 0x02, 0x01], 2));
        // A RASL_R slice (type 9) with one-byte lengths, and truncated samples.
        assert!(is_rasl(&[2, 0x12, 0x01], 1));
        assert!(!is_rasl(&[0, 0, 0, 3, 0x40, 0x01, 0x0c, 0, 0], 4));
        assert!(!is_rasl(&[], 4));

        assert_eq!(hevc_length_size(&[0; 22]), Some(1));
        let mut hvcc = [0; 23];
        hvcc[21] = 0xff;
        assert_eq!(hevc_length_size(&hvcc), Some(4));
        hvcc[21] = 0xfe;
        assert_eq!(hevc_length_size(&hvcc), None);
        assert_eq!(hevc_length_size(&[0; 21]), None);
    }

    #[test]
    fn preroll_comes_from_the_caller_then_roll_groups_then_the_codec() {
        let roll = full(
            b"sgpd",
            1,
            0,
            &[
                b"roll",
                &2u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &(-2i16).to_be_bytes(),
            ],
        );
        let mut demuxer = demuxer(one_track(b"soun", None, &[roll]), &[40]);
        assert_eq!(demuxer.preroll(), Duration::from_millis(20));
        demuxer.set_preroll(Duration::from_millis(5));
        assert_eq!(demuxer.preroll(), Duration::from_millis(5));

        // An AAC-LC track without roll groups uses its frame.
        let esds = full(
            b"esds",
            0,
            0,
            &[&[
                0x03, 0x14, 0, 1, 0, 0x04, 0x0f, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05,
                0x02, 0x12, 0x10,
            ]],
        );
        let entries = [audio_entry(b"mp4a", 2, 44_100, &[&esds])];
        let aac = Bytes::from(init(&[TrackSpec {
            entries: &entries,
            ..SOUND
        }]));
        let demuxer = Demuxer::new(aac, &Fragment::new("init"), None, None).unwrap();
        assert!(matches!(demuxer.codec(), Codec::Aac(_)));
        assert_eq!(demuxer.preroll(), Duration::from_secs(1024) / 44_100);
    }

    #[test]
    fn several_tracks_need_one_chosen() {
        let audio = [audio_entry(b"abcd", 1, 1000, &[])];
        let video = [video_entry(b"abcd", 16, 16, &[])];
        let two = Bytes::from(init(&[
            TrackSpec {
                entries: &audio,
                trex: TrackDefaults {
                    sample_description_index: 1,
                    sample_duration: 10,
                    sample_size: 1,
                    sample_flags: SYNC,
                },
                ..SOUND
            },
            TrackSpec {
                id: 2,
                handler: b"vide",
                timescale: 1000,
                entries: &video,
                trex: TrackDefaults {
                    sample_description_index: 1,
                    sample_duration: 40,
                    sample_size: 1,
                    sample_flags: SYNC,
                },
                ..SOUND
            },
        ]));
        let init_fragment = Fragment::new("init");
        assert!(matches!(
            Demuxer::new(two.clone(), &init_fragment, None, None),
            Err(Error::UnsupportedMedia { reason, .. }) if reason.contains("2 tracks")
        ));
        assert!(matches!(
            Demuxer::new(two.clone(), &init_fragment, None, Some(3)),
            Err(Error::Initialization(reason)) if reason.contains("[1, 2]")
        ));

        // A fragment holding both tracks: each demuxer takes only its own.
        let both = [
            segment_from(1, Some(0), &audio_samples(2), &[], &[1, 2]),
            segment_from(2, Some(0), &audio_samples(1), &[], &[3]),
        ]
        .concat();
        let body = Bytes::from(both);
        for (track_id, expected) in [(1, vec![(0, 1), (10, 2)]), (2, vec![(0, 3)])] {
            let mut demuxer =
                Demuxer::new(two.clone(), &init_fragment, None, Some(track_id)).unwrap();
            assert_eq!(demuxer.track_id(), track_id);
            let loaded = demuxer.load(0, &body, &Fragment::new("media0")).unwrap();
            demuxer.push(loaded);
            let packets: Vec<(i64, u8)> = drain(&mut demuxer)
                .iter()
                .map(|packet| (packet.pts, packet.data[0]))
                .collect();
            assert_eq!(packets, expected, "track {track_id}");
        }
    }

    #[test]
    fn encrypted_samples_are_unsupported() {
        let sinf = bx(b"sinf", &[&bx(b"frma", &[b"abcd"])]);
        let entries = [audio_entry(b"enca", 1, 1000, &[&sinf])];
        let protected = Bytes::from(init(&[TrackSpec {
            entries: &entries,
            ..SOUND
        }]));
        assert!(matches!(
            Demuxer::new(protected, &Fragment::new("init"), None, None),
            Err(Error::UnsupportedMedia { reason, .. }) if reason.contains("encrypted")
        ));
    }

    #[test]
    fn seeking_needs_a_timeline() {
        let demuxer = Demuxer::new(
            one_track(b"soun", None, &[]),
            &Fragment::new("init"),
            None,
            None,
        )
        .unwrap();
        let error = demuxer.seek_range(Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
