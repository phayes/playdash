# tidal_dash

Parse TIDAL MPEG-DASH manifests and set them up for progressive streaming playback.

This crate lets you play TIDAL MPEG-DASH streams with seeking.

## Installation

```toml
[dependencies]
tidal_dash = "0.1"
rodio = { version = "0.22", default-features = false, features = ["playback"] }
```

`DashSource` is included by default: a ready-to-use rodio source for rodio and symphonia.
It supports unprotected streams; DRM-protected `cenc` and `cbcs` streams are not yet supported.

## Rodio playback

```rust,ignore
use rodio::{DeviceSinkBuilder, Player};
use tidal_dash::{DashManifest, DashSource};

let manifest = DashManifest::new(dash_xml)?;
let source = DashSource::new(&manifest, "FLAC")?;

let device = DeviceSinkBuilder::open_default_sink()?;
let player = Player::connect_new(device.mixer());
player.append(source);
player.sleep_until_end();
```

## Lower-level API

For another playback system, `DashManifest::stream` returns an `MpegStreamReader`
that implements `std::io::Read` and `std::io::Seek` over the concatenated
initialization segment and media fragments.

Skip the rodio integration with `default-features = false`:

```toml
[dependencies]
tidal_dash = { version = "0.1", default-features = false }
```

```rust,ignore
use std::io::Read;
use std::time::Duration;
use tidal_dash::DashManifest;

let manifest = DashManifest::new(dash_xml)?;
let mut reader = manifest.stream("FLAC", true)?;

let mut mpeg_buffer = [0u8; 4096];
reader.read(&mut mpeg_buffer)?;

// Seek to a specific timestamp. Lands at the start of the containing fragment.
reader.seek_time_coarse(Duration::from_secs(90))?;
reader.read(&mut mpeg_buffer)?;
```

If you create several readers for the same track, for example rebuilding one after a seek,
`DashManifest::stream_with_cache` with a shared `FragmentCache` reuses pooled connections
and never downloads a fragment twice, even while another reader's download is in flight.
`DashSource` does this internally; use `DashSource::new_with_cache` to share a cache or to
supply your own `Transport` through `FragmentCache::new`.

## Caveats

`SegmentBase` and multi-period playback and live (`type="dynamic"`) manifests are not yet supported. Contributions
to improve this are very welcome.

## Example player

`examples/player` uses `DashSource`:

```text
cargo run --example player -- manifest.xml
```

The manifest argument is an `http(s)` URL, MPEG-DASH XML, a `data:` URL, a file
path, or omitted to read stdin. Arrow keys jump 1 second or 1 minute.

## Contributing

Contributions are welcome.  I would be particularily grateful for contributions that:

1. Add support for encrypted stream
