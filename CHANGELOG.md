# Changelog

## [Unreleased]

### Added

- `PacketStream` (blocking and async): a `SegmentStream`'s fragmented MP4 demuxed into
  `Packet`s in decode order. Each packet has its timestamps on the stream's media clock,
  keyframe flag, and priming trims from the edit list.
  - `codec()` returns a `Codec`: AAC (including HE-AAC), MP3, AC-3, E-AC-3, FLAC, Opus,
    PCM, H.264, HEVC, AV1 or VP9 with their decoder configuration, or `Codec::Unknown`.
    The configuration types, such as `AacConfig`, are in `playdash::codec`.
  - `seek_time` lands on the packet presenting at a time, and `seek_keyframe` on the last
    keyframe at or before it, with less to decode. Both return a `PacketSeeked`, holding the
    pre-roll packets to decode before the stream's next packet, where it landed, and the
    time before which to discard decoded output. Its `landed_time` and
    `discard_before_time` give those as `Duration`s, `discards` says whether a decoded
    frame is before it, and `trim_start_of` how much of an audio packet's output to drop.
    Pre-roll comes from `with_preroll`, the media's `roll` and `prol`
    sample groups, or `Codec::default_preroll`. A seek reads on into the next segment when
    a timeline that only approximates segment boundaries puts the time past the end of the
    segment it names. Leading pictures that cannot be decoded from where decoding starts,
    flagged in the sample flags or, for HEVC, RASL pictures, are left out; when one would be
    shown, decoding starts at the keyframe before.
- `MediaStream` (blocking and async): a representation's packets in the form codec
  decoders read, from `DashManifest::media_stream`, or from a `PacketStream` by
  `PacketStream::into_media_stream`, `into_media_stream_for` or `into_media_stream_as`.
  It yields `MediaPacket`s with the same timing as `Packet`s,
  and `initialization()` gives the `DecoderInitialization` to configure a decoder with: a
  `DecoderFormat`, its initialization bytes and `DecoderParameters`. `seek_time` and
  `seek_keyframe` return a `Seeked` whose pre-roll is converted the same way, with
  the same methods as `PacketSeeked`.
  - `DecoderStyle` chooses the kind of format for any codec: `Elementary`, the default,
    for bitstream decoders, or `ConfigRecord` for decoders configured the way MP4 is, such
    as VideoToolbox. `DecoderStyle`'s docs list the style common decoders take.
  - H.264 and HEVC become Annex B, with their parameter sets as initialization, or, as
    `DecoderFormat::H264LengthPrefixed` and `HevcLengthPrefixed`, stay length-prefixed, with
    the `avcC` or `hvcC` record as initialization. Either way, `DecoderParameters::Video`
    also lists the VPS, SPS and PPS units unframed, as MediaCodec's `csd-0` and `csd-1`
    and VideoToolbox's parameter-set API take them.
  - AV1 becomes low-overhead OBUs with temporal delimiters, with av1C's configuration
    OBUs as initialization, or, as `DecoderFormat::Av1Samples`, stays as stored but with a
    size field on every OBU, with the `av1C` record.
  - AAC is raw access units with the AudioSpecificConfig, or, as `DecoderFormat::AacAdts`,
    each access unit after an ADTS header, for streams ADTS can describe; FLAC is frames with a `fLaC`
    stream header, and the bare STREAMINFO in `DecoderParameters::Flac`; Opus is raw
    packets with an `OpusHead` and its full channel mapping; VP9 is frames with the `vpcC`
    record; MP3, AC-3, E-AC-3 and PCM pass through.
  - Unknown codecs fail with `Error::UnsupportedDecoderInput`.
  - `DashManifest::media_stream` returns a `MediaStreamBuilder`, which opens a
    representation as a `MediaStream` in one call. Its options are `cache`, `keys` (with
    the `encryption` feature, added to the given cache or a new one), `retention`, `track`
    (by `ContentKind`) or `track_id`,
    `preroll`, and `style` or `format`. Finish it with `open`, or with `open_at` or
    `open_at_keyframe`, which also seek and return the `Seeked`.
  - `playdash::decoder` has the per-codec adapters, `H264Adapter`, `HevcAdapter`,
    `Av1Adapter`, `AacAdapter` and `PassthroughAdapter`, which convert packets one at a
    time.
- `DashManifest::representations_of`, `highest_bandwidth` and `lowest_bandwidth` (blocking
  and async): pick representations by `ContentKind` and advertised bandwidth.
- `Error::UnsupportedDecoderInput` and `Error::MalformedDecoderInput`.
- `SegmentStream::tracks` (blocking and async): the representation's tracks as `Track`s,
  each with its ID, kind, codec and timescale, read from the initialization segment.
  `PacketStream::with_track` reads one of them, as a muxed representation needs.
- `SegmentCache::add_keys` (blocking and async, with the `encryption` feature): adds
  content keys to a cache that already decrypts, or to one no stream has used yet.
- `SegmentStream::fetch_segment` (blocking and async): fetches the body of any of the
  stream's media segments without moving its position or retention window.
- Timed `Segments`: `Segments::timed` builds them from `MediaSegment`s, each a
  segment location with its start and duration in media ticks, on a `MediaTiming` clock
  (timescale, `@presentationTimeOffset` and `Period@start`). `DashManifest::segments`
  returns timed segments. `span`, `segment_at`, `end`, `estimate_len` and `timing` read
  them.
- `Error::InvalidSegmentTiming`, for media segments with a zero duration or that overlap.
- `Error::MalformedMedia` and `Error::UnsupportedMedia`.
- `list_packets`, `list_tracks` and `save_flac` examples.

### Changed

- What DASH calls a segment is now named a segment throughout; "fragment" is left to the
  `moof`+`mdat` movie fragments inside a media segment. From 0.2.0:
  - `Fragment` is `SegmentLocation`, `Fragments` is `Segments`, `FragmentStream` is
    `SegmentStream` and `FragmentCache` is `SegmentCache` (blocking and async).
  - `DashManifest::fragments` is `segments` (blocking and async).
  - `Error::FragmentLength`, `FragmentFetch` and `FragmentRequest` are `SegmentLength`,
    `SegmentFetch` and `SegmentRequest`, and the `fragment` field of those and of
    `MalformedMedia`, `UnsupportedMedia`, `UnsupportedProtection` and
    `MalformedProtection` is `segment`.
  - `BufferAmount::fragments` and `BufferTotal::fragments` are `segments`.
  - `Transport::get` and `head`, and `SegmentCache::cached` and `known_size`, name their
    argument `segment`.
- `TimeAccuracy` is now `Segment` or `Packet`, replacing `Coarse` and `Exact`. A reported
  `TimePosition` uses `Packet` when its timestamp is exact for the position, so a time seek
  reports `Packet` even when it lands before the time asked for. Compare the timestamp with
  the requested time to see how far back it landed.
- `SegmentStream::seek_time_coarse` is now `seek_time` (blocking and async).
- Every reported media time is now presentation time, shared by all representations of
  a manifest: a media timestamp equal to `@presentationTimeOffset` presents at
  `Period@start`. This covers segment spans, stream `Position`s, `seek_time`, the start
  `segment_stream_at` returns, `BufferStatus` ranges, and packet timestamps. Times
  change only for media whose first segment does not start at the offset, such as a
  `SegmentTimeline` starting late or a `sidx` with a non-zero earliest presentation
  time, or in a period starting after zero.
- `MediaTimeline` is removed, with `DashManifest::media_timeline` (blocking and async).
  Its timing is part of `Segments`, so `SegmentStream::new`, `new_with_transport`,
  `new_with_cache` and `starting_at` (blocking and async) no longer take a timeline.
- `Segments` fields are private, read with `init()`, `media()` and `media_len()`, and
  `Segments` is no longer `Serialize` or `Deserialize`.
- `Segments::media()` yields owned `SegmentLocation`s rather than references.
- A `SegmentTemplate` whose `@media` has an unsupported format tag, or names
  `$Bandwidth$` without a `@bandwidth`, now fails `DashManifest::segments` even when it
  addresses no media segments.
- Streams with a bounded `Retention`, including a `MediaStream` opened with
  `MediaStreamBuilder::retention` (blocking and async), set up only the segments their
  window reaches, not every segment of the representation.
- `BufferTotal::time` is replaced by `start` and `end`, the presentation times the
  representation's media spans. `BufferStatus` percentages count from `start`.
- `DashSource` positions count from the start of the representation's media.
- The crate root now holds the `MediaStream` API and what it takes. Segment streaming
  moves to `playdash::segment`: `SegmentLocation`, `Segments`, `MediaSegment`,
  `MediaTiming`, `SegmentStream`, `Position` and `TimePosition`, with `SegmentCache` and
  `Transport` there and at the root. Packets move to `playdash::packet`: `PacketStream`,
  `Packet`, `PacketSeeked` and `Track`.
- `playdash::nonblocking` is laid out the same way: `DashManifest`, `MediaStream` and
  `MediaStreamBuilder` in `nonblocking`, `SegmentStream`, `SegmentCache` and `Transport`
  in `nonblocking::segment`, and `PacketStream` in `nonblocking::packet`.
- `DashManifest::stream`, `stream_with_cache`, `stream_at` and `stream_at_with_cache` are
  now `segment_stream`, `segment_stream_with_cache`, `segment_stream_at` and
  `segment_stream_at_with_cache` (blocking and async).

### Fixed

- Decryption of a movie fragment whose `tfhd` gives an explicit base data offset now counts
  it from the start of the file, not of the segment, as `SegmentBase` byte ranges need.
- `SegmentList@presentationTimeOffset` and `SegmentBase@presentationTimeOffset` are honoured;
  they were ignored before. A `SegmentBase` offset counts `SegmentBase@timescale` ticks, even
  when the `sidx` uses another timescale.
- A `SegmentStream` with a `Retention` (blocking and async) no longer sometimes downloads a
  segment again just after reading past it.
- `SegmentStream::seek_bytes` (blocking and async) with `SeekFrom::End` fails when the
  stream's length is unknown because HEAD requests failed, rather than seeking as if the
  length were 0.
- A `SegmentTemplate` or `SegmentList` whose segment numbers or media times overflow fails
  `DashManifest::segments` with `Error::DashManifestInvalidSegments`, rather than panicking.

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
