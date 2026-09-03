# tidal_dash

Parse TIDAL MPEG-DASH manifests and set them up for progressive streaming playback.

This crate lets you play TIDAL MPEG-DASH streams with seeking. 

## Installation

```toml
[dependencies]
tidal_dash = { version = "0.1", features = ["rodio"] }
```

The optional `rodio` feature adds a pre-built ready-to-use rodio source `DashSource` for use with rodio and symphonia. 
It supports unprotected streams; DRM-protected `cenc` and `cbcs` streams are not yet supported.

## Basic Usage

`manifest.stream` returns an `MpegStreamReader` that implements `std::io::Read`
and `std::io::Seek` over the concatenated initialization segment and media
fragments. 

```rust
use std::io::Read;
use std::time::Duration;
use tidal_dash::DashManifest;

let manifest = DashManifest::new(dash_xml)?;
let mut reader = manifest.stream("FLAC")?;

let mut mpeg_buffer = [0u8; 4096];
reader.read(&mut mpeg_buffer)?;

// Seek to a specific timestamp. Lands at the start of the containing fragment.
reader.seek_time_coarse(Duration::from_secs(90))?;
reader.read(&mut mpeg_buffer)?;
```

# Rodio Playback

With the `rodio` feature, `DashSource` is a ready-to-play rodio source.

```toml
[dependencies]
rodio = { version = "0.22", default-features = false, features = ["playback"] }
```

```rust
use rodio::{DeviceSinkBuilder, Player};
use tidal_dash::{DashManifest, DashSource};

let manifest = DashManifest::new(dash_xml)?;
let source = DashSource::new(&manifest, "FLAC")?;

let device = DeviceSinkBuilder::open_default_sink()?;
let player = Player::connect_new(device.mixer());
player.append(source);
player.sleep_until_end();
```

## Caveats

This crate is currently built to parse TIDAL-style dash manifest files, and many other flavours of mpeg-dash are not well supported. Contributions to improve this are very welcome. 

## Example player

`examples/player` uses the feature-gated `DashSource` adapter:

```text
cargo run --features rodio --example player -- manifest.xml
```

The manifest argument is MPEG-DASH XML, a `data:` URL, a file path, or omitted
to read stdin. Arrow keys jump 1 second or 1 minute.

## Contributing

Contributions are welcome.  I would be particularily grateful for contributions that:

1. Replace the tidal-specific MPEG-DASH manifest parser with a pure-rust 3rd party crate dedicated to dash manifest parsing.
2. Add support for encrypted stream