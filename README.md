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
| `sync` | yes | The blocking API: `MediaStream`, `fragment::FragmentStream`, `packet::PacketStream`, `FragmentCache` and `Transport`, fetching through ureq |
| `async` | no | `playdash::nonblocking`: the same API, async, fetching through reqwest on tokio |
| `rodio` | no | `DashSource`, a ready-to-play rodio source (implies `sync`) |
| `encryption` | no | Common Encryption (`cenc`, `cbcs`) decryption with keys you supply |
| `log` | no | Logging through the `log` crate |

## Streaming

`DashManifest::fragment_stream` returns a `playdash::fragment::FragmentStream`, which downloads the representation in the
background. It is an `Iterator` of whole fragments, and it implements `std::io::Read` and
`std::io::Seek` over the same bytes, so it can feed an MP4 demuxer or playback system. The two
share one position: after a partial `read`, `next` yields the rest of that fragment.

```rust,ignore
use std::io::Read;
use std::time::Duration;
use playdash::DashManifest;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;

// Whole fragments: the init segment, then every media segment.
for fragment in manifest.fragment_stream("FLAC")? {
    let bytes = fragment?;
}

// Bytes, for a demuxer.
let mut stream = manifest.fragment_stream("FLAC")?;
let mut mpeg_buffer = [0u8; 4096];
stream.read(&mut mpeg_buffer)?;

// Seek to a specific timestamp. Lands at the start of the containing fragment.
stream.seek_time(Duration::from_secs(90))?;
stream.read(&mut mpeg_buffer)?;
```

**N.B.** `Read` and `Iterator` both define `take`, `chain` and `by_ref`, so with
`std::io::Read` in scope, `stream.take(3)` is ambiguous and will not compile. Call those
through the trait you mean: `Iterator::take(stream, 3)` for fragments, `Read::take(stream, 1024)`
for bytes.

## Packets

`playdash::packet::PacketStream` demuxes a `FragmentStream` into packets for your decoder, so you don't need an
MP4 demuxer. It reads single-track representations, and `codec()` reports the codec (AAC, MP3, FLAC etc) with its decoder configuration. Packets come in decode order, timestamped in `timescale()` ticks.

```rust,ignore
use std::time::Duration;
use playdash::packet::PacketStream;
use playdash::DashManifest;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let mut packets = PacketStream::new(manifest.fragment_stream("FLAC")?)?;
let codec = packets.codec().clone();

// The packet at 90 seconds, after its pre-roll. `seek_keyframe` lands on the keyframe
// before instead, with less to decode.
let seeked = packets.seek_time(Duration::from_secs(90))?;
for packet in seeked.preroll {
    decoder.decode(&packet.data)?; // output discarded
}
for packet in packets {
    let packet = packet?;
    decoder.decode(&packet.data)?;
}
```

## Decoder-ready packets

A `Packet` holds an access unit as MP4 stores it, which is not always what a decoder reads:
H.264 and HEVC samples carry length prefixes where decoders expect start codes, and codec
configuration sits in MP4 boxes such as `avcC` and `dOps`. A `MediaStream` gives you
packets that a decoder reads, with the same timing. Its `initialization()` gives everything
to configure the decoder with: a `DecoderFormat`, the initialization bytes in that format,
and `DecoderParameters` such as the sample rate. playdash still doesn't decompress anything;
you choose and run the decoder.

| Codec | Packets | Initialization |
|---|---|---|
| H.264, HEVC | Annex B, with start codes | Parameter sets, with start codes; each unframed in `DecoderParameters` |
| AV1 | Low-overhead OBUs, each temporal unit after a temporal delimiter | Configuration OBUs |
| AAC | Raw access units | AudioSpecificConfig |
| FLAC | Frames | `fLaC` and the metadata blocks; the bare STREAMINFO in `DecoderParameters` |
| Opus | Raw packets | `OpusHead`, and the channel mapping in `DecoderParameters` |
| VP9 | Frames | The `vpcC` record |
| MP3, AC-3, E-AC-3, PCM | As stored | Empty, or the `dac3`/`dec3` payload |

```rust,ignore
use std::time::Duration;
use playdash::DashManifest;

let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
let mut input = manifest.media_stream("video").open()?;
decoder.configure(input.initialization())?;

// Seeking converts the pre-roll too. Reconfigure the decoder after resetting it.
let seeked = input.seek_time(Duration::from_secs(90))?;
decoder.reset()?;
decoder.configure(input.initialization())?;
for packet in &seeked.preroll {
    decoder.decode(&packet.data)?; // output discarded
}
for packet in input {
    for frame in decoder.decode(&packet?.data)? {
        // B-frames after the landing point can present before the time asked for.
        if !seeked.discards(frame.pts, frame.duration) {
            show(frame);
        }
    }
}
```

For audio, drop `seeked.trim_start_of(&packet)` ticks from the start of each packet's
output instead, which covers its encoder priming too. `seeked.landed_time()` gives where
the seek landed as a `Duration`.

That is the `Elementary` style, the default: for H.264, HEVC, AV1, FLAC, MP3 and PCM,
initialization followed by the packets is a valid elementary stream, as bitstream decoders
such as dav1d, openh264 and libFLAC read.
Decoders configured the way MP4 is, such as VideoToolbox, take H.264, HEVC and AV1 as MP4
stores them, with the `avcC`, `hvcC` or `av1C` record as initialization. `DecoderStyle`'s
docs list the style common decoders take. Ask for that style, whatever the codec:

```rust,ignore
use playdash::DecoderStyle;

let input = manifest.media_stream("video").style(DecoderStyle::ConfigRecord).open()?;
```

Other codecs have one format for both styles. `format` asks for one particular
`DecoderFormat`, and `open` fails if the codec cannot be converted to it, such as
`DecoderFormat::AacAdts` for decoders that only read AAC with ADTS headers.

`media_stream` returns a `MediaStreamBuilder`. Its other options:

```rust,ignore
use std::time::Duration;
use playdash::{ContentKind, Retention};

let video = manifest.highest_bandwidth(ContentKind::Video).ok_or("no video")?;
let (input, seeked) = manifest
    .media_stream(video.id)
    .cache(cache.clone())          // share downloads, or fetch through your own Transport
    .retention(Retention { behind: Some(Duration::from_secs(30)), ahead: Some(Duration::from_secs(60)) })
    .track(ContentKind::Video)     // or .track_id(1), for a representation with several tracks
    .preroll(Duration::from_millis(80))
    .open_at(Duration::from_secs(90))?; // or .open_at_keyframe(..)
// decode seeked.preroll, then the stream
```

With the `encryption` feature, `.keys(keys)` decrypts, adding the keys to the cache given or
to a new one. A cache that already decrypts takes extra keys at any time; one without keys
takes them only before any stream has used it.

To work at a lower level, `PacketStream::into_media_stream`, `into_media_stream_for` and
`into_media_stream_as` convert a `PacketStream` you have built, and the per-codec adapters
in `playdash::decoder` convert packets one at a time.

The `save_flac` example writes a FLAC representation's decoder-ready packets to a `.flac`
file:

```sh
cargo run --example save_flac -- MANIFEST_URL output.flac
```

## Async streaming

The `async` feature adds `playdash::nonblocking`, the same API for applications on a tokio
runtime, laid out the same way: `DashManifest` and `MediaStream` in `nonblocking`,
`FragmentStream`, `FragmentCache` and `Transport` in `nonblocking::fragment`, and
`PacketStream` in `nonblocking::packet`.

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
let mut stream = manifest.fragment_stream("FLAC").await?;
while let Some(fragment) = stream.next().await {
    let bytes = fragment?;
}

// Bytes, from 90 seconds in.
let mut stream = manifest.fragment_stream("FLAC").await?;
stream.seek_time(Duration::from_secs(90)).await?;
let mut buffer = [0u8; 4096];
stream.read(&mut buffer).await?;

// Decoder-ready packets, with the same builder options as the blocking API.
let mut input = manifest.media_stream("video").open().await?;

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

let stream = manifest.fragment_stream("1080p")?.with_retention(Retention {
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

## Conformance and Limitations

playdash is tested against the MP4 content of GPAC's
[Telecom ParisTech DASH conformance set](https://download.tsi.telecom-paristech.fr/gpac/DASH_CONFORMANCE/TelecomParisTech/).

We currently pass 61 / 91 (67%).

These are known limitations / failures where support is planned:

1. Hierarchical `sidx` indexes are not supported.

2. Multi-period playback and live (`type="dynamic"`) manifests are not yet supported.

3. open-GOP random access points are not supported.

4. gradual decoding refresh is not supported.

5. DRM systems such as Widevine and FairPlay are not supported. The encryption feature accepts
   content keys supplied by the caller; it does not acquire licenses.

6. Codec changes within a stream are not supported. `PacketStream` refuses samples that switch
   to another sample description, so a `MediaStream` keeps one initialization from
   start to end.

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
