# playdash

Parse MPEG-DASH manifests for progressive, seekable streaming playback.

This crate lets you play MPEG-DASH streams with seeking.

## Installation

```toml
[dependencies]
playdash = "0.1"
```

## Streaming

`DashManifest::stream` returns a `StreamReader` that implements `std::io::Read` and
`std::io::Seek`, so it can feed an MP4 demuxer or playback system.

```rust,no_run
use std::io::Read;
use std::time::Duration;
use playdash::DashManifest;

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let mut reader = manifest.stream("FLAC", true)?;

let mut mpeg_buffer = [0u8; 4096];
reader.read(&mut mpeg_buffer)?;

// Seek to a specific timestamp. Lands at the start of the containing fragment.
reader.seek_time_coarse(Duration::from_secs(90))?;
reader.read(&mut mpeg_buffer)?;
# Ok(())
# }
```

## Rodio playback

The `rodio` feature adds `DashSource`, a ready-to-use rodio source for rodio and symphonia.

```toml
[dependencies]
playdash = { version = "0.1", features = ["rodio"] }
rodio = { version = "0.22", default-features = false, features = ["playback"] }
```

```rust,no_run
use rodio::{DeviceSinkBuilder, Player};
use playdash::{DashManifest, DashSource};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let source = DashSource::new(&manifest, "FLAC")?;

let device = DeviceSinkBuilder::open_default_sink()?;
let player = Player::connect_new(device.mixer());
player.append(source);
player.sleep_until_end();
# Ok(())
# }
```

## Buffer progress

`StreamReader::buffer` returns a handle that reports how much of the stream its cache holds, in fragments, bytes and media time. `DashSource::buffer` reports the same for a rodio source. 

```rust,no_run
use playdash::{DashManifest, DashSource};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
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
# Ok(())
# }
```

## Encrypted streams

The `encryption` feature decrypts ISO Common Encryption (`cenc` and `cbcs`) in pure Rust, given
content keys you already hold. It does not talk to a DRM licence server or CDM.

```toml
[dependencies]
playdash = { version = "0.1", features = ["encryption", "rodio"] }
```

```rust,no_run
use playdash::{ContentKeys, DashManifest, DashSource};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let mut keys = ContentKeys::new();
// KID and key as 32 hex digits; a UUID-form KID from `cenc:default_KID` also works.
keys.insert_hex("0123456789abcdef0123456789abcdef", "00112233445566778899aabbccddeeff")?;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let source = DashSource::new_with_keys(&manifest, "FLAC", keys)?;
# Ok(())
# }
```

## Limitations

1. Hierarchical `sidx` indexes are not supported.

2. Multi-period playback and live (`type="dynamic"`) manifests are not yet supported.

3. DRM systems such as Widevine and FairPlay are not supported. The encryption feature accepts
   content keys supplied by the caller; it does not acquire licenses. Widevine support is planned.

## Example player

See the [example player source on GitHub](https://github.com/phayes/playdash/blob/master/examples/player.rs).

Example Usage:

```bash
# Big Buck Bunny, HE-AAC, SegmentTemplate with $Number$
cargo run --example player -- --id bbb_a64k https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd

# Envivio, AAC-LC, 48 kHz, SegmentTemplate with $Number$
cargo run --example player -- --id v4_258 https://dash.akamaized.net/envivio/EnvivioDash3/manifest.mpd

# Shaka Player's "Angel One", AAC-LC, SegmentBase (sidx index)
cargo run --example player -- --id 4 https://storage.googleapis.com/shaka-demo-assets/angel-one/dash.mpd

# DASH-IF test vector, Elephants Dream, HE-AAC, SegmentBase; audio is listed first
cargo run --example player -- https://dash.akamaized.net/dash264/TestCases/1a/netflix/exMPD_BIP_TC1.mpd
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/phayes/playdash/blob/master/LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/phayes/playdash/blob/master/LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
