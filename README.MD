# VOCAN

VOCAN is a batch audio processing tool designed for voice-over preparation. It applies a configurable chain of dynamics processing, equalization, noise reduction, dereverberation, and loudness normalization, producing audio files in a choice of formats (4-bit IMA ADPCM WAV by default, plus PCM, FLAC, MP3, and OGG) suitable for embedded playback systems and game engines.

The application is written in Rust and provides a graphical interface built with `egui`. All heavy audio encoding is delegated to FFmpeg and optional external DSP utilities, while custom spectral, EQ, and expander modules are implemented natively. Folder scanning and per-file processing run in parallel on a Rayon thread pool sized to the host CPU.

## Features

- **Loudness normalization** (optional)
  - EBU R128 integrated loudness (LUFS) for files ≥ 3 seconds (two-pass, linear).
  - Silence-padded two-pass analysis for files between 1 and 3 seconds.
  - Peak normalization for files under 1 second, with adjustable target peak level.
  - Files that are silent or contain only negligible signal are converted without normalization and logged as _Skipped_.
- **Automixer chain** (optional)
  - De-esser (FFmpeg `deesser` filter)
  - Dereverberation via DeepFilterNet3 (external subprocess, joint denoise + dereverb)
  - Smart downward expander (custom Rust module, noise-floor-based and bounded)
  - Spectral gate (custom Rust FFT-based expander) **or** noise reduction with `nnnoiseless` (RNNoise-based) — mutually exclusive
  - Voice EQ (static biquad bank: low-shelf cut, two parametric cuts, high-shelf boost) at 50% strength
  - High-pass filter, six-band parametric EQ, and compressor with makeup gain (FFmpeg)
- **Multiple output formats**
  - 4-bit IMA ADPCM WAV (default, suggested for video game voice-over)
  - 16-bit PCM WAV, 32-bit float WAV (lossless)
  - FLAC (lossless compression)
  - MP3 and OGG Vorbis (lossy, with adjustable bitrate 64–320 kbps)
- **Analysis mode**
  Scan a folder and compute the average LUFS of all audio files. The result can be applied as the target normalization level with one click.
- **Parallel, resilient processing**
  Files are processed in parallel via Rayon. Each file is wrapped in a panic guard so a single failure is logged without poisoning the worker pool. Processing can be stopped gracefully — the current batch members finish or are skipped, and the run ends cleanly.

## Requirements

- **FFmpeg** – Must be either in the system `PATH` or placed in the same directory as the VOCAN executable (`ffmpeg.exe` on Windows, `ffmpeg` on Linux/macOS).
- **DeepFilterNet3** (optional) – Required only when the _Dereverb (DeepFilterNet3)_ option is enabled. The binary (`deep-filter.exe` on Windows) must reside next to the FFmpeg executable. Obtain it from [DeepFilterNet releases](https://github.com/Rikorose/DeepFilterNet/releases).

## Building

```bash
git clone https://github.com/JakubGrodzicki/VOCAN.git
cd VOCAN
cargo build --release
```

The binary will be located at `target/release/vocan` (or `vocan.exe` on Windows).

Dependencies are listed in `Cargo.toml`. The project uses:

- `eframe` / `egui` for the GUI
- `rfd` for native file dialogs
- `rustfft`, `biquad`, `nnnoiseless` for custom DSP modules
- `hound` for native WAV I/O (DeepFilterNet3 integration and intermediate DSP buffers)
- `tempfile` for temporary file management
- `rayon` / `num_cpus` for parallel file processing
- `anyhow` for error handling
- `serde` / `serde_json` for parsing FFmpeg `loudnorm` JSON output
- `walkdir` for recursive folder scanning

## Usage

1. Launch the application.
2. Set **Source** and **Output** directories.
3. (Optional) Use **Analyze folder loudness...** to measure the average LUFS of the source material and click **Set as target** to apply it.
4. Configure loudness:
   - **Normalize volume** – Enables loudness normalization with the chosen target LUFS (for files ≥ 1 s) and target peak dBFS (for shorter files).
5. Choose the **Output Format** (and bitrate for MP3/OGG).
6. Configure the **Automixer** (optional). When enabled:
   - _Intelligent noise removal (Spectral Gate)_ – Custom FFT gate; mutually exclusive with _nnnoiseless_.
   - _Noise reduction (nnnoiseless)_ – RNNoise-based denoiser; mutually exclusive with the spectral gate.
   - _Dereverb (DeepFilterNet3)_ – Runs before the gate/EQ. Mix level and optional post-filter can be adjusted.
   - _Downward Expander (noise floor)_ – Estimates the noise floor from the pre-dereverb signal and applies bounded downward expansion. A **Safety Margin** slider (0–100%) controls how far below the floor the threshold sits; a **Reduction profile** (Safe / Recommended / Hard / Max) caps the maximum attenuation.
   - Voice EQ is always applied at 50% strength when Automixer is enabled.
7. Click **START PROCESSING**. Progress and log messages are displayed in the lower pane.

Processing can be stopped gracefully via the **Stop** button — in-flight files finish, and remaining files are skipped.

## Technical Details

### Audio Pipeline (Automixer Mode)

When the Automixer is enabled, each file flows through a hybrid Rust/FFmpeg pipeline. All in-Rust DSP operates at 48 kHz mono, 32-bit float. The original sample rate is restored at the final encoding step.

1. **De-esser** – FFmpeg `deesser` filter (`i=0.4:m=0.5:f=0.5:s=o`), output piped to Rust as f32le at 48 kHz mono.
2. **Noise floor estimation** (if the expander is enabled) – Windowed RMS (25 ms, 50% overlap, Hann windowed), p12 percentile. Runs on the post-de-esser / pre-dereverb signal so denoising does not bias the estimate. If the file is too short or too quiet to build a reliable distribution, the expander is silently bypassed for that file.
3. **DeepFilterNet3** (if enabled) – Joint denoise/dereverb at 48 kHz mono. The `-D` flag ensures sample-accurate latency compensation. WAV I/O is handled natively via `hound` (no extra FFmpeg subprocess). Output is mixed with the dry signal according to the selected mix ratio; attenuation limit defaults to 30 dB and an optional `--pf` post-filter can be enabled.
4. **Downward Expander** (if enabled) – RMS-level detector (≈3 ms window) with soft knee, attack/release smoothing, and a mandatory hold time to prevent chattering. Threshold = `noise_floor − margin_db`, where `margin_db` is derived from the Safety Margin slider (2–8 dB). Attenuation is bounded by the selected reduction profile and never reaches −∞.
   | Profile      | Max attenuation |
   | ------------ | ---------------- |
   | Safe         | −8.0 dB          |
   | Recommended  | −12.0 dB         |
   | Hard         | −18.0 dB         |
   | Max          | −32.0 dB         |
5. **Spectral Gate / nnnoiseless** (mutually exclusive) – Either the custom FFT expander or RNNoise runs at 48 kHz mono.
6. **Voice EQ** – Four-band biquad (low shelf −3 dB @ 120 Hz, cut −1.5 dB @ 400 Hz, boost +2 dB @ 2.5 kHz, high shelf +1.5 dB @ 10 kHz). Always applied at 50% strength.
7. **Post-EQ & Dynamics** (FFmpeg)
   - 2-pole + 1-pole high-pass at 70 Hz
   - Five parametric EQ bands (90, 175, 360, 1350, 4246 Hz) plus an 8 kHz high shelf
   - Compressor (threshold ≈ −12 dBFS, ratio 4:1, attack 5 ms, release 80 ms, makeup 4 dB)
8. **Normalization** – EBU R128 two-pass (linear) with optional padding for short files, or peak normalization. Pass-1 stats are measured on the DSP-processed temp WAV so the linear pass-2 operates on the same signal that was measured.
9. **Encoding** – Selected output format, restoring the original sample rate.

### Non-Automixer Mode

When the Automixer is disabled, files go straight to FFmpeg for optional loudness/peak normalization and encoding — no DSP modules, no intermediate temp files.

### Spectral Gate

- FFT size 2048, hop 512 (75% overlap), Hann window.
- Envelope follower with independent attack/release coefficients per frequency bin.
- Threshold defines expansion knee; ratio controls downward expansion slope below threshold.

### nnnoiseless

- Uses the `nnnoiseless` crate (RNNoise port).
- Input is scaled to 16-bit integer range expected by the model.
- Dry/wet mix default 0.75; output is latency-compensated by one RNNoise frame.

### DeepFilterNet3 Integration

- Communicates via temporary WAV files using subprocess calls to `deep-filter` (located next to FFmpeg or the VOCAN executable).
- Expects 48 kHz mono; output is mixed with the original signal according to the selected mix ratio.
- WAV I/O is handled natively via the `hound` crate.

### Loudness Analysis Fallback

| File Duration | Method                                                     |
| ------------- | ---------------------------------------------------------- |
| ≥ 3 s         | Standard two-pass EBU R128                                 |
| 1 s … 3 s     | Two-pass with silence padding to at least 5 s              |
| < 1 s         | Peak normalization to `target_peak_dbfs` (default −3 dBFS) |

If a file is completely silent or contains only negligible signal, it is converted without normalization and logged as _Skipped_.

## Logging and Error Handling

All operations are logged to the GUI. FFmpeg stderr is captured on failure and included in the error message. Per-file panics are caught and logged without killing the worker pool. Temporary files are automatically cleaned up via `tempfile`; the DeepFilterNet3 output directory is also removed after processing.

## License

No explicit license is provided in the source files. All rights reserved by the original author unless otherwise noted.
