# VOCAN – Voice-Oriented Converter and Normalizer

VOCAN is a batch audio processing tool focused on preparing voice recordings for 4-bit ADPCM IMA WAV encoding. It provides hybrid loudness normalization (EBU R128) and peak‑based gain correction, automatically selecting the appropriate method based on file duration. A simple GUI built with **egui** guides the user through folder selection, analysis, and conversion.

## Features

- **Batch conversion** of any FFmpeg‑supported audio format to **ADPCM IMA WAV** (4‑bit).
- **Optional loudness normalization** targeting **LUFS-I** (EBU R128).
- **Automatic fallback strategies** for very short files (< 1 s) using **peak normalization**.
- **Silence padding** for files in the ~1–3 second range that otherwise yield `-inf` measurements.
- **Folder loudness analysis** to determine average LUFS and set a suitable target.
- **Detailed processing logs** with clear indication of the normalization method used.
- **Self‑contained**: only requires an FFmpeg binary placed next to the executable or available in `PATH`.

## How It Works

### Normalization Logic

When loudness normalization is enabled (`Normalize volume` checked), VOCAN applies the following decision tree for each input file:

1. **Standard EBU R128 two‑pass loudnorm**
   - Works for files ≥ ~3 seconds.
   - First pass measures integrated loudness (`measured_I`), true peak, and threshold.
   - Second pass applies linear gain with LRA=1.0 (narrow dynamic range) to reach the user‑defined target LUFS.

2. **Padded loudnorm for short files (1–3 seconds)**
   - If the standard measurement returns `-inf` (signal too short for gating), VOCAN checks duration.
   - Files ≥ 1 second are padded with silence to `max(5 s, duration+1 s)` for the **first pass only**.
   - Because EBU R128 gates silence below -70 LUFS, the measured values remain valid for the **original** unpadded signal.
   - Second pass runs on the original file using the statistics obtained from the padded measurement.

3. **Peak normalization for very short samples (< 1 second)**
   - `volumedetect` filter measures the file’s maximum peak (dBFS).
   - Gain is applied to bring the peak to a user‑defined `Target peak dBFS` (default -3 dBFS).
   - Gain is capped at +40 dB to avoid amplifying noise or silence.

4. **Fallback**
   - If even the padded measurement fails (e.g., truly silent or empty file), the file is simply converted to ADPCM WAV without any gain adjustment.

All output files are written to the chosen output directory, preserving the original folder structure. The final format is always **ADPCM IMA WAV** (4‑bit, mono or stereo as source), and the original sample rate is retained.

### Loudness Analysis

The _Analyze folder loudness_ feature scans an entire directory (recursively), measures the integrated loudness of each file using EBU R128, and displays the arithmetic mean. This helps decide a sensible target LUFS for a given set of recordings.

## Requirements

- **FFmpeg** (any recent static build). VOCAN will look for `ffmpeg` (or `ffmpeg.exe`) in:
  1. System `PATH`.
  2. The same directory as the VOCAN executable.
- **Rust toolchain** (only needed for building from source).

## Installation & Build

### Pre‑built Binary (Windows / Linux)

1. Download the latest release from the [Releases](https://github.com/JakubGrodzicki/VOCAN/releases/tag/Production) page.
2. Place `ffmpeg.exe` (Windows) or `ffmpeg` (Linux) in the same folder as `vocan`.
3. Run the executable.

### Building from Source

```bash
git clone [git@github.com:JakubGrodzicki/VOCAN.git](git@github.com:JakubGrodzicki/VOCAN.git)
cd vocan
cargo build --release
```

The binary will be located in `target/release/vocan` (or `vocan.exe`). Make sure FFmpeg is either on `PATH` or copied next to the binary.

## Usage

1. **Source** – select the folder containing the audio files you want to process.
2. **Output** – choose where the converted `.wav` files will be saved (folder structure is replicated).
3. **Loudness**:
   - Check **Normalize volume** to enable loudness processing.
   - Optionally run **Analyze folder loudness** on your source folder to get an average LUFS value; click **Set as target** to adopt it.
   - Adjust **Target LUFS-I** (applies to files ≥ 1 s).
   - Adjust **Target peak dBFS** (applies only to files < 1 s).
4. Click **START PROCESSING**.
5. Progress and per‑file normalization methods are displayed in the log area.

## Technical Notes

- **LRA = 1.0** is hard‑coded for the `loudnorm` filter. This narrow dynamic range reduces quantization noise when encoding to 4‑bit ADPCM, making it particularly suitable for voice‑over and dialog.
- **True peak target** is fixed at -1.5 dBFS during `loudnorm` passes to prevent clipping after resampling.
- The original sample rate is explicitly restored (`-ar`) after `loudnorm` processing, as EBU R128 internally resamples to 192 kHz.
- **Peak normalization gain** is limited to +40 dB. A higher value usually indicates a silent or invalid file, which will be skipped from gain adjustment.

## License

VOCAN is provided under the **MIT License**.
FFmpeg is distributed under its own licenses; please refer to [ffmpeg.org](https://ffmpeg.org).

## Contributing

Bug reports and pull requests are welcome. Please open an issue first to discuss significant changes.
