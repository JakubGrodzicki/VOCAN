# VOCAN

VOCAN is a tool for batch audio processing. Voice-over teams use VOCAN to prepare audio files for games and other embedded playback systems.

VOCAN applies these processes to each audio file:

- Dynamics processing
- Equalization
- Noise reduction
- Dereverberation
- Loudness normalization

VOCAN writes the output files in a format that you select. The default format is 4-bit IMA ADPCM WAV. VOCAN can also write PCM, FLAC, MP3, and OGG files.

VOCAN is written in Rust. The graphical interface uses the `egui` library. VOCAN sends the audio encoding tasks to FFmpeg and, for some tasks, to external DSP tools. VOCAN also has its own DSP modules for the spectral gate, the equalizer, and the expander.

VOCAN scans folders and processes files in parallel. VOCAN uses a Rayon thread pool. The size of the pool matches the number of CPU cores on the host computer, minus one.

## Features

### Loudness normalization (optional)

VOCAN can normalize the loudness of each file. VOCAN uses this method:

1. If the file is 3 seconds or longer, VOCAN measures the EBU R128 integrated loudness (LUFS). VOCAN uses a two-pass linear method.
2. If step 1 fails, or if the file is shorter than 3 seconds, VOCAN adds silence to a copy of the file. VOCAN then repeats the two-pass measurement on this copy.
3. If step 2 fails, VOCAN uses peak normalization instead. You can set the target peak level.
4. If step 3 also fails, VOCAN does not normalize the file. VOCAN converts the file without normalization and writes "Skipped" in the log.

A loudness measurement fails when the file is silent or has very little signal.

### Silence trimming (optional)

VOCAN can cut the dead air off the front and the back of every take. Select the **Trim silence** checkbox, in the **Files** section.

This option is independent of everything else. It does not belong to the Automixer chain and the Automixer master switch does not control it. You can use it on its own, with only the format conversion.

VOCAN never touches the pauses **inside** a line. A dramatic pause, a breath, or a gap between sentences comes through untouched. VOCAN only removes silence that runs from the very start of the file to the first sound, and from the last sound to the very end.

**Silence below.** This setting decides what counts as silence. There is no free-form dB entry, because the right number depends on the noise floor of your own recordings, which you cannot read off a waveform by eye. Pick a preset instead:

| Preset | Threshold | What it does |
| --- | --- | --- |
| Safe | −60 dB | Cuts only silence close to digital zero. On a real room recording this often trims nothing at all. |
| Recommended | −45 dB | The default. Right for clean studio material. |
| Hard | −32 dB | Copes with an untreated room. Can eat a quiet breath. |
| MAX | −21 dB | Cuts quiet consonants and the tails of words, not just the dead air. |

If a preset is too low for your material, the silence in your files is louder than the threshold, so VOCAN finds no silence and trims nothing. That is the safe way to be wrong, but it does mean "nothing happened" is a real answer. Move one preset up.

**Keep.** This setting decides how much of the original silence stays at each end: Tight (0 ms), Short (0.25 s), Medium (0.5 s), or Long (1 s). Tight is the default and cuts at the first and last sample above the threshold.

Keep is a **ceiling, not a target**. VOCAN keeps up to this much of the silence that was already in the file and never adds any. A take with 0.3 s of lead-in keeps 0.3 s even on Long.

**Cost.** Trimming launches no extra processes. VOCAN adds the filter to a chain that FFmpeg was going to run anyway, so a trimmed run launches exactly as many processes and reads each file exactly as many times as an untrimmed one.

If nothing in a file reaches the threshold, nothing would survive the trim. With the Automixer on, VOCAN reports the file as an error and writes no output for it, rather than publishing an empty audio file that looks like a successful conversion. Without the Automixer, VOCAN cannot see the signal in memory and so cannot make that check without spending another FFmpeg process on every file in the batch; such a file is written out empty.

### Automixer chain (optional)

The Automixer applies these processes, in this order:

1. De-esser. This step uses the FFmpeg `deesser` filter.
2. Dereverberation, with DeepFilterNet3. This step also reduces noise.
3. Downward expansion. This step uses a Rust module. The module finds the noise floor and applies bounded expansion.
4. Noise reduction. Select one of these two methods:
   - The spectral gate. This is a Rust module, based on the FFT.
   - `nnnoiseless`. This method is based on RNNoise.
5. Voice EQ. This step uses a static bank of biquad filters. The bank has a low-shelf cut, two parametric cuts, and a high-shelf boost. VOCAN applies the EQ at 50% strength.
6. Post-processing with FFmpeg: a high-pass filter, a six-band parametric EQ, and a compressor with makeup gain.

### Output formats

VOCAN can write these formats:

- 4-bit IMA ADPCM WAV. This is the default format. Use this format for game voice-over.
- 16-bit PCM WAV.
- 24-bit PCM WAV. This is the standard bit depth for professional audio work.
- FLAC. This format uses lossless compression.
- MP3. You can set the bitrate, from 64 to 320 kbps.
- OGG Vorbis. You can set the bitrate, from 64 to 320 kbps.

### Analysis mode

VOCAN can scan a folder and calculate the average LUFS value of all audio files in the folder. You can apply this value as the loudness target with one click.

### Parallel and resilient processing

VOCAN processes files in parallel. If a file causes an error, VOCAN logs the error and continues with the other files. You can stop a run at any time. VOCAN first finishes the files that are in progress. VOCAN then skips the remaining files and stops.

## Requirements

VOCAN needs FFmpeg. Do one of these two things:

- Add FFmpeg to the system PATH.
- Put the FFmpeg executable file in the same folder as the VOCAN executable file.

On Windows, the FFmpeg file name is `ffmpeg.exe`. On Linux and macOS, the file name is `ffmpeg`.

DeepFilterNet3 is optional. VOCAN needs DeepFilterNet3 only when you enable the dereverb option. Get the `deep-filter` executable file from the [DeepFilterNet releases page](https://github.com/Rikorose/DeepFilterNet/releases). Put this file in the same folder as the FFmpeg executable file. On Windows, the file name is `deep-filter.exe`.

The setup script in the next section can install all these requirements for you.

## Build Instructions

### Quick setup (recommended)

VOCAN has one setup script for macOS and Linux, and one setup script for Windows. Each script does all these steps:

1. Detects your operating system and CPU architecture.
2. Installs Rust, if it is missing.
3. Installs FFmpeg, if it is missing.
4. Downloads DeepFilterNet3 and places it next to the VOCAN binary.
5. Builds VOCAN in release mode.
6. Runs the automated test suite, to confirm that the build works.

Do these steps on macOS or Linux:

```bash
git clone https://github.com/JakubGrodzicki/VOCAN.git
cd VOCAN
chmod +x installMacLinux.sh
./installMacLinux.sh
```

Do these steps on Windows, in PowerShell:

```powershell
git clone https://github.com/JakubGrodzicki/VOCAN.git
cd VOCAN
.\installWindows.ps1
```

The script prints the location of the finished binary at the end.

To skip the DeepFilterNet3 download, add the flag `--no-dfn3` (macOS/Linux) or `-NoDfn3` (Windows). To skip the test suite, add the flag `--skip-tests` (macOS/Linux) or `-SkipTests` (Windows).

**Note about DeepFilterNet3 and downloaded-file warnings:** the script downloads the `deep-filter` executable file from its official GitHub releases page and clears the "downloaded file" flag from your operating system, so that the file can run. On macOS, this flag is the Gatekeeper quarantine attribute. On Windows, this flag is the Mark of the Web. This is the same action you would take yourself, through your file manager, to run a downloaded executable file.

### Manual build

If you prefer not to use the setup script, do these steps:

1. Clone the repository.

   ```bash
   git clone https://github.com/JakubGrodzicki/VOCAN.git
   cd VOCAN
   ```

2. Build the release binary.

   ```bash
   cargo build --release
   ```

3. Find the binary at `target/release/VOCAN`. On Windows, the file name is `VOCAN.exe`.

`Cargo.toml` lists all dependencies. VOCAN uses these main dependencies:

- `eframe` and `egui`, for the graphical interface
- `rfd`, for native file dialogs
- `rustfft`, `biquad`, and `nnnoiseless`, for the custom DSP modules
- `hound`, to read and write WAV files
- `tempfile`, to manage temporary files
- `rayon` and `num_cpus`, for parallel processing
- `anyhow`, for error handling
- `serde` and `serde_json`, to parse the FFmpeg `loudnorm` JSON output
- `walkdir`, to scan folders

## Usage

Do these steps to process a folder of audio files:

1. Start the application.
2. Set the source folder and the output folder.
3. To measure the average loudness of the source files, click **Analyze folder loudness...**. To apply this value as the target, click **Set as target**.
4. To enable loudness normalization, select the **Normalize volume** checkbox. Set the target LUFS value and the target peak dBFS value. VOCAN uses the target peak value only as a fallback, when the LUFS measurement fails.
5. Select an output format. If you select MP3 or OGG, also set the bitrate.
6. To cut the silence off the start and the end of every take, select the **Trim silence** checkbox. This option sits in the **Files** section, with the other settings that apply to every file. It works on its own — you do not need to enable the Automixer for it. Then set:
   - **Silence below**: what counts as silence. Safe (−60 dB), Recommended (−45 dB), Hard (−32 dB), or MAX (−21 dB). If the trim seems to do nothing, your recordings are noisier than the preset expects — move one step up.
   - **Keep**: how much of the original silence stays at each end. Tight (0 ms), Short (0.25 s), Medium (0.5 s), or Long (1 s).
7. To enable the Automixer, select the **Automixer** checkbox. Then set the Automixer options:
   - **Intelligent noise removal (Spectral Gate)**: select only this option, or only the nnnoiseless option, not both.
   - **Noise reduction (nnnoiseless)**: select only this option, or only the spectral gate option, not both.
   - **Dereverb (DeepFilterNet3)**: this step runs before the noise-reduction step and the EQ step. You can set the mix level (0–100%). You can also enable the **Post-filter** option — this applies more aggressive noise suppression in the noisiest parts of the signal. Use post-filter for outdoor or field recordings; leave it off for clean studio recordings to preserve naturalness.
   - **Downward Expander (noise floor)**: this option estimates the noise floor from the signal before the dereverb step. The option then applies bounded downward expansion. Set the **Safety Margin** slider, from 0% to 100%. This value sets the distance between the noise floor and the expansion threshold. Select a **Reduction profile** (also called "depth"): Safe, Recommended, Hard, or Max. Each profile defines the maximum attenuation depth — how much the signal gets reduced when below the threshold. Higher profiles (Hard, Max) are more aggressive but can sound less natural.
   - When Automixer is on, VOCAN always applies the Voice EQ, at 50% strength.
8. Click **START PROCESSING**. VOCAN shows the progress and the log messages in the lower pane.

To stop a run, click **Stop**. VOCAN finishes the files that are in progress, then skips the remaining files.

## Automated Tests

VOCAN has an automated test suite. Run this command to run the fast tests:

```bash
cargo test
```

The fast tests check the DSP math, the loudness-normalization logic, and the application state. These tests do not need FFmpeg.

Run this command to run the full test suite:

```bash
cargo test -- --ignored
```

The full test suite needs a real FFmpeg executable on the PATH. These tests generate WAV files, then process the files through the real pipeline.

The full test suite also includes tests for the DeepFilterNet3 dereverb feature. These tests need a `deep-filter` executable file next to FFmpeg. The setup script in "Build Instructions" installs this file for you. If VOCAN cannot find the file, these tests print a skip notice and pass without an error, so the full test suite still works on a computer without DeepFilterNet3.

`TESTING.md` has a manual test checklist for the graphical interface.

## Technical Details

### Audio pipeline (Automixer mode)

When Automixer is on, each file goes through a pipeline of Rust steps and FFmpeg steps. Every Rust DSP step uses mono, 32-bit float audio.

VOCAN runs the Rust chain at the **source sample rate**. Two steps are the exception: nnnoiseless and DeepFilterNet3 are models trained at 48 kHz and require it. When you enable either one, VOCAN converts the whole Rust chain to 48 kHz instead. VOCAN restores the original sample rate at the final encoding step in every case. This means a 44.1 kHz file with neither of those two steps enabled is never resampled at all.

1. **De-esser**: the FFmpeg `deesser` filter (`i=0.4:m=0.5:f=0.5:s=o`) processes the file. FFmpeg sends the output to Rust, as mono f32le audio at the pipeline rate described above. If nothing comes back at all — the file was silent from end to end, and the trim removed all of it — VOCAN fails that file with an error instead of writing an empty output.
2. **Noise floor estimation**: this step runs only when the expander is on. VOCAN measures the windowed RMS level of the signal. The window is 25 ms long, with 50% overlap and a Hann shape. VOCAN normalizes the windowed energy by the sum of squares of the window itself, so the result is a true RMS level. VOCAN takes the 12th percentile of the result. This step runs on the signal after the de-esser step and before the dereverb step. This order stops the dereverb step from changing the noise floor estimate. If the file is too short or too quiet, VOCAN skips the expander step for that file.
3. **DeepFilterNet3**: this step runs only when dereverb is on. The model removes noise and reverb at the same time, at 48 kHz mono. The `-D` flag aligns the output with the input, sample for sample. VOCAN reads and writes the WAV data with the `hound` library. This step does not need an extra FFmpeg process. VOCAN mixes the output with the dry signal, at the ratio that you set. The default attenuation limit is 30 dB. You can enable an optional **post-filter** (`--pf`), which applies more aggressive suppression to highly noisy regions — this is useful for outdoor or field recordings but may reduce naturalness in clean recordings.
4. **Downward Expander**: this step runs only when the expander is on. The level detector uses an RMS window of about 3 ms. The expander has a soft knee, attack and release smoothing, and a mandatory hold time. The hold time stops the expander from opening and closing too fast. The threshold is the noise floor minus the margin. The Safety Margin slider sets the margin, from 2 dB to 8 dB. The reduction profile (also called "depth") sets the maximum attenuation — this is how much the signal gets reduced below the threshold. The attenuation never reaches −∞ dB; instead, each profile applies a bounded reduction.

   | Profile | Maximum attenuation |
   | --- | --- |
   | Safe | −8.0 dB |
   | Recommended | −12.0 dB |
   | Hard | −18.0 dB |
   | Max | −32.0 dB |

   **Note on calibration:** before this was corrected, the noise floor estimate read 4.26 dB lower than the true level, because it normalized the Hann-windowed energy by the sample count instead of by the window's sum of squares. The threshold sat that much lower too, so the Safety Margin slider behaved as though it spanned 6.3 dB to 12.3 dB. The margin now matches the documented 2 dB to 8 dB. If you tuned the slider against an older build, expect the expander to act more strongly at the same setting, and lower the slider to compensate.

5. **Spectral Gate or nnnoiseless**: select one of these two options. The spectral gate runs at the pipeline rate. nnnoiseless always runs at 48 kHz.
6. **Voice EQ**: a four-band biquad filter processes the signal. The bands are: a low shelf at 120 Hz (−3 dB), a cut at 400 Hz (−1.5 dB), a boost at 2.5 kHz (+2 dB), and a high shelf at 10 kHz (+1.5 dB). VOCAN always applies this filter at 50% strength.
7. **Post-EQ and dynamics** (FFmpeg):
   - A 2-pole high-pass filter and a 1-pole high-pass filter, both at 70 Hz.
   - Five parametric EQ bands, at 90, 175, 360, 1350, and 4246 Hz.
   - A high shelf at 8 kHz.
   - A compressor. The threshold is about −12 dBFS. The ratio is 4:1. The attack time is 5 ms. The release time is 80 ms. The makeup gain is 4 dB.
   - The **silence trim**, if it is on. It comes last in this chain, so the threshold judges the signal after every cleaning step has finished with it. See "Silence trimming" below.
8. **Normalization**: VOCAN uses the two-pass linear EBU R128 method, with padding for short files, or peak normalization. See "Loudness normalization fallback" below. VOCAN measures the pass-1 stats on the processed temporary WAV file. This method keeps the pass-1 measurement and the pass-2 normalization on the same signal.
9. **Encoding**: VOCAN writes the file in the format that you selected. VOCAN restores the original sample rate.

### Silence trimming

VOCAN builds this filter chain, and gives it to FFmpeg as part of a chain that already runs. `THRESHOLD` comes from the **Silence below** preset and `SILENCE` from the **Keep** preset; shown here at the defaults, Recommended and Tight:

```
silenceremove=start_periods=1:start_duration=0.05:start_threshold=-45dB:start_silence=0.05,
areverse,
silenceremove=start_periods=1:start_duration=0.05:start_threshold=-45dB:start_silence=0.05,
areverse
```

**Why it is built this way.** The recipe that circulates for this job is a single `silenceremove` with a start section and a stop section:

```
silenceremove=start_periods=1:start_duration=0.05:start_threshold=-45dB:
              stop_periods=1:stop_duration=0.35:stop_threshold=-45dB
```

That recipe does not trim the end of a file. A positive `stop_periods` does not mean "trim the tail" — it means "stop copying at the Nth silence", and FFmpeg then discards everything after that point. Measured on a 3.5-second test file (1 s of silence, 0.5 s of tone, a 0.5 s pause, 0.5 s of tone, 1 s of silence), that recipe returns **0.82 seconds**: it cut the file at the pause and threw the second half away. A negative `stop_periods` is no better — it deletes the pauses inside the line as well. Any voice-over take with a beat in the middle of it comes back mutilated, and nothing in the output says so.

VOCAN therefore trims the tail the same way it trims the head: it reverses the stream, runs the same head trim again, and reverses back. Both ends get identical treatment, and neither pass ever looks past the first sound it finds, so pauses inside the line survive by construction. The same 3.5-second test file returns **1.5 seconds** — both bursts and the beat between them.

**Why `start_silence`.** `start_duration=0.05` is the detection window: 50 ms of sound has to arrive before the filter accepts that the take has begun, so a single click above the threshold cannot anchor the trim. Without `start_silence`, FFmpeg *consumes* the audio it used to make that decision, and every take loses the first 50 ms of its own onset — on a plosive, that is the whole plosive.

So `start_silence` is never below 0.05: that floor cancels the detection window out, which is what makes the Tight preset an exact cut rather than a 50 ms bite. The **Keep** preset is added on top of the floor, and the mapping is exactly linear — Tight is `start_silence=0.05`, Short is `0.30`, Medium is `0.55`, Long is `1.05`. A file with no silence at either end comes through Tight sample for sample.

**Where it runs.** With the Automixer off, the trim is the whole `-af` chain. There is no cleaning chain for it to sit behind, so the threshold meets the raw recording — this is the case the presets exist for.

With the Automixer on, the trim goes **last**, appended to the end of the second FFmpeg pass, after the post-EQ and the compressor and before the loudness normalization. Trimming first would be cheaper, because every Rust DSP step would then see a shorter signal. It would also ask the threshold to tell speech from silence on the noisiest version of the audio that exists, before dereverb, the expander and the spectral gate have taken the noise out. On an untreated room that is exactly when the trim finds no silence and does nothing, so VOCAN pays for the reliability instead.

In both cases the trim is part of the filter chain that the loudness measurement pass uses as well as the encoding pass, so the signal that is measured is the signal that is normalized.

`areverse` buffers the whole stream inside FFmpeg. For voice-over takes this costs nothing. It is worth knowing before pointing VOCAN at hour-long files with this option on.

**One note on the interaction with other steps.** The 3-second threshold that picks the first loudness measurement is read from the untrimmed length, because that is all a probe can report without an extra process. It only decides which measurement is tried first, and the padded pass behind it catches any file that turns out to be too short.

### Non-Automixer mode

When Automixer is off, FFmpeg processes the file directly. FFmpeg applies the silence trim, if it is on, and then only the loudness step or the peak-normalization step, then encodes the file. VOCAN does not run the Rust DSP modules. VOCAN does not create temporary files.

### Spectral Gate

- The FFT size is 2048 samples. The hop size is 512 samples, for 75% overlap. VOCAN uses a Hann window.
- VOCAN pads the signal with one FFT frame of silence at each end before the analysis. Without this padding, the samples near each end receive only partial overlap-add: the file starts with an audible fade-in, and up to 2047 samples at the end (about 43 ms at 48 kHz) are never written and stay silent.
- An envelope follower tracks each frequency bin. Each bin has its own attack rate and release rate.
- The threshold sets the expansion knee. The ratio sets the slope of the expansion below the threshold.

### nnnoiseless

- VOCAN uses the `nnnoiseless` crate. This crate is a Rust port of RNNoise.
- VOCAN scales the input to the 16-bit integer range that the model expects.
- The default dry/wet mix is 0.75. VOCAN compensates for one RNNoise frame of latency in the output.

### DeepFilterNet3 integration

- VOCAN sends the audio to the `deep-filter` executable through temporary WAV files. VOCAN looks for this executable next to FFmpeg, or next to the VOCAN executable.
- The model expects 48 kHz mono audio. VOCAN mixes the output with the original signal, at the ratio that you set.
- VOCAN reads and writes the WAV files with the `hound` library.

### Loudness normalization fallback

| File duration | Method |
| --- | --- |
| 3 seconds or longer | Standard two-pass EBU R128 |
| Shorter than 3 seconds | Two-pass EBU R128, with silence padding to at least 5 seconds |
| Last resort | Peak normalization, to the target peak dBFS. The default target is −3 dBFS. The gain cannot exceed 40 dB. |

VOCAN always uses the padded method for files shorter than 3 seconds. This method gives the FFmpeg `loudnorm` filter enough time to measure the EBU R128 loudness correctly.

The `loudnorm` filter has a silence gate at −70 LUFS. This gate excludes silence from the measurement. Pass 2 always normalizes the original file, without padding.

## Logs and Errors

VOCAN writes all log messages to the graphical interface. If FFmpeg fails, VOCAN captures the FFmpeg error output and adds it to the log message. If one file causes a panic, VOCAN catches the panic and logs it. The panic does not stop the worker pool.

VOCAN uses the `tempfile` crate to manage temporary files. VOCAN deletes these files automatically. VOCAN also deletes the DeepFilterNet3 output folder after each run.

## License

The source files do not state a license. All rights belong to the original author, unless another document states otherwise.
