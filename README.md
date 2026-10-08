# playdash

Parse MPEG-DASH manifests and set them up for progressive streaming playback.

This crate lets you play MPEG-DASH streams with seeking.

## Installation

```toml
[dependencies]
playdash = "0.1"
```

## Streaming

`DashManifest::stream` returns an `MpegStreamReader` that implements `std::io::Read` and
`std::io::Seek`, so you can hand it to any MP4 demuxer or playback system.

```rust,ignore
use std::io::Read;
use std::time::Duration;
use playdash::DashManifest;

let manifest = DashManifest::new(dash_xml)?;
let mut reader = manifest.stream("FLAC", true)?;

let mut mpeg_buffer = [0u8; 4096];
reader.read(&mut mpeg_buffer)?;

// Seek to a specific timestamp. Lands at the start of the containing fragment.
reader.seek_time_coarse(Duration::from_secs(90))?;
reader.read(&mut mpeg_buffer)?;
```

## Rodio playback

The `rodio` feature adds `DashSource`, a ready-to-use rodio source for rodio and symphonia.

```toml
[dependencies]
playdash = { version = "0.1", features = ["rodio"] }
rodio = { version = "0.22", default-features = false, features = ["playback"] }
```

```rust,ignore
use rodio::{DeviceSinkBuilder, Player};
use playdash::{DashManifest, DashSource};

let manifest = DashManifest::new(dash_xml)?;
let source = DashSource::new(&manifest, "FLAC")?;

let device = DeviceSinkBuilder::open_default_sink()?;
let player = Player::connect_new(device.mixer());
player.append(source);
player.sleep_until_end();
```

## Encrypted streams

The `encryption` feature decrypts ISO Common Encryption (`cenc` and `cbcs`) in pure Rust, given
content keys you already hold. It does not talk to a DRM licence server or CDM.

```toml
[dependencies]
playdash = { version = "0.1", features = ["encryption", "rodio"] }
```

```rust,ignore
use playdash::{ContentKeys, DashManifest, DashSource};

let mut keys = ContentKeys::new();
// KID and key as 32 hex digits; a UUID-form KID from `cenc:default_KID` also works.
keys.insert_hex("0123456789abcdef0123456789abcdef", "00112233445566778899aabbccddeeff")?;

let manifest = DashManifest::new(dash_xml)?;
let source = DashSource::new_with_keys(&manifest, "FLAC", keys)?;
```

## Limitations:

1.  Hierarchical `sidx` indexes are not supported.

2. Multi-period playback and live (`type="dynamic"`) manifests are not yet supported. 

3. Widevine and fairplay are not yet supported. (Support is planned).

## Example player

```bash
# Big Buck Bunny, HE-AAC, SegmentTemplate with $Number$
cargo run --example player -- --id bbb_a64k https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd

Envivio, AAC-LC, 48 kHz, SegmentTemplate with $Number$
cargo run --example player -- --id v4_258 https://dash.akamaized.net/envivio/EnvivioDash3/manifest.mpd

# Shaka Player's "Angel One", AAC-LC, SegmentBase (sidx index)
cargo run --example player -- --id 4 https://storage.googleapis.com/shaka-demo-assets/angel-one/dash.mpd

# DASH-IF test vector, Elephants Dream, HE-AAC, SegmentBase; audio is listed first
cargo run --example player -- https://dash.akamaized.net/dash264/TestCases/1a/netflix/exMPD_BIP_TC1.mpd
```