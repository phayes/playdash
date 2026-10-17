//! Media segments: each `moof` + `mdat` pair's samples, with their bytes, timing and flags.

use super::init::{RollGroups, TrackDefaults, roll_groups};
use super::reader::{Atom, ParseError, atoms, find, malformed, require};
use std::ops::Range;

/// tfhd flags (ISO/IEC 14496-12 §8.8.7.1).
const BASE_DATA_OFFSET: u32 = 0x01;
const SAMPLE_DESCRIPTION_INDEX: u32 = 0x02;
const DEFAULT_DURATION: u32 = 0x08;
const DEFAULT_SIZE: u32 = 0x10;
const DEFAULT_FLAGS: u32 = 0x20;
const DURATION_IS_EMPTY: u32 = 0x01_0000;
const DEFAULT_BASE_IS_MOOF: u32 = 0x02_0000;

/// trun flags (ISO/IEC 14496-12 §8.8.8.1).
const DATA_OFFSET: u32 = 0x01;
const FIRST_SAMPLE_FLAGS: u32 = 0x04;
const SAMPLE_DURATION: u32 = 0x100;
const SAMPLE_SIZE: u32 = 0x200;
const SAMPLE_FLAGS: u32 = 0x400;
const SAMPLE_COMPOSITION_OFFSET: u32 = 0x800;

/// One track's run of samples in one `moof`.
#[derive(Debug)]
pub(crate) struct Traf {
    pub(crate) track_id: u32,
    /// The `traf` box, for boxes this module does not interpret, such as `senc`.
    #[cfg_attr(not(feature = "encryption"), allow(dead_code))]
    pub(crate) atom: Atom,
    /// `tfdt` base media decode time of the first sample, in track timescale ticks.
    pub(crate) decode_time: Option<u64>,
    pub(crate) samples: Vec<Sample>,
    /// `roll` or `prol` descriptions local to this fragment.
    pub(crate) roll: Option<RollGroups>,
}

/// One sample of a [`Traf`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    /// The sample's bytes in the body parsed.
    pub(crate) range: Range<usize>,
    /// Decode time relative to the traf's first sample, in track timescale ticks.
    pub(crate) decode_offset: u64,
    pub(crate) duration: u32,
    /// Presentation time minus decode time.
    pub(crate) composition_offset: i64,
    /// `sample_flags` (ISO/IEC 14496-12 §8.8.3.1).
    pub(crate) flags: u32,
    /// 1-based `stsd` entry describing the sample.
    pub(crate) description_index: u32,
}

impl Sample {
    /// Whether the sample is a sync sample (a random access point).
    pub(crate) fn is_sync(&self) -> bool {
        self.flags & 0x0001_0000 == 0
    }

    /// Whether the sample is a leading sample that cannot be decoded when decoding
    /// starts at the sync sample before it (`is_leading` 1, such as an HEVC RASL picture).
    pub(crate) fn is_undecodable_leading(&self) -> bool {
        (self.flags >> 26) & 0x3 == 1
    }
}

/// Parses every `moof` in `body` and the samples it describes. `body_start` is the
/// offset of `body` in its resource, against which an explicit `tfhd` base data offset
/// counts. `defaults` gives each track's `trex` defaults by track ID.
pub(crate) fn parse(
    body: &[u8],
    body_start: u64,
    defaults: &[(u32, TrackDefaults)],
) -> Result<Vec<Traf>, ParseError> {
    let mut trafs = Vec::new();
    for moof in atoms(body, 0, body.len()) {
        let moof = moof?;
        if &moof.kind != b"moof" {
            continue;
        }
        // A traf without an explicit base starts at the moof if first, else where the
        // previous traf's data ended.
        let mut implicit_base = moof.start;
        for traf in moof.children(body) {
            let traf = traf?;
            if &traf.kind == b"traf" {
                let parsed = track_fragment(body, body_start, moof, traf, implicit_base, defaults)?;
                implicit_base = parsed.1;
                trafs.push(parsed.0);
            }
        }
    }
    Ok(trafs)
}

/// One `traf`, and where its sample data ends.
fn track_fragment(
    body: &[u8],
    body_start: u64,
    moof: Atom,
    traf: Atom,
    implicit_base: usize,
    defaults: &[(u32, TrackDefaults)],
) -> Result<(Traf, usize), ParseError> {
    // tfhd: the track and its defaults for this fragment.
    let mut tfhd = require(traf.children(body), b"tfhd")?.fields(body);
    let (_, flags) = tfhd.version_flags()?;
    let track_id = tfhd.u32()?;
    let trex = defaults
        .iter()
        .find(|(id, _)| *id == track_id)
        .map(|(_, defaults)| *defaults)
        .unwrap_or_default();
    let base = match flags & BASE_DATA_OFFSET {
        0 if flags & DEFAULT_BASE_IS_MOOF != 0 => moof.start,
        0 => implicit_base,
        _ => {
            // Absolute in the resource, which `body` is a range of.
            let offset = tfhd.u64()?;
            offset
                .checked_sub(body_start)
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or_else(|| malformed("base data offset precedes the fragment"))?
        }
    };
    let mut fragment_defaults = trex;
    if flags & SAMPLE_DESCRIPTION_INDEX != 0 {
        fragment_defaults.sample_description_index = tfhd.u32()?;
    }
    if flags & DEFAULT_DURATION != 0 {
        fragment_defaults.sample_duration = tfhd.u32()?;
    }
    if flags & DEFAULT_SIZE != 0 {
        fragment_defaults.sample_size = tfhd.u32()?;
    }
    if flags & DEFAULT_FLAGS != 0 {
        fragment_defaults.sample_flags = tfhd.u32()?;
    }

    let decode_time = match find(traf.children(body), b"tfdt")? {
        Some(tfdt) => {
            let mut fields = tfdt.fields(body);
            let (version, _) = fields.version_flags()?;
            Some(match version {
                1 => fields.u64()?,
                _ => u64::from(fields.u32()?),
            })
        }
        None => None,
    };

    // Samples from each trun, and roll groups from sgpd.
    let mut samples = Vec::new();
    let mut data_end = base;
    let mut roll = None;
    if flags & DURATION_IS_EMPTY == 0 {
        for child in traf.children(body) {
            let child = child?;
            match &child.kind {
                b"trun" => {
                    data_end = track_run(
                        body,
                        child,
                        base,
                        data_end,
                        &fragment_defaults,
                        &mut samples,
                    )?;
                }
                b"sgpd" => {
                    if let Some(groups) = roll_groups(body, child)? {
                        roll = Some(groups);
                    }
                }
                _ => {}
            }
        }
    }

    Ok((
        Traf {
            track_id,
            atom: traf,
            decode_time,
            samples,
            roll,
        },
        data_end,
    ))
}

/// Appends the samples of one `trun`. Returns where their data ends.
fn track_run(
    body: &[u8],
    trun: Atom,
    base: usize,
    previous_end: usize,
    defaults: &TrackDefaults,
    samples: &mut Vec<Sample>,
) -> Result<usize, ParseError> {
    let mut fields = trun.fields(body);
    let (version, flags) = fields.version_flags()?;
    let count = fields.u32()? as usize;
    // Samples may be empty, but not more numerous than the body's bytes.
    if count > body.len() {
        return Err(malformed(format!("trun claims {count} samples")));
    }
    let mut pos = if flags & DATA_OFFSET != 0 {
        let offset = isize::try_from(fields.i32()?).expect("i32 fits isize");
        base.checked_add_signed(offset)
            .ok_or_else(|| malformed("trun data offset precedes the body"))?
    } else {
        previous_end
    };
    let first_sample_flags = match flags & FIRST_SAMPLE_FLAGS {
        0 => None,
        _ => Some(fields.u32()?),
    };

    let mut decode_offset = samples.last().map_or(0, |sample: &Sample| {
        sample.decode_offset + u64::from(sample.duration)
    });
    for index in 0..count {
        let duration = match flags & SAMPLE_DURATION {
            0 => defaults.sample_duration,
            _ => fields.u32()?,
        };
        let size = match flags & SAMPLE_SIZE {
            0 => defaults.sample_size,
            _ => fields.u32()?,
        };
        let sample_flags = match (flags & SAMPLE_FLAGS, first_sample_flags) {
            (0, Some(first)) if index == 0 => first,
            (0, _) => defaults.sample_flags,
            _ => fields.u32()?,
        };
        let composition_offset = match (flags & SAMPLE_COMPOSITION_OFFSET, version) {
            (0, _) => 0,
            (_, 0) => i64::from(fields.u32()?),
            _ => i64::from(fields.i32()?),
        };
        let end = pos
            .checked_add(size as usize)
            .filter(|&end| end <= body.len())
            .ok_or_else(|| malformed("sample data extends past the fragment"))?;
        samples.push(Sample {
            range: pos..end,
            decode_offset,
            duration,
            composition_offset,
            flags: sample_flags,
            description_index: defaults.sample_description_index,
        });
        pos = end;
        decode_offset += u64::from(duration);
    }
    Ok(pos)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mp4::init::tests::{bx, full};

    /// A trun sample: duration, size, flags and composition offset, each optional.
    #[derive(Clone, Copy, Default)]
    pub(crate) struct RunSample {
        pub(crate) duration: Option<u32>,
        pub(crate) size: Option<u32>,
        pub(crate) flags: Option<u32>,
        pub(crate) composition_offset: Option<i32>,
    }

    /// A `trun` (version 1) of `samples`, with a data offset if given. Every sample must
    /// set the same fields.
    pub(crate) fn trun(
        samples: &[RunSample],
        data_offset: Option<i32>,
        first_flags: Option<u32>,
    ) -> Vec<u8> {
        let first = samples.first().copied().unwrap_or_default();
        let mut flags = 0;
        let mut body = (samples.len() as u32).to_be_bytes().to_vec();
        if let Some(offset) = data_offset {
            flags |= DATA_OFFSET;
            body.extend(offset.to_be_bytes());
        }
        if let Some(first) = first_flags {
            flags |= FIRST_SAMPLE_FLAGS;
            body.extend(first.to_be_bytes());
        }
        flags |= first.duration.map_or(0, |_| SAMPLE_DURATION)
            | first.size.map_or(0, |_| SAMPLE_SIZE)
            | first.flags.map_or(0, |_| SAMPLE_FLAGS)
            | first
                .composition_offset
                .map_or(0, |_| SAMPLE_COMPOSITION_OFFSET);
        for sample in samples {
            for value in [sample.duration, sample.size, sample.flags]
                .into_iter()
                .flatten()
            {
                body.extend(value.to_be_bytes());
            }
            if let Some(offset) = sample.composition_offset {
                body.extend(offset.to_be_bytes());
            }
        }
        full(b"trun", 1, flags, &[&body])
    }

    /// A `moof` with one `traf` for `track_id`, then an `mdat` of `data`. The trun's data
    /// offset points at the mdat payload, relative to the moof (default-base-is-moof).
    pub(crate) fn segment(
        track_id: u32,
        decode_time: u64,
        samples: &[RunSample],
        extra: &[Vec<u8>],
        data: &[u8],
    ) -> Vec<u8> {
        segment_from(track_id, Some(decode_time), samples, extra, data)
    }

    /// [`segment`], with a `tfdt` only when `decode_time` is given.
    pub(crate) fn segment_from(
        track_id: u32,
        decode_time: Option<u64>,
        samples: &[RunSample],
        extra: &[Vec<u8>],
        data: &[u8],
    ) -> Vec<u8> {
        let build = |offset: i32| {
            let tfhd = full(b"tfhd", 0, DEFAULT_BASE_IS_MOOF, &[&track_id.to_be_bytes()]);
            let tfdt = decode_time.map(|time| full(b"tfdt", 1, 0, &[&time.to_be_bytes()]));
            let trun = trun(samples, Some(offset), None);
            let mut traf: Vec<&[u8]> = vec![&tfhd];
            traf.extend(tfdt.as_deref());
            traf.push(&trun);
            for extra in extra {
                traf.push(extra);
            }
            bx(
                b"moof",
                &[
                    &full(b"mfhd", 0, 0, &[&1u32.to_be_bytes()]),
                    &bx(b"traf", &traf),
                ],
            )
        };
        let moof_len = build(0).len();
        [build(moof_len as i32 + 8), bx(b"mdat", &[data])].concat()
    }

    const TREX: TrackDefaults = TrackDefaults {
        sample_description_index: 1,
        sample_duration: 1024,
        sample_size: 4,
        sample_flags: 0x0101_0000,
    };

    fn sized(size: u32) -> RunSample {
        RunSample {
            size: Some(size),
            ..RunSample::default()
        }
    }

    #[test]
    fn samples_take_trun_then_tfhd_then_trex_values() {
        let samples = [
            RunSample {
                duration: Some(10),
                size: Some(3),
                flags: Some(0x0200_0000),
                composition_offset: Some(-5),
            },
            RunSample {
                duration: Some(20),
                size: Some(2),
                flags: Some(0x0101_0000),
                composition_offset: Some(7),
            },
        ];
        let body = segment(1, 9000, &samples, &[], b"abcde");
        let trafs = parse(&body, 0, &[(1, TREX)]).unwrap();
        let [traf] = &trafs[..] else {
            panic!("one traf")
        };
        assert_eq!((traf.track_id, traf.decode_time), (1, Some(9000)));
        let data: Vec<&[u8]> = traf
            .samples
            .iter()
            .map(|sample| &body[sample.range.clone()])
            .collect();
        assert_eq!(data, [&b"abc"[..], b"de"]);
        assert_eq!(traf.samples[0].decode_offset, 0);
        assert_eq!(traf.samples[1].decode_offset, 10);
        assert_eq!(traf.samples[0].composition_offset, -5);
        assert!(traf.samples[0].is_sync());
        assert!(!traf.samples[1].is_sync());

        // Without per-sample values: trex defaults, and first_sample_flags for the first.
        let tfhd = full(b"tfhd", 0, DEFAULT_BASE_IS_MOOF, &[&1u32.to_be_bytes()]);
        let build = |offset: i32| {
            let trun = trun(&[RunSample::default(); 3], Some(offset), Some(0x0200_0000));
            bx(b"moof", &[&bx(b"traf", &[&tfhd, &trun])])
        };
        let moof_len = build(0).len();
        let body = [build(moof_len as i32 + 8), bx(b"mdat", &[&[0; 12]])].concat();
        let trafs = parse(&body, 0, &[(1, TREX)]).unwrap();
        let samples = &trafs[0].samples;
        assert_eq!(trafs[0].decode_time, None);
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.duration)
                .collect::<Vec<_>>(),
            [1024; 3]
        );
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.range.len())
                .collect::<Vec<_>>(),
            [4; 3]
        );
        assert_eq!(
            samples.iter().map(Sample::is_sync).collect::<Vec<_>>(),
            [true, false, false]
        );

        // tfhd defaults override trex.
        let tfhd = full(
            b"tfhd",
            0,
            DEFAULT_BASE_IS_MOOF | DEFAULT_DURATION | DEFAULT_SIZE | DEFAULT_FLAGS,
            &[
                &1u32.to_be_bytes(),
                &5u32.to_be_bytes(),
                &2u32.to_be_bytes(),
                &0x0400_0000u32.to_be_bytes(),
            ],
        );
        let build = |offset: i32| {
            let trun = trun(&[RunSample::default(); 2], Some(offset), None);
            bx(b"moof", &[&bx(b"traf", &[&tfhd, &trun])])
        };
        let moof_len = build(0).len();
        let body = [build(moof_len as i32 + 8), bx(b"mdat", &[&[0; 4]])].concat();
        let samples = &parse(&body, 0, &[(1, TREX)]).unwrap()[0].samples;
        assert_eq!((samples[1].duration, samples[1].range.len()), (5, 2));
        assert!(samples[1].is_sync());
        assert_eq!((samples[1].flags >> 26) & 3, 1);
        assert!(samples[1].is_undecodable_leading());
    }

    #[test]
    fn several_moofs_and_skipped_top_level_boxes() {
        let first = segment(1, 0, &[sized(2)], &[], b"ab");
        let second = segment(1, 2048, &[sized(3)], &[], b"cde");
        let body = [
            bx(b"styp", &[b"msdh"]),
            bx(b"emsg", &[&[0; 6]]),
            first,
            bx(b"prft", &[&[0; 12]]),
            second,
        ]
        .concat();
        let trafs = parse(&body, 0, &[(1, TREX)]).unwrap();
        assert_eq!(trafs.len(), 2);
        assert_eq!(&body[trafs[0].samples[0].range.clone()], b"ab");
        assert_eq!(&body[trafs[1].samples[0].range.clone()], b"cde");
        assert_eq!(trafs[1].decode_time, Some(2048));
    }

    #[test]
    fn explicit_base_data_offsets_count_from_the_resource() {
        // The moof sits 1000 bytes into its file; its traf names absolute offsets.
        let build = |base: u64| {
            let tfhd = full(
                b"tfhd",
                0,
                BASE_DATA_OFFSET,
                &[&1u32.to_be_bytes(), &base.to_be_bytes()],
            );
            bx(
                b"moof",
                &[&bx(b"traf", &[&tfhd, &trun(&[sized(3)], None, None)])],
            )
        };
        let moof_len = build(0).len() as u64;
        let body = [build(1000 + moof_len + 8), bx(b"mdat", &[b"xyz"])].concat();
        let trafs = parse(&body, 1000, &[(1, TREX)]).unwrap();
        assert_eq!(&body[trafs[0].samples[0].range.clone()], b"xyz");
        assert!(matches!(
            parse(&body, 2000, &[(1, TREX)]),
            Err(ParseError::Malformed(reason)) if reason.contains("precedes")
        ));
    }

    #[test]
    fn reads_fragment_roll_groups() {
        let sgpd = full(
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
        let body = segment(1, 0, &[sized(1); 3], &[sgpd], b"abc");
        let traf = &parse(&body, 0, &[(1, TREX)]).unwrap()[0];
        assert_eq!(traf.roll.as_ref().unwrap().distances, [-2]);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let body = segment(1, 0, &[sized(2), sized(2)], &[], b"abcd");
        for len in 0..body.len() {
            let _ = parse(&body[..len], 0, &[(1, TREX)]);
        }
        assert!(matches!(
            parse(&body[..body.len() - 1], 0, &[(1, TREX)]),
            Err(ParseError::Malformed(_))
        ));
    }
}
