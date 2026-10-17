//! Decrypts real `cenc` and `cbcs` fixtures from ffmpeg and shaka-packager
//! and checks they decode to the same samples as the unencrypted source.
#![cfg(all(feature = "encryption", feature = "rodio"))]

use playdash::{
    BufferStatus, Bytes, ContentKeys, DashManifest, DashSource, Error, Fragment, FragmentCache,
    FragmentStream, Fragments, Transport,
};
use rodio::{Decoder, Source};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const KID: &str = "0123456789abcdef0123456789abcdef";
const KEY: &str = "00112233445566778899aabbccddeeff";
const BASE: &str = "https://cdn.test/encryption/";

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/files/encryption");
    std::fs::read(path.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// Serves fixtures by URL file name, honoring byte ranges.
struct Fixtures;

impl Transport for Fixtures {
    fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<Bytes, Error> {
        let name = fragment.url.strip_prefix(BASE).expect("fixture URL");
        let body = Bytes::from(fixture(name));
        Ok(match &fragment.range {
            Some(range) => body.slice(*range.start() as usize..=*range.end() as usize),
            None => body,
        })
    }
}

/// Serves fixtures like [`Fixtures`], but holds back the last media fragment
/// until opened.
#[derive(Default)]
struct Gated {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gated {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

impl Transport for Gated {
    fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Bytes, Error> {
        if fragment.url.ends_with("_3.m4s") {
            let open = self.open.lock().unwrap();
            drop(self.opened.wait_while(open, |open| !*open).unwrap());
        }
        Fixtures.get(fragment, size_hint)
    }
}

/// Collects the statuses a buffer handle reports.
#[derive(Default)]
struct Reports {
    statuses: Mutex<Vec<BufferStatus>>,
    reported: Condvar,
}

impl Reports {
    fn push(&self, status: &BufferStatus) {
        self.statuses.lock().unwrap().push(status.clone());
        self.reported.notify_all();
    }

    /// The first report matching `ready`, waiting up to five seconds.
    fn wait_for(&self, ready: impl Fn(&BufferStatus) -> bool) -> BufferStatus {
        let statuses = self.statuses.lock().unwrap();
        let (statuses, _) = self
            .reported
            .wait_timeout_while(statuses, Duration::from_secs(5), |statuses| {
                !statuses.iter().any(&ready)
            })
            .unwrap();
        statuses
            .iter()
            .find(|status| ready(status))
            .expect("no matching buffer report")
            .clone()
    }
}

fn keys() -> ContentKeys {
    let mut keys = ContentKeys::new();
    keys.insert_hex(KID, KEY).unwrap();
    keys
}

fn keyed_cache() -> Arc<FragmentCache> {
    Arc::new(FragmentCache::new(Arc::new(Fixtures)).with_keys(keys()))
}

fn manifest(name: &str) -> DashManifest {
    let xml = String::from_utf8(fixture(name)).unwrap();
    DashManifest::new(xml)
        .unwrap()
        .with_base_url(&format!("{BASE}{name}"))
        .unwrap()
}

fn decode(reader: impl std::io::Read + std::io::Seek + Send + Sync + 'static) -> Vec<f32> {
    Decoder::builder()
        .with_data(reader)
        .with_mime_type("audio/mp4")
        .build()
        .unwrap()
        .collect()
}

/// Samples of the unencrypted encode every fixture was made from.
fn plain_samples() -> Vec<f32> {
    let samples = decode(Cursor::new(fixture("plain.mp4")));
    assert!(samples.len() > 30_000, "{} samples", samples.len());
    samples
}

/// The fragments of `file`, a single fragmented MP4 file at `url`: the
/// `ftyp`+`moov` byte range, then each `moof`+`mdat`.
fn split_fragments(url: &str, file: &[u8]) -> Fragments {
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
        if kind == b"mfra" {
            break;
        }
        pos = end;
    }
    let mut fragments = ranges
        .into_iter()
        .map(|range| Fragment::with_range(url, range));
    let init = fragments.next().expect("a moov");
    Fragments::new(init, fragments)
}

fn ranged_reader(name: &str, cache: Arc<FragmentCache>) -> FragmentStream {
    let fragments = split_fragments(&format!("{BASE}{name}"), &fixture(name));
    FragmentStream::new_with_cache(fragments, None, cache).unwrap()
}

#[test]
fn decrypts_shaka_cbcs_dash() {
    let source = DashSource::new_with_cache(&manifest("shaka_cbcs.mpd"), "0", keyed_cache());
    let samples: Vec<f32> = source.unwrap().collect();
    assert_eq!(samples, plain_samples());
}

#[test]
fn decrypts_shaka_cenc_dash() {
    let source = DashSource::new_with_cache(&manifest("shaka_cenc.mpd"), "0", keyed_cache());
    let samples: Vec<f32> = source.unwrap().collect();
    assert_eq!(samples, plain_samples());
}

#[test]
fn decrypts_ffmpeg_cenc_byte_ranges() {
    let reader = ranged_reader("ffmpeg_cenc.mp4", keyed_cache());
    assert_eq!(decode(reader), plain_samples());
}

#[test]
fn iterates_decrypted_fragments() {
    let manifest = manifest("shaka_cbcs.mpd");
    let fragments: Vec<_> = manifest
        .stream_with_cache("0", keyed_cache())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        fragments.len(),
        1 + manifest.fragments("0").unwrap().media.len()
    );
    let joined: Vec<u8> = fragments.iter().flatten().copied().collect();
    assert_eq!(decode(Cursor::new(joined)), plain_samples());
}

#[test]
fn keyed_cache_passes_unprotected_media_through() {
    let reader = ranged_reader("plain.mp4", keyed_cache());
    assert_eq!(decode(reader), plain_samples());
}

#[test]
fn seeks_within_decrypted_stream() {
    let manifest = manifest("shaka_cbcs.mpd");
    let mut source = DashSource::new_with_cache(&manifest, "0", keyed_cache()).unwrap();
    // Past the first fragment, so the decoder is rebuilt from the cache.
    source.try_seek(Duration::from_millis(700)).unwrap();
    let tail: Vec<f32> = source.collect();

    let plain = plain_samples();
    let expected_start = 700 * 22_050 / 1000;
    assert!(tail.len().abs_diff(plain.len() - expected_start) <= 1);
    assert_eq!(tail, plain[plain.len() - tail.len()..]);
}

#[test]
fn buffer_reports_downloads() {
    let gated = Arc::new(Gated::default());
    let cache = Arc::new(FragmentCache::new(gated.clone()).with_keys(keys()));
    let source = DashSource::new_with_cache(&manifest("shaka_cbcs.mpd"), "0", cache).unwrap();
    let buffer = source.buffer();
    let reports = Arc::new(Reports::default());
    buffer.on_change({
        let reports = reports.clone();
        move |status| reports.push(status)
    });

    // The last of three half-second fragments is held back.
    let status = buffer.status();
    assert_eq!(status.total.fragments, 3);
    assert_eq!(status.total.time, Duration::from_millis(1500));
    assert!(!status.is_complete());

    gated.open();
    let complete = reports.wait_for(BufferStatus::is_complete);
    assert_eq!(
        complete.ranges,
        [Duration::ZERO..Duration::from_millis(1500)]
    );
    assert_eq!(complete.total.bytes, Some(complete.downloaded.bytes));
    assert_eq!(
        complete.buffered_until(Duration::from_millis(1200)),
        Duration::from_millis(1500)
    );
}

#[test]
fn cache_holds_same_length_plaintext() {
    let cache = keyed_cache();
    let manifest = manifest("shaka_cenc.mpd");
    let source = DashSource::new_with_cache(&manifest, "0", cache.clone()).unwrap();
    assert!(source.count() > 0);

    let fragments = manifest.fragments("0").unwrap();
    for fragment in std::iter::once(&fragments.init).chain(&fragments.media) {
        let raw = Fixtures.get(fragment, None).unwrap();
        let cached = cache.cached(fragment).expect("fetched during playback");
        assert_eq!(cached.len(), raw.len(), "{fragment}");
        assert_ne!(cached, raw, "{fragment} was not decrypted");
    }
    let init = cache.cached(&fragments.init).unwrap();
    assert!(!init.windows(4).any(|window| window == b"enca"));
}

#[test]
fn missing_key_fails_before_playback() {
    let mut keys = ContentKeys::new();
    keys.insert_hex("ffffffffffffffffffffffffffffffff", KEY)
        .unwrap();
    let cache = Arc::new(FragmentCache::new(Arc::new(Fixtures)).with_keys(keys));
    match DashSource::new_with_cache(&manifest("shaka_cbcs.mpd"), "0", cache) {
        Err(Error::MissingContentKey(kid)) => assert_eq!(kid, KID),
        Err(error) => panic!("unexpected error: {error}"),
        Ok(_) => panic!("played without the content key"),
    }
}

#[test]
fn protected_dash_without_keys_is_rejected() {
    let cache = Arc::new(FragmentCache::new(Arc::new(Fixtures)));
    assert!(matches!(
        DashSource::new_with_cache(&manifest("shaka_cbcs.mpd"), "0", cache),
        Err(Error::RodioProtectedContent(scheme)) if scheme == "cbcs"
    ));
}

/// The same fixtures through the async `FragmentStream`, served over HTTP.
#[cfg(feature = "async")]
mod nonblocking {
    use super::*;
    use futures_util::StreamExt;
    use playdash::nonblocking::{FragmentCache, FragmentStream};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Serves the fixtures by file name on a local port, answering `Range`
    /// requests with 206. Returns the base URL.
    fn serve_fixtures() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let name = request_line.split(' ').nth(1).unwrap()[1..].to_owned();
                    let mut range = None;
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap() > 2 {
                        if let Some(value) = header
                            .to_ascii_lowercase()
                            .trim()
                            .strip_prefix("range: bytes=")
                        {
                            let (start, end) = value.split_once('-').unwrap();
                            range = Some(start.parse::<usize>().unwrap()..=end.parse().unwrap());
                        }
                        header.clear();
                    }
                    let file = fixture(&name);
                    let (status, body) = match range {
                        Some(range) => ("206 Partial Content", &file[range]),
                        None => ("200 OK", &file[..]),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let stream = reader.get_mut();
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(body).unwrap();
                });
            }
        });
        format!("http://{address}")
    }

    /// The DASH fixture `name`, served from `base`.
    fn async_manifest(base: &str, name: &str) -> playdash::nonblocking::DashManifest {
        let xml = String::from_utf8(fixture(name)).unwrap();
        playdash::nonblocking::DashManifest::new(xml)
            .unwrap()
            .with_base_url(&format!("{base}/{name}"))
            .unwrap()
    }

    /// Every item of `stream`, joined into one file.
    async fn joined(stream: FragmentStream) -> Vec<u8> {
        let items: Vec<_> = stream.collect().await;
        items.into_iter().flat_map(|item| item.unwrap()).collect()
    }

    fn keyed_cache(keys: ContentKeys) -> Arc<FragmentCache> {
        Arc::new(FragmentCache::default().with_keys(keys))
    }

    #[tokio::test]
    async fn stream_decrypts_shaka_dash() {
        let base = serve_fixtures();
        for name in ["shaka_cbcs.mpd", "shaka_cenc.mpd"] {
            let stream = async_manifest(&base, name)
                .stream_with_cache("0", keyed_cache(keys()))
                .await
                .unwrap();
            assert_eq!(
                decode(Cursor::new(joined(stream).await)),
                plain_samples(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn stream_decrypts_ffmpeg_cenc_byte_ranges() {
        let base = serve_fixtures();
        for name in ["ffmpeg_cenc.mp4", "plain.mp4"] {
            let fragments = split_fragments(&format!("{base}/{name}"), &fixture(name));
            let stream = FragmentStream::new_with_cache(fragments, None, keyed_cache(keys()))
                .await
                .unwrap();
            assert_eq!(
                decode(Cursor::new(joined(stream).await)),
                plain_samples(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn stream_without_the_key_fails_before_media() {
        let base = serve_fixtures();
        let mut keys = ContentKeys::new();
        keys.insert_hex("ffffffffffffffffffffffffffffffff", KEY)
            .unwrap();
        let opened = async_manifest(&base, "shaka_cbcs.mpd")
            .stream_with_cache("0", keyed_cache(keys))
            .await;
        assert!(matches!(opened, Err(Error::MissingContentKey(kid)) if kid == KID));
    }
}
