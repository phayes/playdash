# Conformance manifests

MPDs from GPAC's Telecom ParisTech DASH conformance set,
<https://download.tsi.telecom-paristech.fr/gpac/DASH_CONFORMANCE/TelecomParisTech/>, mirroring
its directory tree. `tests/conformance.rs` parses each manifest under `pass/`, resolving its
relative URLs against its source, and streams its media from GPAC's server. For each
representation it checks that:

- the manifest's ID for the representation reaches it;
- its stream lists the tracks its `ContentKind` says (two for muxed ones);
- `PacketStream` reads the first two fragments of each track, starting at a keyframe, with
  non-empty packets, increasing decode times and distinct presentation times;
- if `ffprobe` is on `PATH`, the packets of the first fragment match ffprobe's;
- a packet-accurate seek to the middle lands on the next packet, after a pre-roll that
  starts at a keyframe, and decodes the packet presenting at that time.

Subtitle tracks are listed but not read.

## Running

The test needs the network and downloads a lot, so it only runs with the test-only
`conformance` feature:

```sh
cargo test --features conformance --test conformance
```

`--all-features` turns it on too.

- `PLAYDASH_CONFORMANCE_CACHE=<dir>` keeps every download in `<dir>` and reads it back on
  later runs.
- `PLAYDASH_CONFORMANCE_FILTER=<text>` checks only the manifests whose path contains `<text>`,
  such as `mp4-live/` or `mp4-onDemand-mpd-AV`.

`index_lists_every_manifest` runs offline with the other tests and keeps `index.json` in step
with the directories.

## Layout

- `pass/`: manifests expected to pass.
- `fail/`: manifests known to fail, which the test skips.
- `index.json`: each manifest by its path under `pass/` or `fail/`, with its source `url`.
  Each one under `fail/` has either:
  - a `reason`: what this crate gets wrong with it; or
  - an `invalid_reason`: why it cannot pass whatever this crate does, such as media missing
    from the server. Invalid manifests are always skipped, and do not count towards the
    conformance figures.

When a manifest fails, either fix the crate or move it from `pass/` to the same path under
`fail/` and give it a `reason` or `invalid_reason` in `index.json`. Once fixed, move it back
and remove the `reason`.

## Scope

All MP4 content in the set: the `mp4-*` directories, `SRD/mp4-live`, `SRD/mp4-onDemand`, and
`advanced/` with its `pto/` and `periods/` subdirectories. Not included:

- `advanced/invalid_*.mpd` and `SRD/invalid/`, deliberately invalid manifests for
  validators;
- `base64/`, whose manifests point at media on a BBC server;
- `mpeg2-*`, MPEG-2 transport streams;
- `ttml-*`, standalone TTML subtitles.

To add a manifest, download it to its source path under `pass/` and add its entry to
`index.json`.
