# Changelog

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
