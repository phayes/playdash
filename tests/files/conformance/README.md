# Conformance manifests

MPDs from GPAC's Telecom ParisTech DASH conformance set,
<https://download.tsi.telecom-paristech.fr/gpac/DASH_CONFORMANCE/TelecomParisTech/>, mirroring
its directory tree. `tests/conformance.rs` parses each manifest under `pass/`, resolving its
relative URLs against its source, and checks it to the level of its `test_type` in
`index.json`:

- `parse`: it parses.
- `manifest`: also, for each representation, the manifest's ID for it reaches it, and it lists
  its fragments, one per media segment of its timeline.
- `full`, the default: also streams its media from GPAC's server.

`parse` and `manifest` are for manifests whose media is missing from the server. `parse` is
for those whose fragments cannot be listed without the media, such as `SegmentBase` ones,
whose segment index is in the media file.

For `full` manifests, each representation also:

- lists the tracks its `ContentKind` says in its stream (two for muxed ones);
- `PacketStream` reads the first two fragments of each track, starting at a keyframe, with
  non-empty packets, increasing decode times and distinct presentation times;
- if `ffprobe` is on `PATH`, the packets of the first fragment match ffprobe's;
- a packet-accurate seek to the middle lands on the next packet, after a pre-roll that
  starts at a keyframe, and decodes the packet presenting at that time.

Subtitle tracks are listed but not read.

## Running

`parse` and `manifest` checks run offline with the other tests. Streaming the `full`
manifests needs the network and downloads a lot, so it only runs with the test-only
`conformance` feature:

```sh
cargo test --features conformance --test conformance
```

`--all-features` turns it on too.

- `PLAYDASH_CONFORMANCE_CACHE=<dir>` keeps every download in `<dir>` and reads it back on
  later runs.
- `PLAYDASH_CONFORMANCE_FILTER=<text>` streams only the manifests whose path contains `<text>`,
  such as `mp4-live/` or `mp4-onDemand-mpd-AV`.

`index_lists_every_manifest` runs offline with the other tests and keeps `index.json` in step
with the directories.

## Layout

- `pass/`: manifests expected to pass.
- `fail/`: manifests that fail because of something this crate needs to fix or improve, which
  the test skips.
- `invalid/`: manifests that cannot pass whatever this crate does, as the manifest itself is
  broken, which the test skips. They do not count towards the conformance figures.
- `index.json`: each manifest by its path under `pass/`, `fail/` or `invalid/`, with its
  source `url` and, if not `full`, its `test_type`. Each one under `fail/` has a `reason`:
  what this crate gets wrong with it. Each one under `invalid/` has an `invalid_reason`: what
  is broken. Any may have a `note` with anything else to know about it; each under `pass/`
  whose `test_type` is below `full` has one saying why.

When a manifest fails, either fix the crate or move it from `pass/` to the same path under
`fail/` and give it a `reason` in `index.json`. Once fixed, move it back and remove the
`reason`. When its media is missing from the server, lower its `test_type` instead. Only a
manifest that cannot pass even `parse` belongs under `invalid/`.

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
