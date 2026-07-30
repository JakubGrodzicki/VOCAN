# Testing

## Automated tests

```bash
cargo test                  # fast: pure logic, no ffmpeg required
cargo test -- --ignored     # full: ffmpeg-dependent integration/e2e tests
```

The `--ignored` tests shell out to a real `ffmpeg` binary on PATH. Each one
also checks at runtime whether ffmpeg is available and prints a `SKIP`
notice instead of failing if it isn't, so they degrade gracefully on a
machine without ffmpeg -- but the primary gate is `#[ignore]` itself.

`cargo test -- --ignored` also runs `tests/dfn3_integration.rs`, which
exercises the real DeepFilterNet3 dereverb path. These tests check at
runtime for a `deep-filter` binary next to `ffmpeg` on PATH, and print a
`SKIP` notice (not a failure) if it isn't there -- so they pass trivially on
a machine or CI runner without DeepFilterNet3 installed, and actually run
the model on a machine that has it (for example, after running
`installMacLinux.sh` / `installWindows.ps1` without `--no-dfn3` / `-NoDfn3`).

### What's covered where

- `src/*.rs` `#[cfg(test)] mod tests` blocks: pure logic (DSP math on
  synthetic signals, the loudness-normalization decision table, ffmpeg
  stderr parsing, `AudioBatchApp` message-handling state).
- `tests/ffmpeg_integration.rs`: `ffmpeg.rs` functions against real ffmpeg,
  using synthetic WAV fixtures generated on the fly.
- `tests/pipeline_e2e.rs`: full `process_single_file` pipeline across output
  formats and automixer option combinations, asserting measurable output
  properties (format, loudness, finiteness/no-clipping) rather than
  byte-exact golden files. DFN3 dereverb is intentionally excluded from
  this file's combination matrix (see `tests/dfn3_integration.rs` instead).
- `tests/dfn3_integration.rs`: the DeepFilterNet3 dereverb integration --
  direct calls to `apply_dereverb_dfn3`, and a full pipeline run with
  dereverb enabled. Skipped automatically when `deep-filter` isn't
  installed next to ffmpeg.

## Manual smoke test (GUI)

The GUI itself (`src/app.rs`'s `eframe::App::update`) has no automated
widget-level test coverage -- egui is immediate-mode and not worth automating
here. Before a release, or after any GUI-adjacent change, run through:

1. Launch the app (`cargo run --release`).
2. Browse to a source folder and an output folder (or type the paths).
3. Click "Analyze folder loudness..." on a small folder; confirm the average
   LUFS reading appears and "Set as target" works.
4. Toggle Normalize volume on/off; drag both sliders.
5. Toggle Automixer, then each sub-module individually (Spectral Gate /
   nnnoiseless -- mutually exclusive in the UI --, DFN3 dereverb + mix/post-filter,
   Downward Expander with Safety Margin % and each Reduction profile).
6. Pick each Output Format in turn; confirm the bitrate field only appears
   for MP3/OGG.
7. Click "START PROCESSING" on a small folder; watch the progress bar and
   log pane (colors: red for `[ERROR]` lines).
8. Click "Stop" mid-run; confirm it actually stops and the UI returns to an
   idle state.
9. If testing DFN3 dereverb specifically: ensure a `deep-filter` binary is
   next to `ffmpeg` (or next to the app executable) first. `tests/dfn3_integration.rs`
   already covers the underlying pipeline logic; this manual pass is only to
   confirm the checkbox, mix slider, and post-filter option behave correctly
   in the GUI itself.
