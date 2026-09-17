# AGENTS.md

Single-binary Rust CLI. `src/main.rs` is the whole program: it converts ROSE Online terrain
(`.ZON` / `.HIM` / `.TIL`) into Bevy-friendly assets (mesh `.bin`, baked albedo, normal map,
lightmap PNG, `.mat.ron`). No tests, no CI, no lint/format config, no library target.

## Commands

- Iterate with `cargo check` — the release profile is `lto = true`, `codegen-units = 1`, so
  `cargo build --release` is slow.
- Run: `cargo run --release -- --data-root <PATH_TO_3Ddata> [--resolution 2048] [--zone <dir relative to data_root>]`
- There is no test suite and no sample data in the repo. Nothing can be exercised without a real
  ROSE `3Ddata` tree, so prefer reviewing over running unless the user supplies that path.

## Dependency outside this repo

`Cargo.toml` uses **absolute** path deps on the sibling `rose-offline` checkout
(`../rose-offline/rose-data` and `../rose-offline/rose-file-readers` in spirit). The build fails
without that sibling repo present at the path written in `Cargo.toml`. All parsing (`ZonFile`,
`HimFile`, `TilFile`, `ZonTileRotation`) lives in `rose-file-readers` — fix format bugs there,
not here.

- `rose-data` is declared but never referenced by the code; it still compiles and emits warnings.
  Warnings from path deps are noise here.
- The lockfile carries two `image` versions: our direct dep is 0.24, Bevy 0.16 pulls 0.25. Never
  pass image types across that boundary.
- Bevy is used purely as a mesh/data-structure library (`default-features = false`, no App/World).
  Keep its feature list minimal.

## Things that break silently

- Output is written **in place into the game data folder** as `block_X_Y.*`. No backup; stale
  outputs are never deleted.
- Exit code is always 0. Zone/block failures are only printed to stderr inside rayon loops
  (`Error processing ...`). Judge success by stderr, not status.
- Missing tile textures do not fail: albedo silently falls back to a solid **magenta** 128x128
  placeholder (`src/main.rs:418`). Magenta in output means a bad `--data-root` or unresolved path.
- `--resolution` must be a multiple of 16. `tile_res = res / 16` truncates, otherwise the baked
  albedo gets black strips on the right/bottom.
- Blocks are scanned as a fixed 64x64 grid of `{bx}_{by}.HIM`; a missing HIM skips the block with
  no message. `.TIL` and `{bx}_{by}/LM_TERRAIN.DDS` are optional.
- World scale is baked into three places: height ` / 100.0`, X/Z vertex spacing `2.5`, and the
  matching normals at `src/main.rs:207` (mesh) and `src/main.rs:761` (normal map). Both normals
  encode `(-dh/dx * 2.5, k, -dh/dy * 2.5)`; change one, change the other.
- Heightmap sampling is Catmull-Rom on purpose (`src/main.rs:774`) for C1-continuous normals;
  reverting to bilinear reintroduces block seams.
- DDS decoding is hand-rolled (DXT1/3/5, RGB565, RGBA4444) because ROSE textures need it; the
  `image` crate is only a fallback path.
- `.mesh.bin` is a hand-rolled little-endian format whose only spec is `README.md:48`. Changing the
  writer must update the README and any downstream loader (none exists in `rose-offline` today).
