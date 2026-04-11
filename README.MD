# VOCAN

VOCAN is a cross-platform graphical tool for batch processing voice-over and dialogue audio files. It applies a hybrid normalization strategy (EBU R128 loudness normalization and peak-based fallback for short samples), an optional fixed processing chain (EQ, de-esser, compressor), and converts the result to 4-bit ADPCM IMA WAV. The tool is designed to streamline preparation of assets for game engines, interactive applications, or constrained storage environments where low-bitrate audio is required.

## Features

- **Batch audio processing** – Recursively traverses a source directory and processes all supported audio files.
- **Intelligent loudness normalization**
  - Two-pass EBU R128 loudnorm for files ≥ ~3 seconds.
  - Two-pass loudnorm with temporary silence padding for files between 1 and ~3 seconds.
  - Peak normalization for files under 1 second (configurable target peak, capped at +40 dB gain).
  - Silent or empty files are converted without normalization.
- **Optional "Automixer" chain** – A predefined signal chain applied before normalization:
  - High-pass filter (70 Hz, 18 dB/octave)
  - Six-band parametric EQ to clean up mud, boxiness, harshness, and add presence/air
  - De-esser targeting 6–8 kHz sibilance
  - Compressor (threshold -12 dBFS, 4:1 ratio, 5 ms attack, 80 ms release, 4 dB makeup)
- **Integrated loudness analysis** – Analyze a folder to determine the average integrated loudness (LUFS-I) of its audio files. The result can be used as the target normalization level.
- **Stop/resume support** – Gracefully cancels processing after the current file finishes.
- **Output format** – ADPCM IMA WAV (4-bit), a widely supported compressed PCM format ideal for embedded systems and older game engines.
- **Cross-platform** – Runs on Windows, macOS, and Linux. Bundled `ffmpeg` detection prioritizes system PATH, falling back to a binary placed alongside the executable.

## How It Works

VOCAN relies on `ffmpeg` for all audio operations. The core logic:

1. **Source discovery** – Walks the input directory and identifies files by extension (`.wav`, `.mp3`, `.flac`, `.ogg`, `.aiff`, `.m4a`, etc.).
2. **Sample rate preservation** – Extracts the original sample rate before processing because `loudnorm` internally resamples to 192 kHz. The final ADPCM WAV is written at the original rate.
3. **Normalization strategy**
   - If volume normalization is disabled, only the automixer chain (if enabled) is applied before encoding.
   - If enabled:  
     a. Attempt standard two-pass `loudnorm` with the user-defined target LUFS.  
     b. If the first pass returns `-inf` (signal too short or silent), check file duration:
     - Duration ≥ 1 s → re-run first pass with silence padding (`apad` + `atrim`), then second pass on the unpadded original.
     - Duration < 1 s → measure peak amplitude with `volumedetect` and apply a linear gain to reach the desired peak dBFS (default -3.0).  
       c. Extreme fallback: convert without normalization if the signal remains inaudible even after padding.
4. **Encoding** – The processed stream is encoded to `adpcm_ima_wav` and written to the output folder, preserving the relative path structure.

### Automixer Chain Details

The automixer is a hardcoded filter graph meant to be a "closest-to-universal" voice-over cleanup preset. The parameters are:

- HPF 70 Hz (12 dB/oct + 6 dB/oct cascade → 18 dB/oct, Q=1.0 on the resonant stage)
- Bell 90 Hz, Q 2.478, -2.0 dB
- Bell 175 Hz, Q 1.0, -2.22 dB
- Bell 360 Hz, Q 1.0, -1.23 dB
- Bell 1350 Hz, Q 1.4, +1.4 dB
- Bell 4246 Hz, Q 2.0, -1.36 dB
- High shelf 8000 Hz, Q 1.0, +1.0 dB
- De‑esser: `deesser=i=0.4:m=0.5:f=0.5:s=o`
- Compressor: `acompressor=threshold=0.251:ratio=4:attack=5:release=80:makeup=4`

All filters are implemented using native FFmpeg filters; the chain is applied before any normalization step.

## Building from Source

### Prerequisites

- Rust toolchain (stable, edition 2021)
- Cargo
- FFmpeg binary (see [FFmpeg Requirements](#ffmpeg-requirements))

### Build Steps

```bash
git clone git@github.com:JakubGrodzicki/VOCAN.git
cd vocan
cargo build --release
```

The executable will be located at `target/release/vocan` (or `vocan.exe` on Windows).

### FFmpeg Requirements

VOCAN expects an `ffmpeg` binary to be available. It searches in two locations:

1. The system `PATH`.
2. The directory containing the VOCAN executable (`ffmpeg.exe` on Windows, `ffmpeg` on Unix-like systems).

If neither is found, the application will start but all processing operations will fail. It is recommended to obtain a recent static build of FFmpeg (e.g., from [ffmpeg.org](https://ffmpeg.org/download.html) or a package manager).

## Usage

1. Launch the application.
2. Set the **Source** folder containing your audio files.
3. Set the **Output** folder where processed files will be written.
4. (Optional) Click **Analyze folder loudness...** to measure the average LUFS of a directory. You can then adopt that value as the target.
5. Enable **Normalize volume** and adjust the target LUFS and peak threshold for short samples.
6. Enable **Automixer** if you want the EQ + de‑esser + compressor chain applied.
7. Click **START PROCESSING**.

Progress and detailed per-file logs are displayed in the lower panel. You may click **Stop** to gracefully cancel after the current file completes.

## Supported Audio Formats

Any format that FFmpeg can decode is supported. The following extensions are explicitly recognized during directory scanning:

`wav`, `wave`, `mp3`, `flac`, `aiff`, `aif`, `ogg`, `opus`, `m4a`, `aac`, `wma`, `mp2`, `ac3`, `dts`, `mka`

Files with other extensions are ignored to avoid processing sidecar metadata.

## Caveats and Limitations

- **Silence padding for short files** uses a fixed duration of max(5.0, file_duration+1.0) seconds. This may temporarily increase memory usage during analysis but does not affect the final output length.
- **Peak normalization gain is capped at +40 dB** to prevent extreme amplification of near‑silent signals.
- **Automixer chain is fixed** – parameters are not user‑configurable. It is intended as a convenience for quickly cleaning voice‑over recordings without manual mixing.
- **ADPCM IMA WAV** is a 4‑bit format. While suitable for speech and sound effects, it introduces quantization noise. The loudness normalization with LRA=1.0 (narrow dynamic range) helps mitigate perceived noise.

## License

This project is distributed under the MIT License. See the `LICENSE` file for details (if included). The `ffmpeg` binary is subject to its own license terms (LGPL/GPL).

## Contributing

Bug reports, feature suggestions, and pull requests are welcome. Please ensure that any changes maintain compatibility with the existing FFmpeg filter chain and the hybrid normalization logic.
