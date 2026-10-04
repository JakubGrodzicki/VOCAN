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

`cargo test -- --ignored` also runs `tests/vad_integration.rs`, which
exercises the real Silero VAD speech detection. These tests need the model
file and a speech recording. They read both paths from environment variables:

- `VOCAN_SILERO_MODEL`: full path to `silero_vad.onnx`
- `VOCAN_VAD_FIXTURE`: full path to a 16 kHz mono speech WAV (CI uses the
  60 s `test.wav` from the Silero repository)

If the model, the fixture or ffmpeg is missing, each test prints a `SKIP`
notice and passes, the same as the DFN3 tests. CI downloads both files into
the runner's temp folder, checks their SHA-256, and sets both variables
automatically. The installers set `VOCAN_SILERO_MODEL` for the test run when
the model download succeeded; the fixture is CI only, so the speech-recording
tests skip on a normal install.

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
  It also holds the silence trimming tests: the threshold trim cuts both ends
  and keeps the pause inside the line, works from the end of the Automixer
  chain, depends on the `Silence below` preset (a noisy take is only trimmed
  when the preset is high enough), the `Keep` preset leaves the advertised
  amount of silence and never invents any, a take with no dead air passes
  through unchanged, and a fully silent file is reported as an error instead
  of being written empty.
- `tests/spawn_budget.rs`: pins how many ffmpeg processes VOCAN launches per
  file (plain conversion 3, Automixer 4, no normalization drops the
  measurement pass only). Includes the trimming entries: the threshold trim
  costs no extra process in either pipeline, and trimming without
  normalization still costs exactly one process. Silero VAD mode adds one
  process per file for the detection decode (see the README, "Silero VAD
  speech detection").
- `tests/vad_integration.rs`: Silero VAD against the real model and real
  ffmpeg (ignored tests, skip when the model, the fixture or ffmpeg is
  missing). Covers detection on the speech fixture and the end-to-end trim
  behaviour: 150 ms margins, 75 ms fades, no change when there is no speech,
  with the Automixer on and off.
- `src/vad.rs` `#[cfg(test)] mod tests`: gate tests for the VAD logic that
  needs no model or ffmpeg (hysteresis, minimum speech burst, minimum pause,
  span and margin arithmetic, model path lookup order). One ignored test
  there, `probabilities_match_the_reference_run`, pins the model's own scores
  on the speech fixture, so a wrong input to the model (for example a lost
  context window) fails it.
- `tests/installer_consistency.rs`: gate test that `MODEL_URL` and
  `MODEL_SHA256` appear verbatim in `installWindows.ps1`,
  `installMacLinux.sh` and `.github/workflows/ci.yml`, so a pinned-model bump
  cannot be done in one place and forgotten in another.
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
10. Silero VAD checkbox. Run on a small folder of takes with silence or noise
    around the speech:
    - With `models/silero_vad.onnx` next to the app executable: tick **Trim
      silence**, tick **Detect speech with Silero VAD**. Confirm the **Silence
      below** and **Keep** controls no longer apply. Process the folder and
      open an output in an editor: about 150 ms before the first word, about
      150 ms after the last word, a short fade at each edge, and any pause
      inside the line still there.
    - Process a file that holds only noise or silence: the output must match
      the input (no trim, no error).
    - Repeat once with the Automixer on and once with it off.
    - Rename or remove the `models` folder (and unset `VOCAN_SILERO_MODEL`),
      then restart the app: the checkbox must be disabled and show a hint to
      re-run the installer.
    - Set `VOCAN_SILERO_MODEL` to the full path of the model with the `models`
      folder removed: the checkbox must be enabled again.
