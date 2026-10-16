# Changelog

## [Unreleased]

### Added

- An `async` feature with `playdash::nonblocking`, the blocking API's async twin on tokio and
  reqwest: `DashManifest`, `FragmentStream`, `FragmentCache` and `Transport`. Its
  `FragmentStream` is a `Stream` of whole fragments and a tokio `AsyncRead` and `AsyncSeek`,
  with the same byte and time seeking, HEAD probing, buffer reports and decryption.
- The blocking `FragmentStream` is also an `Iterator` of whole fragments, as `Bytes`, sharing
  its position with `Read` and `Seek`.
- `Retention`, set with `FragmentStream::with_retention`: how much media a stream keeps behind
  its position and how far ahead it downloads, for long video. Fragments behind the window are
  dropped from the cache unless another stream keeps them, and seeking back downloads them
  again.
- `Bytes`, `Position`, `TimePosition`, `TimeAccuracy` and `BufferHandle` are available with
  either `sync` or `async`.
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
- The blocking API (`FragmentStream`, `FragmentCache`, `Transport`, `BufferHandle`,
  `DashManifest::new_from_url` and `stream`) moved behind a `sync` feature, on by default.
  Without it, ureq is no longer a dependency.
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
