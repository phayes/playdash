# playdash

Parse MPEG-DASH manifests for progressive, seekable streaming playback.

This crate lets you play MPEG-DASH streams with seeking.

## Installation

```toml
[dependencies]
playdash = "0.2"
```

## Features

| Feature | Default | What it adds |
|---|---|---|
| `sync` | yes | The blocking API: `FragmentStream`, `FragmentCache` and `Transport`, fetching through ureq |
| `async` | no | `playdash::nonblocking`: the same API, async, fetching through reqwest on tokio |
| `rodio` | no | `DashSource`, a ready-to-play rodio source (implies `sync`) |
| `encryption` | no | Common Encryption (`cenc`, `cbcs`) decryption with keys you supply |
| `log` | no | Logging through the `log` crate |

## Streaming

`DashManifest::stream` returns a `FragmentStream`, which downloads the representation in the
background. It is an `Iterator` of whole fragments, and it implements `std::io::Read` and
`std::io::Seek` over the same bytes, so it can feed an MP4 demuxer or playback system. The two
share one position: after a partial `read`, `next` yields the rest of that fragment.

```rust,ignore
use std::io::Read;
use std::time::Duration;
use playdash::DashManifest;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;

// Whole fragments: the init segment, then every media segment.
for fragment in manifest.stream("FLAC")? {
    let bytes = fragment?;
}

// Bytes, for a demuxer.
let mut stream = manifest.stream("FLAC")?;
let mut mpeg_buffer = [0u8; 4096];
stream.read(&mut mpeg_buffer)?;

// Seek to a specific timestamp. Lands at the start of the containing fragment.
stream.seek_time_coarse(Duration::from_secs(90))?;
stream.read(&mut mpeg_buffer)?;
```

**N.B.** `Read` and `Iterator` both define `take`, `chain` and `by_ref`, so with
`std::io::Read` in scope, `stream.take(3)` is ambiguous and will not compile. Call those
through the trait you mean: `Iterator::take(stream, 3)` for fragments, `Read::take(stream, 1024)`
for bytes.

## Async streaming

The `async` feature adds `playdash::nonblocking`, the same API for applications on a tokio
runtime: `DashManifest`, `FragmentStream`, `FragmentCache` and `Transport`. Constructors and
seeks are awaited, and a `FragmentStream` is a `futures` `Stream` of whole fragments and a
tokio `AsyncRead` and `AsyncSeek` over the same bytes, sharing one position. Fetches go through
a reqwest client of its own, or yours with `with_client`.

```toml
[dependencies]
playdash = { version = "0.2", default-features = false, features = ["async"] }
```

```rust,ignore
use std::time::Duration;
use futures_util::StreamExt;
use playdash::nonblocking::DashManifest;
use tokio::io::AsyncReadExt;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;

// Whole fragments: the init segment, then every media segment.
let mut stream = manifest.stream("FLAC").await?;
while let Some(fragment) = stream.next().await {
    let bytes = fragment?;
}

// Bytes, from 90 seconds in.
let mut stream = manifest.stream("FLAC").await?;
stream.seek_time_coarse(Duration::from_secs(90)).await?;
let mut buffer = [0u8; 4096];
stream.read(&mut buffer).await?;

// A manifest you already hold, fetched through your own client.
let manifest = DashManifest::new(xml)?
    .with_client(client)
    .with_base_url("https://media.example/stream.mpd")?;
```

**N.B.** `StreamExt` and `AsyncReadExt` both define `take` and `chain`, so with both in scope
call those through the trait you mean, as in `StreamExt::take(&mut stream, 3)`.

## Retention

By default a `FragmentStream` keeps the whole representation and downloads to the end, which
suits audio. For long video, `with_retention` bounds both, in media time around the position:
fragments further behind are dropped from the cache, and downloading stops once it is far
enough ahead, resuming as playback moves. Seeking back to a dropped fragment downloads it
again. A fragment another stream sharing the cache still keeps is never dropped.

```rust,ignore
use std::time::Duration;
use playdash::Retention;

let stream = manifest.stream("1080p")?.with_retention(Retention {
    behind: Some(Duration::from_secs(30)),
    ahead: Some(Duration::from_secs(60)),
});
```

## Rodio playback

The `rodio` feature adds `DashSource`, a ready-to-use rodio source for rodio and symphonia.

```toml
[dependencies]
playdash = { version = "0.2", features = ["rodio"] }
rodio = { version = "0.22", default-features = false, features = ["playback"] }
```

```rust,ignore
use rodio::{DeviceSinkBuilder, Player};
use playdash::{DashManifest, DashSource};

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let source = DashSource::new(&manifest, "FLAC")?;

let device = DeviceSinkBuilder::open_default_sink()?;
let player = Player::connect_new(device.mixer());
player.append(source);
player.sleep_until_end();
```

## Buffer progress

`FragmentStream::buffer` returns a handle that reports how much of the stream its cache holds, in fragments, bytes and media time. `DashSource::buffer` reports the same for a rodio source. 

```rust,ignore
use playdash::{DashManifest, DashSource};

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let source = DashSource::new(&manifest, "FLAC")?;

// Called after each fragment downloads or has its size learned.
source.buffer().on_change(|status| {
    println!(
        "{:.0}% downloaded, {} of {} fragments, in ranges {:?}",
        status.downloaded_percent(),
        status.downloaded.fragments,
        status.total.fragments,
        status.ranges,
    );
});
```

## Encrypted streams

The `encryption` feature decrypts ISO Common Encryption (`cenc` and `cbcs`) in pure Rust, given
content keys you already hold. It does not talk to a DRM licence server or CDM.

```toml
[dependencies]
playdash = { version = "0.2", features = ["encryption", "rodio"] }
```

```rust,ignore
use playdash::{ContentKeys, DashManifest, DashSource};

let mut keys = ContentKeys::new();
// KID and key as 32 hex digits; a UUID-form KID from `cenc:default_KID` also works.
keys.insert_hex("0123456789abcdef0123456789abcdef", "00112233445566778899aabbccddeeff")?;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let source = DashSource::new_with_keys(&manifest, "FLAC", keys)?;
```

## Limitations

1. Hierarchical `sidx` indexes are not supported.

2. Multi-period playback and live (`type="dynamic"`) manifests are not yet supported.

3. DRM systems such as Widevine and FairPlay are not supported. The encryption feature accepts
   content keys supplied by the caller; it does not acquire licenses. Widevine support is planned.

## Example audio player

See the [example audio player source on GitHub](https://github.com/phayes/playdash/blob/master/examples/audio_player.rs).

Example Usage:

```bash
# Big Buck Bunny, HE-AAC, SegmentTemplate with $Number$
cargo run --example audio_player --features rodio -- --id bbb_a64k https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd

# Envivio, AAC-LC, 48 kHz, SegmentTemplate with $Number$
cargo run --example audio_player --features rodio -- --id v4_258 https://dash.akamaized.net/envivio/EnvivioDash3/manifest.mpd

# Shaka Player's "Angel One", AAC-LC, SegmentBase (sidx index)
cargo run --example audio_player --features rodio -- --id 4 https://storage.googleapis.com/shaka-demo-assets/angel-one/dash.mpd

# DASH-IF test vector, Elephants Dream, HE-AAC, SegmentBase; audio is listed first
cargo run --example audio_player --features rodio -- https://dash.akamaized.net/dash264/TestCases/1a/netflix/exMPD_BIP_TC1.mpd
```

## Example video player

See the [example video player source on GitHub](https://github.com/phayes/playdash/blob/master/examples/video_player.rs).
It plays video in a window, synced to audio from rodio, and needs `ffmpeg` on `PATH` to decode
the video.

Example Usage:

```bash
# Big Buck Bunny, H.264 and HE-AAC, SegmentTemplate with $Number$
cargo run --release --example video_player --features rodio -- --id bbb_30fps_1280x720_4000k https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd

# Shaka Player's "Angel One", H.264 and AAC-LC, SegmentBase (sidx index)
cargo run --release --example video_player --features rodio -- https://storage.googleapis.com/shaka-demo-assets/angel-one/dash.mpd
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/phayes/playdash/blob/master/LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/phayes/playdash/blob/master/LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.
