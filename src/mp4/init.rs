//! Initialization segments: the `moov` box's tracks, as fragmented media needs them.
//!
//! Sample entries are located but their codec configuration is not parsed here, so a track
//! with an unfamiliar or damaged configuration never stops other tracks from being read.

use super::reader::{Atom, ParseError, atoms, find, fourcc, malformed, require, unsupported};

/// A parsed initialization segment.
#[derive(Debug)]
pub(crate) struct Init {
    /// Ticks per second of `mvhd`, which edit-list durations and `fragment_duration` use.
    pub(crate) movie_timescale: u32,
    /// Duration of the whole fragmented movie from `mehd`, in `movie_timescale` ticks.
    pub(crate) fragment_duration: Option<u64>,
    pub(crate) tracks: Vec<Track>,
}

/// One `trak`, with its `trex` defaults.
#[derive(Debug)]
pub(crate) struct Track {
    pub(crate) id: u32,
    /// `hdlr` handler type, such as `soun` or `vide`.
    pub(crate) handler: [u8; 4],
    /// Ticks per second of the track's media timestamps (`mdhd`).
    pub(crate) timescale: u32,
    /// `elst` entries, in order. Empty without an edit list.
    pub(crate) edits: Vec<Edit>,
    /// `stsd` sample entries; movie fragments pick one by 1-based index.
    pub(crate) entries: Vec<SampleEntry>,
    pub(crate) defaults: TrackDefaults,
    /// `roll` or `prol` sample group descriptions from `stbl`.
    pub(crate) roll: Option<RollGroups>,
}

/// One edit-list entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Edit {
    /// Presentation duration in movie timescale ticks. Zero is common in fragmented files,
    /// where it means unknown.
    pub(crate) segment_duration: u64,
    /// Media time the edit starts at, in track timescale ticks; `-1` for an empty edit.
    pub(crate) media_time: i64,
    /// Integer part of the playback rate.
    pub(crate) rate: i16,
}

/// `trex` per-sample defaults, which `tfhd` and `trun` override.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrackDefaults {
    pub(crate) sample_description_index: u32,
    pub(crate) sample_duration: u32,
    pub(crate) sample_size: u32,
    pub(crate) sample_flags: u32,
}

/// `roll` (audio roll recovery) or `prol` (audio pre-roll) sample group descriptions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RollGroups {
    pub(crate) grouping_type: [u8; 4],
    /// Roll distance of each description, in samples. Negative counts samples before.
    pub(crate) distances: Vec<i16>,
    /// The 1-based description samples belong to without a `sbgp` saying otherwise
    /// (`sgpd` version 2).
    pub(crate) default_index: Option<u32>,
}

/// A sample entry in `stsd`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleEntry {
    pub(crate) atom: Atom,
    /// Offset of the child boxes after the entry's fixed fields; `None` for an entry whose
    /// fixed fields this crate does not know.
    pub(crate) children: Option<usize>,
}

impl SampleEntry {
    /// The entry's type as stored, such as `mp4a` or, when protected, `enca`.
    pub(crate) fn kind(&self) -> [u8; 4] {
        self.atom.kind
    }

    /// Offset of the entry's 4-byte type.
    #[cfg(feature = "encryption")]
    pub(crate) fn type_offset(&self) -> usize {
        self.atom.start + 4
    }

    /// The first child box of type `kind`.
    pub(crate) fn child(&self, buf: &[u8], kind: &[u8; 4]) -> Result<Option<Atom>, ParseError> {
        match self.children {
            Some(children) => find(atoms(buf, children, self.atom.end), kind),
            None => Ok(None),
        }
    }

    /// The protection scheme box, present when the entry is or was protected.
    pub(crate) fn sinf(&self, buf: &[u8]) -> Result<Option<Atom>, ParseError> {
        self.child(buf, b"sinf")
    }

    /// The entry's media format: its type, or for a protected entry the original type
    /// recorded in `sinf/frma`.
    pub(crate) fn format(&self, buf: &[u8]) -> Result<[u8; 4], ParseError> {
        let Some(sinf) = self.sinf(buf)? else {
            return Ok(self.kind());
        };
        match find(sinf.children(buf), b"frma")? {
            Some(frma) => frma.fields(buf).array(),
            None => Ok(self.kind()),
        }
    }

    /// Whether the entry's type still marks its samples as encrypted.
    pub(crate) fn is_encrypted(&self) -> bool {
        matches!(&self.kind(), b"enca" | b"encv" | b"enct" | b"encs")
    }
}

/// Parses the `moov` box of an initialization segment.
pub(crate) fn parse(buf: &[u8]) -> Result<Init, ParseError> {
    let moov = require(atoms(buf, 0, buf.len()), b"moov")?;

    let mut mvhd = require(moov.children(buf), b"mvhd")?.fields(buf);
    let (version, _) = mvhd.version_flags()?;
    // creation and modification times
    mvhd.skip(if version == 1 { 16 } else { 8 })?;
    let movie_timescale = mvhd.u32()?;

    // Track fragment defaults and the overall duration.
    let mut defaults = Vec::new();
    let mut fragment_duration = None;
    if let Some(mvex) = find(moov.children(buf), b"mvex")? {
        for child in mvex.children(buf) {
            let child = child?;
            match &child.kind {
                b"trex" => defaults.push(trex(buf, child)?),
                b"mehd" => {
                    let mut mehd = child.fields(buf);
                    let (version, _) = mehd.version_flags()?;
                    fragment_duration = Some(match version {
                        1 => mehd.u64()?,
                        _ => u64::from(mehd.u32()?),
                    });
                }
                _ => {}
            }
        }
    }

    let mut tracks = Vec::new();
    for trak in moov.children(buf) {
        let trak = trak?;
        if &trak.kind == b"trak" {
            tracks.push(track(buf, trak, &defaults)?);
        }
    }
    Ok(Init {
        movie_timescale,
        fragment_duration,
        tracks,
    })
}

fn trex(buf: &[u8], trex: Atom) -> Result<(u32, TrackDefaults), ParseError> {
    let mut fields = trex.fields(buf);
    fields.version_flags()?;
    let track_id = fields.u32()?;
    Ok((
        track_id,
        TrackDefaults {
            sample_description_index: fields.u32()?,
            sample_duration: fields.u32()?,
            sample_size: fields.u32()?,
            sample_flags: fields.u32()?,
        },
    ))
}

fn track(buf: &[u8], trak: Atom, defaults: &[(u32, TrackDefaults)]) -> Result<Track, ParseError> {
    let mut tkhd = require(trak.children(buf), b"tkhd")?.fields(buf);
    let (version, _) = tkhd.version_flags()?;
    // creation and modification times
    tkhd.skip(if version == 1 { 16 } else { 8 })?;
    let id = tkhd.u32()?;

    let mdia = require(trak.children(buf), b"mdia")?;
    let mut mdhd = require(mdia.children(buf), b"mdhd")?.fields(buf);
    let (version, _) = mdhd.version_flags()?;
    // creation and modification times
    mdhd.skip(if version == 1 { 16 } else { 8 })?;
    let timescale = mdhd.u32()?;

    let mut hdlr = require(mdia.children(buf), b"hdlr")?.fields(buf);
    hdlr.version_flags()?;
    hdlr.skip(4)?; // pre_defined
    let handler = hdlr.array::<4>()?;

    let edits = match find(trak.children(buf), b"edts")? {
        Some(edts) => match find(edts.children(buf), b"elst")? {
            Some(elst) => edit_list(buf, elst)?,
            None => Vec::new(),
        },
        None => Vec::new(),
    };

    // Sample descriptions and roll groups.
    let minf = require(mdia.children(buf), b"minf")?;
    let stbl = require(minf.children(buf), b"stbl")?;
    let stsd = require(stbl.children(buf), b"stsd")?;
    let mut fields = stsd.fields(buf);
    fields.version_flags()?;
    fields.u32()?; // entry_count
    let mut entries = Vec::new();
    for entry in atoms(buf, fields.pos, stsd.end) {
        entries.push(sample_entry(buf, entry?, &handler)?);
    }
    let mut roll = None;
    for child in stbl.children(buf) {
        let child = child?;
        if &child.kind == b"sgpd"
            && let Some(groups) = roll_groups(buf, child)?
        {
            roll = Some(groups);
        }
    }

    Ok(Track {
        id,
        handler,
        timescale,
        edits,
        entries,
        defaults: defaults
            .iter()
            .find(|(track_id, _)| *track_id == id)
            .map(|(_, defaults)| *defaults)
            .unwrap_or_default(),
        roll,
    })
}

fn edit_list(buf: &[u8], elst: Atom) -> Result<Vec<Edit>, ParseError> {
    let mut fields = elst.fields(buf);
    let (version, _) = fields.version_flags()?;
    let count = fields.u32()?;
    let mut edits = Vec::new();
    for _ in 0..count {
        let (segment_duration, media_time) = match version {
            1 => (fields.u64()?, fields.u64()? as i64),
            _ => (u64::from(fields.u32()?), i64::from(fields.i32()?)),
        };
        let rate = fields.u16()? as i16;
        fields.skip(2)?; // media_rate_fraction
        edits.push(Edit {
            segment_duration,
            media_time,
            rate,
        });
    }
    Ok(edits)
}

/// Locates the child boxes of a sample entry of a track with `handler`. Audio entries'
/// fixed fields depend on their QuickTime sound description version; visual entries' are
/// fixed. Other entries' children are not looked for.
fn sample_entry(buf: &[u8], entry: Atom, handler: &[u8; 4]) -> Result<SampleEntry, ParseError> {
    // Every SampleEntry starts with 6 reserved bytes and a data_reference_index.
    const HEADER: usize = 8;
    let fixed = match handler {
        b"soun" => {
            // AudioSampleEntry: 20 bytes of audio fields, which QuickTime sound
            // description versions 1 and 2 extend before the child boxes.
            let mut fields = entry.fields(buf);
            fields.skip(HEADER)?;
            let extension = match fields.u16()? {
                0 => 0,
                1 => 16,
                2 => 36,
                version => {
                    return Err(unsupported(format!("audio sample entry version {version}")));
                }
            };
            Some(HEADER + 20 + extension)
        }
        // VisualSampleEntry: 70 bytes of visual fields.
        b"vide" => Some(HEADER + 70),
        _ => None,
    };
    let children = fixed
        .map(|fixed| entry.payload + fixed)
        .filter(|&children| children <= entry.end);
    if handler == b"soun" && children.is_none() {
        return Err(malformed(format!("truncated {} box", fourcc(&entry.kind))));
    }
    Ok(SampleEntry {
        atom: entry,
        children,
    })
}

/// The descriptions of a `roll` or `prol` `sgpd`; `None` for other grouping types.
pub(crate) fn roll_groups(buf: &[u8], sgpd: Atom) -> Result<Option<RollGroups>, ParseError> {
    let mut fields = sgpd.fields(buf);
    let (version, _) = fields.version_flags()?;
    let grouping_type = fields.array::<4>()?;
    if !matches!(&grouping_type, b"roll" | b"prol") {
        return Ok(None);
    }
    let default_length = match version {
        0 => 2,
        _ => fields.u32()?,
    };
    let default_index = match version {
        0 | 1 => None,
        _ => Some(fields.u32()?),
    };
    let count = fields.u32()?;
    let mut distances = Vec::new();
    for _ in 0..count {
        let length = match default_length {
            0 => fields.u32()?,
            length => length,
        };
        let &[high, low, ..] = fields.bytes(length as usize)? else {
            return Err(malformed("truncated sgpd box"));
        };
        distances.push(i16::from_be_bytes([high, low]));
    }
    Ok(Some(RollGroups {
        grouping_type,
        distances,
        default_index,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
        let len = 8 + parts.iter().map(|part| part.len()).sum::<usize>();
        let mut out = (len as u32).to_be_bytes().to_vec();
        out.extend(kind);
        for part in parts {
            out.extend(*part);
        }
        out
    }

    pub(crate) fn full(kind: &[u8; 4], version: u8, flags: u32, parts: &[&[u8]]) -> Vec<u8> {
        let header = (u32::from(version) << 24 | flags).to_be_bytes();
        let mut all: Vec<&[u8]> = vec![&header];
        all.extend(parts);
        bx(kind, &all)
    }

    /// A track description: everything `trak` needs but its sample entries.
    pub(crate) struct TrackSpec<'a> {
        pub(crate) id: u32,
        pub(crate) handler: &'a [u8; 4],
        pub(crate) timescale: u32,
        pub(crate) entries: &'a [Vec<u8>],
        /// Extra `trak` children, such as `edts`.
        pub(crate) trak_extra: &'a [Vec<u8>],
        /// Extra `stbl` children, such as `sgpd`.
        pub(crate) stbl_extra: &'a [Vec<u8>],
        pub(crate) trex: TrackDefaults,
    }

    pub(crate) fn trak(spec: &TrackSpec<'_>) -> Vec<u8> {
        let mut stsd_body = (spec.entries.len() as u32).to_be_bytes().to_vec();
        for entry in spec.entries {
            stsd_body.extend(entry);
        }
        let mut stbl: Vec<&[u8]> = Vec::new();
        let stsd = full(b"stsd", 0, 0, &[&stsd_body]);
        stbl.push(&stsd);
        for extra in spec.stbl_extra {
            stbl.push(extra);
        }
        let minf = bx(b"minf", &[&bx(b"stbl", &stbl)]);
        let hdlr = full(b"hdlr", 0, 0, &[&[0; 4], spec.handler, &[0; 13]]);
        let mdhd = full(
            b"mdhd",
            0,
            0,
            &[&[0; 8], &spec.timescale.to_be_bytes(), &[0; 8]],
        );
        let tkhd = full(b"tkhd", 0, 3, &[&[0; 8], &spec.id.to_be_bytes(), &[0; 68]]);
        let mut trak: Vec<&[u8]> = vec![&tkhd];
        for extra in spec.trak_extra {
            trak.push(extra);
        }
        let mdia = bx(b"mdia", &[&mdhd, &hdlr, &minf]);
        trak.push(&mdia);
        bx(b"trak", &trak)
    }

    pub(crate) fn trex(id: u32, defaults: TrackDefaults) -> Vec<u8> {
        full(
            b"trex",
            0,
            0,
            &[
                &id.to_be_bytes(),
                &defaults.sample_description_index.to_be_bytes(),
                &defaults.sample_duration.to_be_bytes(),
                &defaults.sample_size.to_be_bytes(),
                &defaults.sample_flags.to_be_bytes(),
            ],
        )
    }

    /// An `ftyp` and `moov` with `tracks`.
    pub(crate) fn init(tracks: &[TrackSpec<'_>]) -> Vec<u8> {
        let mvhd = full(b"mvhd", 0, 0, &[&[0; 8], &1000u32.to_be_bytes(), &[0; 84]]);
        let traks: Vec<Vec<u8>> = tracks.iter().map(trak).collect();
        let trexes: Vec<Vec<u8>> = tracks.iter().map(|spec| trex(spec.id, spec.trex)).collect();
        let trex_refs: Vec<&[u8]> = trexes.iter().map(Vec::as_slice).collect();
        let mvex = bx(b"mvex", &trex_refs);
        let mut moov: Vec<&[u8]> = vec![&mvhd];
        for trak in &traks {
            moov.push(trak);
        }
        moov.push(&mvex);
        [bx(b"ftyp", &[b"iso6", &[0; 4]]), bx(b"moov", &moov)].concat()
    }

    /// An AudioSampleEntry of type `kind` with `children`.
    pub(crate) fn audio_entry(
        kind: &[u8; 4],
        channels: u16,
        sample_rate: u16,
        children: &[&[u8]],
    ) -> Vec<u8> {
        let mut fields = [0u8; 28];
        fields[7] = 1; // data_reference_index
        fields[16..18].copy_from_slice(&channels.to_be_bytes());
        fields[18..20].copy_from_slice(&16u16.to_be_bytes()); // samplesize
        fields[24..26].copy_from_slice(&sample_rate.to_be_bytes());
        let mut parts: Vec<&[u8]> = vec![&fields];
        parts.extend(children);
        bx(kind, &parts)
    }

    /// A VisualSampleEntry of type `kind` with `children`.
    pub(crate) fn video_entry(
        kind: &[u8; 4],
        width: u16,
        height: u16,
        children: &[&[u8]],
    ) -> Vec<u8> {
        let mut fields = [0u8; 78];
        fields[7] = 1; // data_reference_index
        fields[24..26].copy_from_slice(&width.to_be_bytes());
        fields[26..28].copy_from_slice(&height.to_be_bytes());
        let mut parts: Vec<&[u8]> = vec![&fields];
        parts.extend(children);
        bx(kind, &parts)
    }

    pub(crate) const SOUND: TrackSpec<'static> = TrackSpec {
        id: 1,
        handler: b"soun",
        timescale: 44_100,
        entries: &[],
        trak_extra: &[],
        stbl_extra: &[],
        trex: TrackDefaults {
            sample_description_index: 1,
            sample_duration: 1024,
            sample_size: 0,
            sample_flags: 0,
        },
    };

    #[test]
    fn reads_tracks_defaults_edits_and_roll_groups() {
        let audio = [audio_entry(b"mp4a", 2, 44_100, &[&bx(b"esds", &[&[0; 4]])])];
        let video = [video_entry(
            b"avc1",
            640,
            360,
            &[&bx(b"avcC", &[&[1, 2, 3]])],
        )];
        let elst = full(
            b"elst",
            0,
            0,
            &[
                &1u32.to_be_bytes(),
                &0u32.to_be_bytes(),
                &2112u32.to_be_bytes(),
                &[0, 1, 0, 0],
            ],
        );
        let edts = [bx(b"edts", &[&elst])];
        let roll_entry = (-1i16).to_be_bytes();
        let sgpd = [full(
            b"sgpd",
            1,
            0,
            &[
                b"roll",
                &2u32.to_be_bytes(),
                &1u32.to_be_bytes(),
                &roll_entry,
            ],
        )];
        let init = init(&[
            TrackSpec {
                entries: &audio,
                trak_extra: &edts,
                stbl_extra: &sgpd,
                ..SOUND
            },
            TrackSpec {
                id: 2,
                handler: b"vide",
                timescale: 90_000,
                entries: &video,
                ..SOUND
            },
        ]);

        let parsed = parse(&init).unwrap();
        assert_eq!(parsed.movie_timescale, 1000);
        assert_eq!(parsed.fragment_duration, None);
        let [sound, picture] = &parsed.tracks[..] else {
            panic!("two tracks");
        };
        assert_eq!(
            (sound.id, &sound.handler, sound.timescale),
            (1, b"soun", 44_100)
        );
        assert_eq!(sound.defaults.sample_duration, 1024);
        assert_eq!(
            sound.edits,
            [Edit {
                segment_duration: 0,
                media_time: 2112,
                rate: 1,
            }]
        );
        assert_eq!(
            sound.roll,
            Some(RollGroups {
                grouping_type: *b"roll",
                distances: vec![-1],
                default_index: None,
            })
        );
        let esds = sound.entries[0].child(&init, b"esds").unwrap().unwrap();
        assert_eq!(&init[esds.payload..esds.end], [0; 4]);

        assert_eq!((picture.id, &picture.handler), (2, b"vide"));
        assert!(picture.edits.is_empty());
        let avcc = picture.entries[0].child(&init, b"avcC").unwrap().unwrap();
        assert_eq!(&init[avcc.payload..avcc.end], [1, 2, 3]);
        assert_eq!(picture.entries[0].format(&init).unwrap(), *b"avc1");
    }

    #[test]
    fn protected_entries_report_their_original_format() {
        let sinf = bx(b"sinf", &[&bx(b"frma", &[b"fLaC"])]);
        let entries = [audio_entry(b"enca", 1, 22_050, &[&sinf])];
        let init = init(&[TrackSpec {
            entries: &entries,
            ..SOUND
        }]);
        let parsed = parse(&init).unwrap();
        let entry = &parsed.tracks[0].entries[0];
        assert!(entry.is_encrypted());
        assert_eq!(entry.format(&init).unwrap(), *b"fLaC");
        #[cfg(feature = "encryption")]
        assert_eq!(&init[entry.type_offset()..][..4], b"enca");
    }

    #[test]
    fn unknown_handlers_and_short_visual_entries_are_tolerated() {
        let text = [bx(b"stpp", &[&[0; 3]])];
        let short = [bx(b"avc1", &[&[0; 10]])];
        let init = init(&[
            TrackSpec {
                handler: b"subt",
                entries: &text,
                ..SOUND
            },
            TrackSpec {
                id: 2,
                handler: b"vide",
                entries: &short,
                ..SOUND
            },
        ]);
        let parsed = parse(&init).unwrap();
        for track in &parsed.tracks {
            assert_eq!(track.entries[0].children, None);
            assert_eq!(
                track.entries[0].sinf(&init).unwrap().map(|atom| atom.kind),
                None
            );
        }
    }

    #[test]
    fn unsupported_audio_entry_versions_are_named() {
        let mut entry = audio_entry(b"mp4a", 2, 44_100, &[]);
        entry[8 + 8..8 + 10].copy_from_slice(&3u16.to_be_bytes());
        let entries = [entry];
        let init = init(&[TrackSpec {
            entries: &entries,
            ..SOUND
        }]);
        assert!(matches!(
            parse(&init),
            Err(ParseError::Unsupported(reason)) if reason == "audio sample entry version 3"
        ));
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let audio = [audio_entry(b"mp4a", 2, 44_100, &[&bx(b"esds", &[&[0; 4]])])];
        let init = init(&[TrackSpec {
            entries: &audio,
            ..SOUND
        }]);
        for len in 0..init.len() {
            assert!(parse(&init[..len]).is_err());
        }
    }
}
