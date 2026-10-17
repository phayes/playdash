# Changelog

## [Unreleased]

### Added

- `PacketStream` (blocking and async): a `FragmentStream`'s fragmented MP4 demuxed into
  `Packet`s in decode order. Each packet has its timestamps on the stream's media clock,
  keyframe flag, and priming trims from the edit list.
  - `codec()` returns a `Codec`: AAC (including HE-AAC), MP3, AC-3, E-AC-3, FLAC, Opus,
    PCM, H.264, HEVC, AV1 or VP9 with their decoder configuration, or `Codec::Unknown`.
    The configuration types, such as `AacConfig`, are in `playdash::codec`.
  - `seek_time` returns a `Seeked`, holding the pre-roll packets to decode before the
    stream's next packet. Pre-roll comes from `with_preroll`, the media's `roll` and `prol`
    sample groups, or `Codec::default_preroll`. A seek reads on into the next fragment when
    a timeline that only approximates fragment boundaries puts the time past the end of the
    fragment it names. Leading pictures that cannot be decoded from where decoding starts,
    flagged in the sample flags or, for HEVC, RASL pictures, are left out; when one would be
    shown, decoding starts at the keyframe before.
- `DecoderPacketStream` (blocking and async): a `PacketStream`'s packets in the form codec
  decoders read, from `PacketStream::into_decoder_packets`, `into_decoder_packets_for` or
  `into_decoder_packets_as`. It yields `DecoderPacket`s with the same timing as `Packet`s,
  and `initialization()` gives the `DecoderInitialization` to configure a decoder with: a
  `DecoderFormat`, its initialization bytes and `DecoderParameters`. `seek_time` returns a
  `DecoderSeeked` whose pre-roll is converted the same way.
  - `DecoderStyle` chooses the kind of format for any codec: `Elementary`, the default,
    for bitstream decoders, or `ConfigRecord` for decoders configured the way MP4 is, such
    as VideoToolbox, MediaCodec and WebCodecs.
  - H.264 and HEVC become Annex B, with their parameter sets as initialization, or, as
    `DecoderFormat::H264LengthPrefixed` and `HevcLengthPrefixed`, stay length-prefixed, with
    the `avcC` or `hvcC` record as initialization. Either way, `DecoderParameters::Video`
    also lists the VPS, SPS and PPS units unframed, as MediaCodec's `csd-0` and `csd-1`
    and VideoToolbox's parameter-set API take them.
  - AV1 becomes low-overhead OBUs with temporal delimiters, with av1C's configuration
    OBUs as initialization, or, as `DecoderFormat::Av1Samples`, stays as stored, with the
    `av1C` record.
  - AAC is raw access units with the AudioSpecificConfig, or, as `DecoderFormat::AacAdts`,
    each access unit after an ADTS header, for streams ADTS can describe; FLAC is frames with a `fLaC`
    stream header, and the bare STREAMINFO in `DecoderParameters::Flac`; Opus is raw
    packets with an `OpusHead` and its full channel mapping; VP9 is frames with the `vpcC`
    record; MP3, AC-3, E-AC-3 and PCM pass through.
  - Unknown codecs fail with `Error::UnsupportedDecoderInput`.
  - `playdash::decoder` has the per-codec adapters, `H264Adapter`, `HevcAdapter`,
    `Av1Adapter`, `AacAdapter` and `PassthroughAdapter`, which convert packets one at a
    time.
- `Error::UnsupportedDecoderInput` and `Error::MalformedDecoderInput`.
- `FragmentStream::tracks` (blocking and async): the representation's tracks as `Track`s,
  each with its ID, kind, codec and timescale, read from the initialization fragment.
  `PacketStream::with_track` reads one of them, as a muxed representation needs.
- `FragmentStream::media_fragment` (blocking and async): fetches any of the stream's media
  fragments without moving its position or retention window.
- `MediaTimeline::origin`: the media time at the start of the first media fragment, from
  `S@t`, `@presentationTimeOffset` or a `sidx` earliest presentation time.
- `Error::MalformedMedia` and `Error::UnsupportedMedia`.
- `list_packets`, `list_tracks` and `save_flac` examples.

### Changed

- `TimeAccuracy` is now `Fragment` or `Packet`, replacing `Coarse` and `Exact`. A reported
  `TimePosition` uses `Packet` when its timestamp is exact for the position, so a time seek
  reports `Packet` even when it lands before the time asked for. Compare the timestamp with
  the requested time to see how far back it landed.
- `FragmentStream::seek_time_coarse` is now `seek_time` (blocking and async).
- `MediaTimeline` has a new public field, `origin`.

### Fixed

- Decryption of a fragment whose `tfhd` gives an explicit base data offset now counts it from
  the start of the file, not of the fragment, as `SegmentBase` byte ranges need.

## [0.2.0] - 2026-10-16

### Added

- An `async` feature with `playdash::nonblocking`, the blocking API's async twin on tokio and
  reqwest: `DashManifest`, `FragmentStream`, `FragmentCache` and `Transport`. Its
  `FragmentStream` is a `Stream` of whole fragments and a tokio `AsyncRead` and `AsyncSeek`,
  with the same byte and time seeking, HEAD probing, buffer reports and decryption. Its failed
  requests are `Error::DashManifestRequest` and `Error::FragmentRequest`.
- The blocking `FragmentStream` is also an `Iterator` of whole fragments, as `Bytes`, sharing
  its position with `Read` and `Seek`.
- `Retention`, set with `FragmentStream::with_retention`: how much media a stream keeps behind
  its position and how far ahead it downloads, for long video. Fragments behind the window are
  dropped from the cache unless another stream keeps them, and seeking back downloads them
  again.
- `Bytes`, re-exported from the `bytes` crate, for implementing a `Transport`.
- `MediaTimeline::spans`, `MediaTimeline::total_duration` and `MediaTimeline::fragment_at`,
  which maps a timestamp to the index of its media fragment in `Fragments::media`.
- `Fragments`: a representation's initialization fragment and its media fragments.
- `DashManifest::stream_at` and `stream_at_with_cache`, and `FragmentStream::starting_at`
  (blocking and async): a stream of the initialization fragment, then the media from the
  fragment covering a time, as a decoder starting there needs. It reports and seeks by the
  representation's media time, and returns the fragment's start.
- `Representation::kind` (`ContentKind`: video, audio, text, muxed or unknown), `width`,
  `height`, `frame_rate` (`FrameRate`), `audio_sampling_rate`, `lang`, `roles` and
  `adaptation_set`.
- `BufferStatus::buffered_until_all`: how far several streams played together, such as video
  and its audio, are buffered.
- `DashManifest::load_segment_indexes`, for loading `SegmentBase` indexes over a `Transport`,
  and `nonblocking::DashManifest::load_segment_indexes`.
- serde `Serialize` and `Deserialize` for `Fragment`, `MediaTimeline`, `Retention`, `Position`,
  `TimePosition`, `TimeAccuracy`, `BufferStatus`, `BufferAmount` and `BufferTotal`, and
  `Serialize` for `Representation`.
- A `video_player` example: a minifb window, decoding video through the `ffmpeg` command, synced
  to audio played by `DashSource`.
- A `list_representations` example, printing a manifest's representations as JSON.

### Changed

- The `log` feature is no longer on by default.
- The `player` example is now `audio_player`.
- The blocking API (`FragmentStream`, `FragmentCache`, `Transport`,
  `DashManifest::new_from_url` and `stream`, and `Error::DashManifestFetch` and
  `Error::FragmentFetch`) moved behind a `sync` feature, on by default. Without it, ureq is no
  longer a dependency.
- `BufferHandle`, `BufferStatus`, `BufferAmount`, `BufferTotal`, `Position`, `TimePosition` and
  `TimeAccuracy` need the `sync` or `async` feature. With neither, the crate parses manifests
  only.
- `StreamReader` is now `FragmentStream`, matching `nonblocking::FragmentStream`.
- `Representation` is no longer `Copy`.
- `DashManifest::fragments` returns `Fragments`, with the initialization fragment in its own
  `init` field rather than first in one list, and `FragmentStream`'s constructors take
  `Fragments`.
- `Transport::get` and `FragmentCache::cached` use `Bytes` instead of `Arc<[u8]>`.
- A stream's download thread now waits for more work instead of exiting once everything is
  fetched, and ends when the stream is dropped.
- `DashManifest::stream` and `stream_with_cache`, and `FragmentStream`'s constructors, no
  longer take `eager_head`. Call `FragmentStream::start_eager_head` to probe sizes in the
  background.
- `Error::StreamHttp` is now `Error::FragmentFetch`, and `Error::StreamInitializationError` is
  now `Error::Initialization`.
- Parsing never touches the network. `DashManifest::new`, `new_from_data_url` and
  `with_base_url` no longer fetch `SegmentBase` indexes; `new_from_url` still does. Call
  `load_segment_indexes` after `with_base_url` for a manifest addressed by `SegmentBase`.

## [0.1.1] - 2026-10-15

### Fixed

- Manifests whose `AdaptationSet@group` is not an integer (e.g. Tidal's `group="main"`) now
  parse. The non-conformant value is ignored instead of failing the whole manifest.
- Disabling the default `log` feature now removes all logging dependencies. Previously
  `dash-mpd-core` always pulled in `tracing`.

### Changed

- `dash-mpd-core` is now vendored (from upstream 0.1.0, commit `c052cc1`) until the upstream fix
  for non-integer `AdaptationSet@group` is released.
- Dual-licensed under MIT OR Apache-2.0 (previously MIT only).

## [0.1.0] - 2026-10-08

Initial release.
