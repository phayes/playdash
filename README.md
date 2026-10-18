# playdash

`playdash` reads MPEG-DASH streams and turns them into compressed audio or video packets that
you can send to a decoder. It downloads media as playback advances and supports seeking without
first saving the whole file.

MPEG-DASH is a streaming format. A small XML file, called a *manifest* or MPD, describes the
available audio and video variants and where to download their short media segments. `playdash`
handles the manifest, segment downloads, fragmented MP4 parsing, and decoder input conversion.
It does not decode or display the media; you choose the decoder and playback system.

The main API is [`MediaStream`](https://docs.rs/playdash/latest/playdash/struct.MediaStream.html).
Use it when building a player, transcoder, analyzer, or media-processing pipeline.

## Installation

```toml
[dependencies]
playdash = "0.2"
```

The default `sync` feature provides the blocking API shown below.

## Quick start

Load a manifest, choose one of its media variants, and open it as a
[`MediaStream`](https://docs.rs/playdash/latest/playdash/struct.MediaStream.html):

```rust,no_run
use playdash::{ContentKind, DashManifest};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;

let video = manifest
    .highest_bandwidth(ContentKind::Video)
    .ok_or("the manifest has no video")?;

let input = manifest.media_stream(video.id).open()?;

// Configure your decoder once before reading packets.
let initialization = input.initialization();
configure_decoder(initialization);

// Packets are compressed frames in decode order.
for packet in input {
    let packet = packet?;
    decode(&packet.data, packet.pts, packet.duration);
}
# Ok(())
# }
# fn configure_decoder(_: &playdash::DecoderInitialization) {}
# fn decode(_: &[u8], _: i64, _: u64) {}
```

A DASH manifest can offer several *representations* of the same content, such as video at
different resolutions or audio at different bitrates. Each representation has an ID. You can
select one directly:

```rust,no_run
# use playdash::DashManifest;
# fn main() -> Result<(), playdash::Error> {
# let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let input = manifest.media_stream("video-1080p").open()?;
# let _ = input;
# Ok(())
# }
```

Use `manifest.representations()` to inspect every choice, or
`highest_bandwidth`/`lowest_bandwidth` to choose by media type and advertised bitrate.

## Connecting a decoder

[`MediaStream`](https://docs.rs/playdash/latest/playdash/struct.MediaStream.html) produces
compressed packets; it does not decompress them. Before sending packets
to your decoder, configure it with `input.initialization()`. This value includes:

- the packet format produced by the stream;
- codec setup bytes, if the codec needs them;
- useful audio or video parameters such as sample rate, channel layout, or dimensions.

By default, `playdash` chooses an elementary-stream format suitable for many software decoders.
Some platform APIs expect packets and configuration in the form used by MP4 instead. Select that
form with
[`DecoderStyle::ConfigRecord`](https://docs.rs/playdash/latest/playdash/enum.DecoderStyle.html#variant.ConfigRecord):

```rust,no_run
# use playdash::{DashManifest, DecoderStyle};
# fn main() -> Result<(), playdash::Error> {
# let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let input = manifest
    .media_stream("video")
    .style(DecoderStyle::ConfigRecord)
    .open()?;
# let _ = input;
# Ok(())
# }
```

See [`DecoderStyle`](https://docs.rs/playdash/latest/playdash/enum.DecoderStyle.html) for a decoder
compatibility guide. If you already know the exact input format your decoder needs, choose it
with
[`MediaStreamBuilder::format`](https://docs.rs/playdash/latest/playdash/struct.MediaStreamBuilder.html#method.format).

## Seeking

Seeking may need packets from just before the requested time so that inter-frame video or audio
codec state can be reconstructed. `seek_time` returns those packets as `seeked.preroll`.
Decode them first and discard their output. Then continue with the stream:

```rust,no_run
use std::time::Duration;
use playdash::DashManifest;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let mut input = manifest.media_stream("video").open()?;
let seeked = input.seek_time(Duration::from_secs(90))?;

reset_decoder();
configure_decoder(input.initialization());

for packet in &seeked.preroll {
    decode_and_discard(&packet.data);
}

for packet in input {
    let packet = packet?;
    for frame in decode(&packet.data) {
        // A decoded frame may still be earlier than the requested time.
        if !seeked.discards(frame.pts, frame.duration) {
            display(frame);
        }
    }
}
# Ok(())
# }
# fn reset_decoder() {}
# fn configure_decoder(_: &playdash::DecoderInitialization) {}
# fn decode_and_discard(_: &[u8]) {}
# struct Frame { pts: i64, duration: u64 }
# fn decode(_: &[u8]) -> Vec<Frame> { vec![] }
# fn display(_: Frame) {}
```

For audio, `seeked.trim_start_of(&packet)` reports how many timestamp ticks to remove from the
start of a decoded packet. `seeked.landed_time()` reports the actual landing time.

To open and seek in one operation, use `open_at`. Use `seek_keyframe` or `open_at_keyframe` when
you want the previous keyframe and will decide yourself how much decoded output to discard.

## Controlling downloads and memory

By default, a stream downloads ahead and keeps every downloaded segment. For long media, set a
retention window. Downloads pause at the forward limit, and old segments are released after the
backward limit:

```rust,no_run
use std::time::Duration;
use playdash::{DashManifest, Retention};

# fn main() -> Result<(), playdash::Error> {
# let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let input = manifest
    .media_stream("video")
    .retention(Retention {
        behind: Some(Duration::from_secs(30)),
        ahead: Some(Duration::from_secs(60)),
    })
    .open()?;
# let _ = input;
# Ok(())
# }
```

`input.buffer()` returns a handle for observing download progress. Supply a shared
[`SegmentCache`](https://docs.rs/playdash/latest/playdash/struct.SegmentCache.html) with
[`MediaStreamBuilder::cache`](https://docs.rs/playdash/latest/playdash/struct.MediaStreamBuilder.html#method.cache)
when several streams should reuse connections and downloaded
segments, or when you need a custom transport.

## Async API

Enable `async` for an equivalent API built on Tokio, reqwest, and `futures_util::Stream`:

```toml
[dependencies]
playdash = { version = "0.2", default-features = false, features = ["async"] }
```

```rust,no_run
use futures_util::StreamExt;
use playdash::nonblocking::DashManifest;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
let mut input = manifest.media_stream("audio").open().await?;
configure_decoder(input.initialization());

while let Some(packet) = input.next().await {
    decode(&packet?.data);
}
# Ok(())
# }
# fn configure_decoder(_: &playdash::DecoderInitialization) {}
# fn decode(_: &[u8]) {}
```

The async types live in `playdash::nonblocking`. Their methods and builder options match the
blocking API closely.

## Lower-level APIs

Most applications should use
[`MediaStream`](https://docs.rs/playdash/latest/playdash/struct.MediaStream.html). Two lower-level
layers are available when you need
more control:

- [`PacketStream`](https://docs.rs/playdash/latest/playdash/packet/struct.PacketStream.html)
  demultiplexes fragmented MP4 into packets exactly as MP4 stores them. Use it when your pipeline
  needs original MP4 samples or when you want to apply codec conversion yourself.
- [`SegmentStream`](https://docs.rs/playdash/latest/playdash/segment/struct.SegmentStream.html)
  downloads initialization and media segments. It can yield complete segments or act as a
  seekable byte reader. Use it when you already have an MP4 demuxer or need segment-level access.

The async equivalents are under
[`playdash::nonblocking`](https://docs.rs/playdash/latest/playdash/nonblocking/).

## Rodio playback

The `rodio` feature provides
[`DashSource`](https://docs.rs/playdash/latest/playdash/struct.DashSource.html), the shortest path
to audio playback. It downloads,
demultiplexes, and decodes through rodio/Symphonia:

```toml
[dependencies]
playdash = { version = "0.2", features = ["rodio"] }
rodio = { version = "0.22", default-features = false, features = ["playback"] }
```

```rust,no_run
use playdash::{DashManifest, DashSource};
use rodio::{DeviceSinkBuilder, Player};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let source = DashSource::new(&manifest, "audio")?;

let device = DeviceSinkBuilder::open_default_sink()?;
let player = Player::connect_new(device.mixer());
player.append(source);
player.sleep_until_end();
# Ok(())
# }
```

## Encrypted media

The `encryption` feature decrypts the `cenc` and `cbcs` forms of ISO Common Encryption when you
provide the content keys. It does not contact a DRM license server and does not integrate with a
content decryption module.

```rust,no_run
use playdash::{ContentKeys, DashManifest};

# fn main() -> Result<(), playdash::Error> {
let mut keys = ContentKeys::new();
keys.insert_hex(
    "0123456789abcdef0123456789abcdef", // key ID
    "00112233445566778899aabbccddeeff", // content key
)?;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let input = manifest.media_stream("video").keys(keys).open()?;
# let _ = input;
# Ok(())
# }
```

## Cargo features

- `sync` (default): blocking
  [`MediaStream`](https://docs.rs/playdash/latest/playdash/struct.MediaStream.html) and lower-level
  APIs, with HTTP through ureq.
- `async`: async APIs under `playdash::nonblocking`, with HTTP through reqwest.
- `rodio`: ready-to-play audio through rodio and Symphonia; enables `sync`.
- `encryption`: `cenc` and `cbcs` decryption with caller-provided keys.
- `log`: internal logging through the `log` crate.

## Examples

The repository includes small programs for common tasks:

```sh
# Show the variants offered by a manifest.
cargo run --example list_representations -- MANIFEST_URL

# Inspect the lower-level packets stored in MP4.
cargo run --example list_packets -- MANIFEST_URL

# Save a FLAC representation as a playable file.
cargo run --example save_flac -- MANIFEST_URL output.flac

# Play audio through rodio.
cargo run --example audio_player --features rodio -- MANIFEST_URL

# Play video and audio; ffmpeg must be on PATH.
cargo run --release --example video_player --features rodio -- MANIFEST_URL
```

## Current limitations

- Only static, single-period playback is supported. Live (`type="dynamic"`) manifests and
  seamless multi-period playback are not yet supported.
- Hierarchical `sidx` indexes are not supported.
- Open-GOP random access points and gradual decoding refresh are not supported.
- A stream cannot change codecs partway through playback.
- DRM license acquisition is outside the crate. The encryption feature only uses keys supplied
  by the caller.

`playdash` is tested with the MP4 cases in GPAC's
[Telecom ParisTech DASH conformance set](https://download.tsi.telecom-paristech.fr/gpac/DASH_CONFORMANCE/TelecomParisTech/).

## License

Licensed under either the [Apache License 2.0](LICENSE-APACHE) or the
[MIT License](LICENSE-MIT), at your option.
