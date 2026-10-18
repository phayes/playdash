//! Fixtures served as DASH representations, and comparing `PacketStream` packets with
//! ffprobe's, shared by the integration tests.
#![allow(dead_code)]

use playdash::packet::Packet;
use playdash::segment::{SegmentLocation, SegmentStream, Segments};
use playdash::{Bytes, DashManifest, Error, SegmentCache, Transport};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The URL prefix [`Fixtures`] serves `tests/files` under.
pub const BASE: &str = "https://cdn.test/";

/// The contents of `path`, under `tests/files`.
pub fn fixture(path: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/files")
        .join(path);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Serves fixtures under `tests/files` by URL path, honoring byte ranges, and records
/// each GET.
#[derive(Default)]
pub struct Fixtures {
    pub gets: Mutex<Vec<SegmentLocation>>,
}

impl Fixtures {
    /// GETs so far of `name`, a file under `tests/files/packets`.
    pub fn gets_of(&self, name: &str) -> usize {
        let url = format!("{BASE}packets/{name}");
        self.gets
            .lock()
            .unwrap()
            .iter()
            .filter(|segment| segment.url == url)
            .count()
    }
}

impl Transport for Fixtures {
    fn get(&self, segment: &SegmentLocation, _size_hint: Option<u64>) -> Result<Bytes, Error> {
        self.gets.lock().unwrap().push(segment.clone());
        let path = segment.url.strip_prefix(BASE).expect("fixture URL");
        let body = Bytes::from(fixture(path));
        Ok(match &segment.range {
            Some(range) => body.slice(*range.start() as usize..=*range.end() as usize),
            None => body,
        })
    }
}

/// The representation in `tests/files/packets/<name>.mpd`, read through `transport`.
pub fn segment_stream(name: &str, transport: Arc<Fixtures>) -> SegmentStream {
    segment_stream_through(name, transport)
}

/// The manifest `tests/files/packets/<name>.mpd`, at the URL [`Fixtures`] serves it from.
pub fn manifest(name: &str) -> DashManifest {
    let xml = String::from_utf8(fixture(&format!("packets/{name}.mpd"))).unwrap();
    DashManifest::new(xml)
        .unwrap()
        .with_base_url(&format!("{BASE}packets/{name}.mpd"))
        .unwrap()
}

/// The representation in `tests/files/packets/<name>.mpd`, read through any `transport`.
pub fn segment_stream_through(name: &str, transport: Arc<dyn Transport>) -> SegmentStream {
    let manifest = manifest(name);
    let id = manifest.representations().next().unwrap().id.to_owned();
    manifest
        .segment_stream_with_cache(id, Arc::new(SegmentCache::new(transport)))
        .unwrap()
}

#[cfg(feature = "async")]
impl playdash::nonblocking::segment::Transport for Fixtures {
    fn get<'a>(
        &'a self,
        segment: &'a SegmentLocation,
        size_hint: Option<u64>,
    ) -> futures_util::future::BoxFuture<'a, Result<Bytes, Error>> {
        Box::pin(async move { Transport::get(self, segment, size_hint) })
    }
}

/// The representation in `tests/files/packets/<name>.mpd`, read by the async API through
/// fresh [`Fixtures`].
#[cfg(feature = "async")]
pub async fn async_segment_stream(name: &str) -> playdash::nonblocking::segment::SegmentStream {
    use playdash::nonblocking::DashManifest;
    use playdash::nonblocking::segment::SegmentCache;

    let manifest = DashManifest::from(manifest(name));
    let id = manifest.representations().next().unwrap().id.to_owned();
    let cache = Arc::new(SegmentCache::new(Arc::new(Fixtures::default())));
    manifest.segment_stream_with_cache(id, cache).await.unwrap()
}

/// The segments of `file`, a single fragmented MP4 file at `url`: the `ftyp`+`moov`
/// byte range, then one media segment per `moof`+`mdat`.
pub fn split_segments(url: &str, file: &[u8]) -> Segments {
    let mut ranges = Vec::new();
    let (mut start, mut pos) = (0, 0);
    while pos < file.len() {
        let size = u32::from_be_bytes(file[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &file[pos + 4..pos + 8];
        let end = pos + size;
        if kind == b"moov" || kind == b"mdat" {
            ranges.push(start as u64..=end as u64 - 1);
            start = end;
        }
        pos = end;
    }
    let mut segments = ranges
        .into_iter()
        .map(|range| SegmentLocation::with_range(url, range));
    let init = segments.next().expect("a moov");
    Segments::new(init, segments)
}

/// The segments of `tests/files/packets/muxed.mp4`: H.264 and AAC in one representation.
pub fn muxed() -> Segments {
    let url = format!("{BASE}packets/muxed.mp4");
    split_segments(&url, &fixture("packets/muxed.mp4"))
}

/// One packet as ffprobe lists it.
#[derive(Debug, Deserialize)]
pub struct Probed {
    pub pts: i64,
    pub dts: i64,
    /// Absent for some video packets.
    pub duration: Option<u64>,
    pub size: String,
    pub flags: String,
    pub data_hash: String,
    #[serde(default)]
    pub side_data_list: Vec<SideData>,
}

#[derive(Debug, Deserialize)]
pub struct SideData {
    pub skip_samples: Option<u64>,
}

/// ffprobe's JSON output: `-show_data_hash CRC32 -of json -show_entries
/// packet=pts,dts,duration,size,flags,data_hash:packet_side_data=skip_samples`.
#[derive(Debug, Deserialize)]
pub struct Probe {
    pub packets: Vec<Probed>,
}

/// The standard (zlib) CRC-32 of `data`, as ffprobe's `CRC32` data hash.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Checks `packet` is the `index`th of `all`, the packets of `name`.
pub fn assert_matches(name: &str, index: usize, packet: &Packet, all: &[Probed], audio: bool) {
    let expected = &all[index];
    let context = format!("{name} packet {index}");
    assert_eq!(packet.pts, expected.pts, "{context} pts");
    assert_eq!(packet.dts, expected.dts, "{context} dts");
    match expected.duration {
        // The container may record a padded last audio frame as shorter than the
        // frame ffprobe's parser decodes.
        Some(duration) if audio && index == all.len() - 1 => {
            assert!(packet.duration <= duration, "{context} duration");
        }
        Some(duration) => assert_eq!(packet.duration, duration, "{context} duration"),
        None => {}
    }
    assert_eq!(
        packet.data.len().to_string(),
        expected.size,
        "{context} size"
    );
    assert_eq!(
        format!("CRC32:{:08x}", crc32(&packet.data)),
        expected.data_hash,
        "{context} data"
    );
    assert_eq!(
        packet.keyframe,
        expected.flags.starts_with('K'),
        "{context} keyframe"
    );

    // Audio before time zero is priming.
    let priming = match audio {
        true => expected.pts.min(0).unsigned_abs().min(packet.duration),
        false => 0,
    };
    assert_eq!(packet.trim_start, priming, "{context} trim_start");
    assert_eq!(packet.trim_end, 0, "{context} trim_end");
}

/// Checks the priming trimmed from `packets` totals what ffprobe marks to skip, where it
/// does: all on the first packet, rather than per packet.
pub fn assert_skips(name: &str, packets: &[Packet], expected: &[Probed]) {
    let skip: u64 = expected
        .iter()
        .flat_map(|packet| &packet.side_data_list)
        .filter_map(|data| data.skip_samples)
        .sum();
    if skip > 0 {
        let trimmed: u64 = packets.iter().map(|packet| packet.trim_start).sum();
        assert_eq!(trimmed, skip, "{name} skip_samples");
    }
}

/// Checks `packets` are every packet of `expected`, ffprobe's packets of `name`.
pub fn check_packets(name: &str, packets: &[Packet], expected: &[Probed], audio: bool) {
    assert_eq!(packets.len(), expected.len(), "{name} packet count");
    for (index, packet) in packets.iter().enumerate() {
        assert_matches(name, index, packet, expected, audio);
    }
    assert_skips(name, packets, expected);
}
