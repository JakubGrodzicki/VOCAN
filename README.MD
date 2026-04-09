# VOCAN: Voice-Over Compression and Normalization

A utility for batch converting audio into 4-bit ADPCM WAV files. I got tired of manually running FFmpeg commands for hundreds of voice-over assets, so I wrapped the process into a basic GUI.

It handles recursive directory scanning and implements a two-pass `loudnorm` filter to ensure consistent volume across files.

---

## Requirements

- **FFmpeg** — Must be installed and accessible in your system PATH. The app calls `ffmpeg` directly for analysis and encoding.
- **Rust** — Built using the 2021 edition.

---

## Features

- **Recursive Processing** — Uses `walkdir` to find every file in your source tree.
- **Loudness Analysis** — Scans a selected folder to calculate the average LUFS. You can use this average as your target to keep the original "feel" of the session while leveling out the peaks.
- **Two-Pass Normalization** — If enabled, it performs an initial scan to gather integrated loudness, LRA, and true peak stats before applying the final filter.
- **ADPCM Encoding** — Specifically targets the `adpcm_ima_wav` codec.

---

## Usage

1. **Paths** — Select your source directory and an output directory.
2. **Loudness** — Either set a manual target LUFS or run the folder analysis to see what you're working with.
3. **Process** — Hit the start button. The UI tracks progress and logs any FFmpeg errors to the bottom panel.

---

## Technical Notes

The app uses `mpsc` channels to keep the UI responsive while FFmpeg runs in the background. If the process hangs, it's almost certainly because FFmpeg is waiting for a prompt it can't show you, or your paths contain characters that your OS doesn't like.

---

## Dependencies

| Crate             | Purpose                                          |
| ----------------- | ------------------------------------------------ |
| `eframe` / `egui` | Interface                                        |
| `rfd`             | File dialogs                                     |
| `serde_json`      | Parsing loudness statistics from FFmpeg's stderr |
| `anyhow`          | Error handling without boilerplate               |
