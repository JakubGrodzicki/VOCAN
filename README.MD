# VOCAN

VOCAN is a batch audio processing tool designed for voice-over preparation. It applies a configurable chain of dynamics processing, equalization, noise reduction, and loudness normalization, producing 4-bit IMA ADPCM WAV files suitable for embedded playback systems.

The application is written in Rust and provides a graphical interface built with `egui`. All heavy audio processing is delegated to FFmpeg and optional external DSP utilities, while custom spectral and EQ modules are implemented natively.

## Features

- **Loudness normalization**
  - EBU R128 integrated loudness (LUFS) for files ≥ 3 seconds (two‑pass).
  - Silence‑padded analysis for files between 1 and 3 seconds.
  - Peak normalization for files under 1 second, with adjustable target peak level.
- **Automixer chain** (optional)
  - De‑esser (FFmpeg `deesser` filter)
  - Spectral gate (custom Rust FFT‑based expander)
  - Noise reduction using `nnnoiseless` (RNNoise‑based)
  - Dereverberation via DeepFilterNet3 (external subprocess)
  - Voice EQ (static biquad bank: low‑shelf cut, two parametric cuts, high‑shelf boost)
  - High‑pass filter and additional parametric EQ (FFmpeg)
  - Compressor with makeup gain (FFmpeg `acompressor`)
- **Analysis mode**  
  Scan a folder and compute the average LUFS of all audio files. The result can be used as the target normalization level.
- **Resilient processing**  
  Preserves original sample rate; stereo is downmixed to mono early in the chain. All intermediate audio is handled as 32‑bit float PCM.

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
- `rustfft`, `biquad`, `nnnoiseless` for custom DSP modules
- `tempfile` for temporary file management
- `serde`, `walkdir`, `rfd` for configuration and file handling

## Usage

1. Launch the application.
2. Set **Source** and **Output** directories.
3. (Optional) Use **Analyze folder loudness** to measure the average LUFS of the source material and set the target automatically.
4. Configure processing:
   - **Normalize volume** – Enables loudness normalization with the chosen target LUFS (for files ≥1 s) and target peak (for shorter files).
   - **Automixer** – Enables the full voice‑over processing chain. When active:
     - _Intelligent noise removal (Spectral Gate)_ – Custom FFT gate; mutually exclusive with _nnnoiseless_.
     - _Noise reduction (nnnoiseless)_ – RNNoise‑based denoiser; mutually exclusive with the spectral gate.
     - _Dereverb (DeepFilterNet3)_ – Runs before the gate/EQ. Mix level and optional post‑filter can be adjusted.
     - Voice EQ is always applied at 50% strength when Automixer is enabled.
5. Click **START PROCESSING**. Progress and log messages are displayed in the lower pane.

Processing can be stopped gracefully – the current file will finish, and subsequent files will be skipped.

## Technical Details

### Audio Pipeline (Automixer Mode)

1. **De‑esser** – FFmpeg `deesser` filter, parameters fixed for voice.
2. **DeepFilterNet3** (if enabled) – Joint denoise/dereverb at 48 kHz mono. The `-D` flag ensures sample‑accurate latency compensation.
3. **Spectral Gate / nnnoiseless** – Either the custom FFT expander or RNNoise runs at 48 kHz mono.
4. **Voice EQ** – Four‑band biquad (low shelf -3 dB @ 120 Hz, cut -1.5 dB @ 400 Hz, boost +2 dB @ 2.5 kHz, high shelf +1.5 dB @ 10 kHz). Strength parameter scales all gains.
5. **Post‑EQ & Dynamics** (FFmpeg)
   - 2‑pole + 1‑pole high‑pass at 70 Hz
   - Five parametric EQ bands
   - Compressor (threshold -12 dBFS, ratio 4:1, attack 5 ms, release 80 ms, makeup 4 dB)
6. **Normalization** – EBU R128 two‑pass with optional padding for short files, or peak normalization.
7. **Encoding** – IMA ADPCM WAV, preserving the original sample rate.

### Spectral Gate

- FFT size 2048, hop 512 (75% overlap), Hann window.
- Envelope follower with independent attack/release coefficients per frequency bin.
- Threshold defines expansion knee; ratio controls downward expansion slope below threshold.

### nnnoiseless

- Uses the `nnnoiseless` crate (RNNoise port).
- Input is scaled to 16‑bit integer range expected by the model.
- Dry/wet mix set to 0.75 (configurable via code).

### DeepFilterNet3 Integration

- Communicates via temporary WAV files using subprocess calls to `deep-filter.exe` and `ffmpeg`.
- Expects 48 kHz mono; output is mixed with the original signal according to the selected mix ratio.

### Loudness Analysis Fallback

| File Duration | Method                                                     |
| ------------- | ---------------------------------------------------------- |
| ≥ 3 s         | Standard two‑pass EBU R128                                 |
| 1 s … 3 s     | Two‑pass with silence padding to at least 5 s              |
| < 1 s         | Peak normalization to `target_peak_dbfs` (default -3 dBFS) |

If a file is completely silent or contains only negligible signal, it is converted without normalization and logged as _Skipped_.

## Logging and Error Handling

All operations are logged to the GUI. FFmpeg stderr is captured on failure and included in the error message. Temporary files are automatically cleaned up via `tempfile`; the DeepFilterNet3 output directory is also removed after processing.

## License

No explicit license is provided in the source files. All rights reserved by the original author unless otherwise noted.
