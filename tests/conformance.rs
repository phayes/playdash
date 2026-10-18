//! Real-world DASH content from GPAC's Telecom ParisTech conformance set: every manifest
//! under `tests/files/conformance/pass` parses, and, to the level its `test_type` says,
//! lists its fragments and streams each representation, with `PacketStream` reading every
//! track. Streaming needs the `conformance` feature. See `tests/files/conformance/README.md`.
#![cfg(feature = "sync")]

mod common;

use playdash::DashManifest;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/files/conformance")
}

type Checked = Result<(), Box<dyn std::error::Error>>;

/// How much of a manifest is checked. Each level checks everything the ones before it do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TestType {
    /// It parses. Offline.
    Parse,
    /// Each representation's ID also reaches it, and it lists its fragments, one per
    /// media segment of its timeline. Offline.
    Manifest,
    /// Each representation also streams from GPAC's server. Needs the `conformance`
    /// feature.
    #[default]
    Full,
}

/// A manifest's entry in `index.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    /// Where the manifest was downloaded from, which its relative URLs resolve against.
    url: String,
    /// How much of it is checked. `full` when absent.
    #[serde(default)]
    test_type: TestType,
    /// Why a manifest under `fail/` fails, as this crate's fault.
    reason: Option<String>,
    /// Why a manifest under `invalid/` cannot pass whatever this crate does, as the
    /// manifest or its media is broken.
    invalid_reason: Option<String>,
    /// Anything else to know about the manifest, such as why its `test_type` is below
    /// `full`.
    note: Option<String>,
}

/// `index.json`: each manifest's entry, by its path under `pass/`, `fail/` or `invalid/`.
fn index() -> BTreeMap<String, Entry> {
    let path = root().join("index.json");
    let json = fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_slice(&json).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The paths of the manifests under `dir` of `tests/files/conformance`, relative to it.
fn manifests(dir: &str) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, found: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, found);
            } else if path.extension().is_some_and(|extension| extension == "mpd") {
                let relative = path.strip_prefix(base).unwrap();
                let parts: Vec<_> = relative.iter().map(|part| part.to_str().unwrap()).collect();
                found.push(parts.join("/"));
            }
        }
    }
    let base = root().join(dir);
    let mut found = Vec::new();
    walk(&base, &base, &mut found);
    found.sort();
    found
}

#[test]
fn index_lists_every_manifest() {
    let mut index = index();
    for dir in ["pass", "fail", "invalid"] {
        for path in manifests(dir) {
            let entry = index.remove(&path);
            let entry = entry.unwrap_or_else(|| panic!("{dir}/{path} is not in index.json"));
            assert!(
                entry.url.ends_with(&format!("/{path}")),
                "{dir}/{path} has the URL of another path: {}",
                entry.url
            );
            assert_eq!(
                (entry.reason.is_some(), entry.invalid_reason.is_some()),
                (dir == "fail", dir == "invalid"),
                "{dir}/{path}: each manifest under fail/, and only those, has a reason, and \
                 each under invalid/, and only those, has an invalid_reason in index.json"
            );
            assert!(
                dir != "pass" || entry.test_type == TestType::Full || entry.note.is_some(),
                "{dir}/{path}: its test_type is below full, so it needs a note saying why"
            );
        }
    }
    let missing: Vec<_> = index.keys().collect();
    assert!(
        missing.is_empty(),
        "in index.json but not under pass/, fail/ or invalid/: {missing:?}"
    );
}

#[test]
fn offline_manifests_check() {
    let index = index();
    let paths: Vec<String> = manifests("pass")
        .into_iter()
        .filter(|path| index[path].test_type < TestType::Full)
        .collect();
    let mut failures = Vec::new();
    for path in &paths {
        let entry = &index[path];
        let checked = parse(path, entry).and_then(|manifest| {
            if entry.test_type >= TestType::Manifest {
                check_ids(&manifest)?;
                check_fragments(&manifest)?;
            }
            Ok(())
        });
        if let Err(error) = checked {
            failures.push(format!("{path}: {error}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} manifests failed:\n{}",
        failures.len(),
        paths.len(),
        failures.join("\n")
    );
}

/// Parses manifest `path` of `pass/`, resolving against where it was downloaded from.
fn parse(path: &str, entry: &Entry) -> Result<DashManifest, Box<dyn std::error::Error>> {
    let xml = fs::read_to_string(root().join("pass").join(path))?;
    Ok(DashManifest::new(xml)?.with_base_url(&entry.url)?)
}

/// Checks each representation of `manifest` is the one its ID reaches.
fn check_ids(manifest: &DashManifest) -> Checked {
    for representation in manifest.representations() {
        let id = representation.id;
        let reached = manifest.representation(id).ok_or("not reached by its ID")?;
        if reached.adaptation_set != representation.adaptation_set {
            return Err(format!(
                "representation {id} of adaptation set {} is reached in adaptation set {}",
                representation.adaptation_set, reached.adaptation_set
            )
            .into());
        }
    }
    Ok(())
}

/// Checks each representation of `manifest` lists at least one timed media fragment.
fn check_fragments(manifest: &DashManifest) -> Checked {
    for representation in manifest.representations() {
        let id = representation.id;
        let fragments = manifest
            .fragments(id)
            .map_err(|error| format!("representation {id}: {error}"))?;
        if fragments.media_len() == 0 {
            return Err(format!("representation {id} has no media fragments").into());
        }
        if fragments.timing().is_none() {
            return Err(format!("representation {id} has untimed media fragments").into());
        }
    }
    Ok(())
}

/// Streaming each `full` manifest under `pass/` from GPAC's server.
#[cfg(feature = "conformance")]
mod stream {
    use super::{Checked, Entry, TestType, check_fragments, check_ids, index, manifests, parse};
    use crate::common::{self, Probe};
    use playdash::fragment::{Fragment, FragmentStream, MediaTiming};
    use playdash::packet::{Packet, PacketStream, Track};
    use playdash::{
        Bytes, ContentKind, DashManifest, Error, FragmentCache, Representation, Retention,
        Transport,
    };
    use std::panic::{self, AssertUnwindSafe};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::{env, fs, thread};

    /// Media fragments read from the start of each track.
    const FRAGMENTS: usize = 2;

    /// Manifests checked at once.
    const WORKERS: usize = 4;

    /// Packets read after a seek, looking for the one presenting at the seek time.
    const AFTER_SEEK: usize = 1_000;

    #[test]
    fn passing_manifests_stream() {
        let index = index();
        let filter = env::var("PLAYDASH_CONFORMANCE_FILTER").unwrap_or_default();
        let paths: Vec<String> = manifests("pass")
            .into_iter()
            .filter(|path| index[path].test_type == TestType::Full && path.contains(&filter))
            .collect();
        let transport = Arc::new(Network::from_env());
        let ffprobe = Command::new("ffprobe").arg("-version").output().is_ok();

        // Check WORKERS manifests at a time, collecting every failure.
        let queue = Mutex::new(paths.iter());
        let failures = Mutex::new(Vec::new());
        thread::scope(|scope| {
            for _ in 0..WORKERS {
                scope.spawn(|| {
                    loop {
                        let Some(path) = queue.lock().unwrap().next() else {
                            break;
                        };
                        let entry = &index[path];
                        let checked = panic::catch_unwind(AssertUnwindSafe(|| {
                            check_manifest(path, entry, &transport, ffprobe)
                        }));
                        let failure = match checked {
                            Ok(Ok(())) => continue,
                            Ok(Err(error)) => error.to_string(),
                            Err(panic) => panic_message(&panic),
                        };
                        failures.lock().unwrap().push(format!("{path}: {failure}"));
                    }
                });
            }
        });

        let mut failures = failures.into_inner().unwrap();
        failures.sort();
        assert!(
            failures.is_empty(),
            "{} of {} manifests failed:\n{}",
            failures.len(),
            paths.len(),
            failures.join("\n")
        );
    }

    fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
        match (panic.downcast_ref::<String>(), panic.downcast_ref::<&str>()) {
            (Some(message), _) => message.clone(),
            (None, Some(message)) => (*message).to_owned(),
            (None, None) => "panicked".to_owned(),
        }
    }

    /// Checks manifest `path` of `pass/`: it passes the `manifest` checks once its segment
    /// indexes load, and every representation streams.
    fn check_manifest(
        path: &str,
        entry: &Entry,
        transport: &Arc<Network>,
        ffprobe: bool,
    ) -> Checked {
        let mut manifest = parse(path, entry)?;
        manifest.load_segment_indexes(transport.as_ref());
        check_ids(&manifest)?;
        check_fragments(&manifest)?;

        let cache = Arc::new(FragmentCache::new(transport.clone()));
        for representation in manifest.representations() {
            let id = representation.id;
            check_representation(&manifest, &representation, &cache, ffprobe)
                .map_err(|error| format!("representation {id}: {error}"))?;
        }
        Ok(())
    }

    /// Checks `representation` of `manifest` lists the tracks its kind says, and reads
    /// each.
    fn check_representation(
        manifest: &DashManifest,
        representation: &Representation,
        cache: &Arc<FragmentCache>,
        ffprobe: bool,
    ) -> Checked {
        let id = representation.id;
        let open = || -> Result<FragmentStream, Error> {
            Ok(manifest
                .fragment_stream_with_cache(id, cache.clone())?
                .with_retention(Retention {
                    behind: Some(Duration::ZERO),
                    ahead: Some(Duration::ZERO),
                }))
        };

        // The tracks, of the kinds the manifest says.
        let tracks = open()?.tracks()?;
        let kinds: Vec<ContentKind> = tracks.iter().map(|track| track.kind).collect();
        let expected = match representation.kind {
            ContentKind::Muxed => vec![ContentKind::Video, ContentKind::Audio],
            ContentKind::Unknown => kinds.clone(),
            kind => vec![kind],
        };
        assert!(
            kinds.len() == expected.len() && expected.iter().all(|kind| kinds.contains(kind)),
            "representation {id} is {:?} but has tracks {kinds:?}",
            representation.kind
        );

        // Where reading stops, and where to seek: the middle of the representation.
        let fragments = manifest.fragments(id)?;
        let span = |index| fragments.span(index).expect("timed fragments");
        let count = fragments.media_len();
        let read = span(FRAGMENTS.min(count) - 1).end - span(0).start;
        let middle = (span(0).start + span(count - 1).end) / 2;
        let timing = *fragments.timing().expect("timed fragments");
        for track in &tracks {
            if track.kind == ContentKind::Text {
                continue;
            }
            let context = format!("representation {id} track {}", track.id);
            let times = Times {
                read,
                seek: middle,
                timing,
            };
            check_track(&context, &open, track, times, ffprobe)
                .map_err(|error| format!("{context}: {error}"))?;
        }
        Ok(())
    }

    /// Where a track's checks read and seek, on the representation's media timeline.
    struct Times {
        /// Media read from the start.
        read: Duration,
        /// Where to seek.
        seek: Duration,
        /// The representation's media clock: packet timestamps count from its
        /// presentation time offset, placed at its period start.
        timing: MediaTiming,
    }

    /// Checks `track` of the representation `open` streams: the packets of its first
    /// `times.read` of media, then a packet-accurate seek to `times.seek`.
    fn check_track(
        context: &str,
        open: &dyn Fn() -> Result<FragmentStream, Error>,
        track: &Track,
        times: Times,
        ffprobe: bool,
    ) -> Checked {
        let Times { read, seek, timing } = times;
        // The packets from the start.
        let mut stream = PacketStream::with_track(open()?, track.id)?;
        assert_eq!(stream.timescale(), track.timescale, "{context} timescale");
        let first = stream.next().ok_or("no packets")??;
        let end = first.dts + ticks(read, track.timescale);
        let mut packets = vec![first];
        for packet in stream {
            let packet = packet?;
            if packet.dts >= end {
                break;
            }
            packets.push(packet);
        }
        check_packets(context, &packets);
        if ffprobe {
            // Media time, in the track's ticks, of presentation time zero.
            let offset = u128::from(timing.presentation_time_offset) * u128::from(track.timescale)
                / u128::from(timing.timescale.get());
            let origin = offset as i64 - ticks(timing.period_start, track.timescale);
            check_probed(context, open()?, track, origin, &packets)?;
        }

        // A seek to the middle.
        let mut stream = PacketStream::with_track(open()?, track.id)?;
        let seeked = stream.seek_time(seek)?;
        let at = ticks(seek, track.timescale);
        assert_eq!(seeked.discard_before, at, "{context} seek discard_before");
        let after: Vec<Packet> = stream.take(AFTER_SEEK).collect::<Result<_, _>>()?;
        let next = after.first().ok_or("no packets after the seek")?;
        assert_eq!(
            next.pts, seeked.landed,
            "{context} seek lands on the next packet"
        );
        assert!(
            next.pts + next.duration as i64 > at,
            "{context} seek lands on a packet ending after {at}, not {}+{}",
            next.pts,
            next.duration
        );
        assert!(
            seeked.preroll.iter().all(|packet| packet.pts < at),
            "{context} seek pre-roll presents before {at}"
        );
        let decoded = seeked.preroll.first().unwrap_or(next);
        assert!(
            decoded.keyframe,
            "{context} seek decodes from a keyframe, not {}",
            decoded.pts
        );
        assert!(
            after
                .iter()
                .any(|packet| packet.pts <= at && at < packet.pts + packet.duration as i64),
            "{context} seek decodes the packet presenting at {at}"
        );
        Ok(())
    }

    /// Checks `packets`, read from a track's start, are in order and decodable.
    fn check_packets(context: &str, packets: &[Packet]) {
        assert!(packets[0].keyframe, "{context} starts with a keyframe");
        for (index, packet) in packets.iter().enumerate() {
            assert!(!packet.data.is_empty(), "{context} packet {index} is empty");
        }
        for (index, pair) in packets.windows(2).enumerate() {
            assert!(
                pair[0].dts < pair[1].dts,
                "{context} dts increases at packet {}: {} then {}",
                index + 1,
                pair[0].dts,
                pair[1].dts
            );
        }
        let mut pts: Vec<i64> = packets.iter().map(|packet| packet.pts).collect();
        pts.sort_unstable();
        for pair in pts.windows(2) {
            assert!(pair[0] < pair[1], "{context} pts {} repeats", pair[0]);
        }
    }

    /// Checks the first of `packets` are ffprobe's packets of `track` in the first media
    /// fragment of `stream`, whose media times start `origin` ticks before the packets'.
    fn check_probed(
        context: &str,
        mut stream: FragmentStream,
        track: &Track,
        origin: i64,
        packets: &[Packet],
    ) -> Checked {
        let init = stream.next().ok_or("no initialization fragment")??;
        let media = stream.next().ok_or("no media fragment")??;
        let mut expected = ffprobe(&[&init[..], &without_sidx(&media)].concat(), track.id)?;
        for packet in &mut expected {
            packet.pts -= origin;
            packet.dts -= origin;
        }
        assert!(
            packets.len() >= expected.len(),
            "{context} read {} packets, fewer than the {} of its first fragment",
            packets.len(),
            expected.len()
        );
        let audio = track.kind == ContentKind::Audio;
        common::check_packets(context, &packets[..expected.len()], &expected, audio);
        Ok(())
    }

    /// `media` with its top-level `sidx` boxes renamed `free`. Their offsets no longer
    /// hold once it follows the initialization fragment, and ffmpeg refuses an `sidx`
    /// that references another.
    fn without_sidx(media: &[u8]) -> Vec<u8> {
        let mut media = media.to_vec();
        let mut pos = 0;
        while pos + 8 <= media.len() {
            let size = u32::from_be_bytes(media[pos..pos + 4].try_into().unwrap()) as usize;
            if &media[pos + 4..pos + 8] == b"sidx" {
                media[pos + 4..pos + 8].copy_from_slice(b"free");
            }
            if size < 8 {
                break;
            }
            pos += size;
        }
        media
    }

    /// ffprobe's packets of track `track_id` of MP4 `file`.
    fn ffprobe(
        file: &[u8],
        track_id: u32,
    ) -> Result<Vec<common::Probed>, Box<dyn std::error::Error>> {
        static FILES: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "playdash-conformance-{}-{}.mp4",
            std::process::id(),
            FILES.fetch_add(1, Ordering::Relaxed)
        );
        let path = env::temp_dir().join(name);
        fs::write(&path, file)?;
        let output = Command::new("ffprobe")
            .args(["-v", "error", "-show_data_hash", "CRC32", "-of", "json=c=1"])
            .args(["-select_streams", &format!("i:{track_id}")])
            .args([
                "-show_entries",
                "packet=pts,dts,duration,size,flags,data_hash:packet_side_data=skip_samples",
            ])
            .arg(&path)
            .output();
        let _ = fs::remove_file(&path);
        let output = output?;
        if !output.status.success() {
            return Err(format!("ffprobe: {}", String::from_utf8_lossy(&output.stderr)).into());
        }
        Ok(serde_json::from_slice::<Probe>(&output.stdout)?.packets)
    }

    /// `time` in ticks of `timescale`, to the nearest tick.
    fn ticks(time: Duration, timescale: u32) -> i64 {
        ((time.as_nanos() * u128::from(timescale) + 500_000_000) / 1_000_000_000) as i64
    }

    /// Fetches over HTTP. With `PLAYDASH_CONFORMANCE_CACHE` naming a directory, keeps each
    /// body there and reads it back on later runs.
    struct Network {
        agent: ureq::Agent,
        cache: Option<PathBuf>,
    }

    impl Network {
        fn from_env() -> Self {
            Self {
                agent: ureq::Agent::new_with_defaults(),
                cache: env::var_os("PLAYDASH_CONFORMANCE_CACHE").map(PathBuf::from),
            }
        }

        /// Where `fragment` is kept under `cache`: its URL's host and path, and its range.
        fn cached_path(cache: &Path, fragment: &Fragment) -> PathBuf {
            let url = fragment
                .url
                .split_once("://")
                .map_or(&*fragment.url, |(_, rest)| rest);
            let mut path = cache.to_owned();
            path.extend(
                url.split(['/', '?', '#'])
                    .filter(|part| !part.is_empty() && *part != "." && *part != ".."),
            );
            if let Some(range) = &fragment.range {
                path.as_mut_os_string()
                    .push(format!("@{}-{}", range.start(), range.end()));
            }
            path
        }
    }

    impl Transport for Network {
        fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Bytes, Error> {
            let Some(cache) = &self.cache else {
                return Transport::get(&self.agent, fragment, size_hint);
            };
            let path = Self::cached_path(cache, fragment);
            if let Ok(body) = fs::read(&path) {
                return Ok(Bytes::from(body));
            }

            // Write through a temporary file, as another worker may fetch the same one.
            let body = Transport::get(&self.agent, fragment, size_hint)?;
            let mut partial = path.clone();
            partial
                .as_mut_os_string()
                .push(format!(".part-{:?}", thread::current().id()));
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&partial, &body)?;
            fs::rename(&partial, &path)?;
            Ok(body)
        }

        fn head(&self, fragment: &Fragment) -> Result<u64, Error> {
            Transport::head(&self.agent, fragment)
        }
    }
}
