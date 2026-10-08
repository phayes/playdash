//! Decrypts real `cenc` and `cbcs` fixtures from ffmpeg and shaka-packager
//! and checks they decode to the same samples as the unencrypted source.
#![cfg(all(feature = "encryption", feature = "rodio"))]

use rodio::{Decoder, Source};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use playdash::{
    ContentKeys, DashManifest, DashSource, Error, Fragment, FragmentCache, MpegStreamReader,
    Transport,
};

const KID: &str = "0123456789abcdef0123456789abcdef";
const KEY: &str = "00112233445566778899aabbccddeeff";
const BASE: &str = "https://cdn.test/encryption/";

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_files/encryption");
    std::fs::read(path.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// Serves fixtures by URL file name, honoring byte ranges.
struct Fixtures;

impl Transport for Fixtures {
    fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<Arc<[u8]>, Error> {
        let name = fragment.url.strip_prefix(BASE).expect("fixture URL");
        let body = fixture(name);
        Ok(match &fragment.range {
            Some(range) => body[*range.start() as usize..=*range.end() as usize].into(),
            None => body.into(),
        })
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

/// Byte ranges of `ftyp`+`moov`, then each `moof`+`mdat`, in a single
/// fragmented MP4 file.
fn split_fragments(file: &[u8]) -> Vec<std::ops::RangeInclusive<u64>> {
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
    ranges
}

fn ranged_reader(name: &str, cache: Arc<FragmentCache>) -> MpegStreamReader {
    let fragments = split_fragments(&fixture(name))
        .into_iter()
        .map(|range| Fragment::with_range(format!("{BASE}{name}"), range))
        .collect();
    MpegStreamReader::new_with_cache(fragments, None, false, cache).unwrap()
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
fn cache_holds_same_length_plaintext() {
    let cache = keyed_cache();
    let manifest = manifest("shaka_cenc.mpd");
    let source = DashSource::new_with_cache(&manifest, "0", cache.clone()).unwrap();
    assert!(source.count() > 0);

    for fragment in manifest.fragments("0").unwrap() {
        let raw = Fixtures.get(&fragment, None).unwrap();
        let cached = cache.cached(&fragment).expect("fetched during playback");
        assert_eq!(cached.len(), raw.len(), "{fragment}");
        assert_ne!(cached, raw, "{fragment} was not decrypted");
    }
    let init = cache.cached(&manifest.fragments("0").unwrap()[0]).unwrap();
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
