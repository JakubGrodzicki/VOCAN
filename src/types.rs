use serde::Deserialize;
use std::sync::mpsc::Sender;

// ---------------------------------------------------------------------------
// Expander Reduction Profile
// ---------------------------------------------------------------------------

/// Preset reduction depth for the downward expander.
///
/// Each variant maps to a fixed maximum attenuation in dB. There is no
/// free-form numeric input — the expander is bounded, never a true gate to -inf.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ReductionProfile {
    /// -8.0 dB — gentle, barely audible.
    Safe,
    /// -12.0 dB — balanced default.
    #[default]
    Recommended,
    /// -18.0 dB — aggressive but still bounded.
    Hard,
    /// -32.0 dB — outlier; can start sounding like a hard gate in practice.
    Max,
}

impl ReductionProfile {
    /// Returns the maximum attenuation in dB for this profile.
    pub fn db(self) -> f32 {
        match self {
            Self::Safe => -8.0,
            Self::Recommended => -12.0,
            Self::Hard => -18.0,
            Self::Max => -32.0,
        }
    }

    /// Human-readable label for the UI dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "Safe (-8.0 dB)",
            Self::Recommended => "Recommended (-12.0 dB)",
            Self::Hard => "Hard (-18.0 dB)",
            Self::Max => "MAX (-32.0 dB)",
        }
    }

    /// All variants in dropdown order.
    pub fn all() -> &'static [ReductionProfile] {
        &[Self::Safe, Self::Recommended, Self::Hard, Self::Max]
    }
}

// ---------------------------------------------------------------------------
// Silence Trim Presets
// ---------------------------------------------------------------------------

/// What counts as silence when trimming the ends of a take.
///
/// There is no free-form dB entry here, for the same reason the expander has
/// none: the number is only meaningful relative to a given recording's own
/// noise floor, which nobody can read off a waveform by eye. Set it too low and
/// the trim silently does nothing; too high and it eats the first word. Four
/// presets, ordered by how much they risk, is a question a user can answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SilenceThreshold {
    /// -60.0 dB — only silence close to digital zero. On a real room
    /// recording this often trims nothing at all, which is the safe way to be
    /// wrong.
    Safe,
    /// -45.0 dB — balanced default, right for clean studio material.
    #[default]
    Recommended,
    /// -32.0 dB — copes with an untreated room; can eat a quiet breath.
    Hard,
    /// -21.0 dB — outlier; cuts quiet consonants and the tails of words.
    Max,
}

impl SilenceThreshold {
    /// Returns the threshold in dB below which audio is treated as silence.
    pub fn db(self) -> f32 {
        match self {
            Self::Safe => -60.0,
            Self::Recommended => -45.0,
            Self::Hard => -32.0,
            Self::Max => -21.0,
        }
    }

    /// Human-readable label for the UI dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "Safe (-60 dB)",
            Self::Recommended => "Recommended (-45 dB)",
            Self::Hard => "Hard (-32 dB)",
            Self::Max => "MAX (-21 dB)",
        }
    }

    /// All variants in dropdown order.
    pub fn all() -> &'static [SilenceThreshold] {
        &[Self::Safe, Self::Recommended, Self::Hard, Self::Max]
    }
}

/// How much of the original silence to leave at each end of a trimmed take.
///
/// A ceiling, not a target. The filter keeps *up to* this much of the silence
/// that was already in the file and never manufactures any, so a take carrying
/// 0.3 s of lead-in keeps 0.3 s even on [`Self::Long`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SilencePad {
    /// 0 ms — cut at the first and last sample above the threshold.
    #[default]
    Tight,
    /// 0.25 s.
    Short,
    /// 0.5 s.
    Medium,
    /// 1 s.
    Long,
}

impl SilencePad {
    /// Returns the amount of silence kept at each end, in seconds.
    pub fn secs(self) -> f32 {
        match self {
            Self::Tight => 0.0,
            Self::Short => 0.25,
            Self::Medium => 0.5,
            Self::Long => 1.0,
        }
    }

    /// Human-readable label for the UI dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tight => "Tight (0 ms)",
            Self::Short => "Short (0.25 s)",
            Self::Medium => "Medium (0.5 s)",
            Self::Long => "Long (1 s)",
        }
    }

    /// All variants in dropdown order.
    pub fn all() -> &'static [SilencePad] {
        &[Self::Tight, Self::Short, Self::Medium, Self::Long]
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct LoudnormStats {
    pub input_i: String,
    pub input_tp: String,
    pub input_lra: String,
    pub input_thresh: String,
    pub target_offset: String,
}

/// Describes the normalization method used for a file -- used for logging.
#[derive(Debug, PartialEq)]
pub enum NormResult {
    /// Standard 2-pass EBU R128 (files >= 3s).
    Standard,
    /// 2-pass EBU R128 with silence padding (files < 3s, or files >= 3s whose
    /// standard pass returned -inf/invalid).
    Padded,
    /// Peak normalization fallback, used only when EBU R128 measurement
    /// (standard or padded) failed to produce a valid loudness value.
    Peak { gain_db: f32 },
    /// Conversion without normalization (extreme fallback -- silent or empty signal,
    /// or peak gain would exceed the 40dB safety cap).
    Skipped,
}

pub enum AppMsg {
    Log(String),
    Progress(usize, usize),
    Error(String),
    Finished,
    Stopped,
    AnalysisResult(f32),
}

// ---------------------------------------------------------------------------
// Output Format
// ---------------------------------------------------------------------------

/// Output container/codec selection.
///
/// `AdpcmWav` is the default and the original behaviour of VOCAN -- it produces
/// 4-bit IMA ADPCM WAV files, the smallest practical format for game voice-over.
///
/// The other variants let users re-normalize or export without the lossy 4-bit
/// quantization step, or pick a different distribution format.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OutputFormat {
    /// 4-bit IMA ADPCM WAV -- suggested for video game voice-over (default).
    #[default]
    AdpcmWav,
    /// 16-bit PCM WAV -- universal compatibility.
    Pcm16Wav,
    /// 24-bit PCM WAV -- industry-standard bit depth for professional audio.
    Pcm24Wav,
    /// FLAC -- lossless compression.
    Flac,
    /// MP3 -- lossy, widely compatible.
    Mp3,
    /// OGG Vorbis -- lossy, good quality at low bitrates.
    Ogg,
}

impl OutputFormat {
    /// Returns the FFmpeg codec name (`-c:a` value) for this format.
    pub fn ffmpeg_codec(&self) -> &'static str {
        match self {
            Self::AdpcmWav => "adpcm_ima_wav",
            Self::Pcm16Wav => "pcm_s16le",
            Self::Pcm24Wav => "pcm_s24le",
            Self::Flac => "flac",
            Self::Mp3 => "libmp3lame",
            Self::Ogg => "libvorbis",
        }
    }

    /// Returns the file extension (without leading dot) for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::AdpcmWav | Self::Pcm16Wav | Self::Pcm24Wav => "wav",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
        }
    }

    /// Returns `true` if this format is lossy and needs a bitrate argument.
    pub fn needs_bitrate(&self) -> bool {
        matches!(self, Self::Mp3 | Self::Ogg)
    }

    /// Human-readable label for the UI dropdown.
    pub fn label(&self) -> &'static str {
        match self {
            Self::AdpcmWav => "4-bit ADPCM WAV (suggested for game VO)",
            Self::Pcm16Wav => "16-bit PCM WAV",
            Self::Pcm24Wav => "24-bit PCM WAV (industry standard)",
            Self::Flac => "FLAC (lossless)",
            Self::Mp3 => "MP3",
            Self::Ogg => "OGG Vorbis",
        }
    }

    /// All variants in dropdown order.
    pub fn all() -> &'static [OutputFormat] {
        &[
            Self::AdpcmWav,
            Self::Pcm16Wav,
            Self::Pcm24Wav,
            Self::Flac,
            Self::Mp3,
            Self::Ogg,
        ]
    }
}

// ---------------------------------------------------------------------------
// Processing Options
// ---------------------------------------------------------------------------

/// Bundles all parameters needed by `process_single_file`.
///
/// This replaces the long parameter list and makes it easier to add new
/// options without changing function signatures everywhere.
#[derive(Clone)]
pub struct ProcessingOptions {
    pub target_lufs: Option<f32>,
    pub target_peak_dbfs: f32,
    pub automixer: bool,
    pub automixer_spectral_gate: bool,
    pub automixer_nn_dereverb: bool,
    pub automixer_dfn3_dereverb: bool,
    pub automixer_dfn3_mix: f32,
    pub automixer_dfn3_postfilter: bool,
    /// Module 5: smart downward expander (noise-floor-based, bounded).
    pub automixer_expander: bool,
    /// 0–100, UI-facing "Safety Margin". Higher = more conservative = larger margin.
    pub automixer_expander_safety_pct: f32,
    /// Preset reduction depth (Safe/Recommended/Hard/Max).
    pub automixer_expander_reduction_profile: ReductionProfile,
    /// Trims leading and trailing silence from every file, via FFmpeg's
    /// `silenceremove`.
    ///
    /// Deliberately **not** an automixer module: it is a filter folded into a
    /// chain the pipeline already builds, so it costs no extra process and no
    /// extra pass, and it works with the automixer on or off. See
    /// `crate::processing::trim_silence_chain`.
    pub trim_silence: bool,
    /// What counts as silence for the trim. Ignored when `trim_silence` is off.
    pub trim_silence_threshold: SilenceThreshold,
    /// How much of the original silence the trim leaves at each end. Ignored
    /// when `trim_silence` is off.
    pub trim_silence_pad: SilencePad,
    pub output_format: OutputFormat,
    /// Bitrate in kbps for lossy formats (MP3, OGG). Ignored for lossless.
    pub bitrate_kbps: u32,
    /// Optional channel back to the UI log, for conditions the caller should
    /// see but that are not errors -- currently only the memory gate making a
    /// file wait, which without an explanation is indistinguishable from a hang.
    pub log: Option<Sender<AppMsg>>,
}

/// Mirrors the initial state of `AudioBatchApp`, so that constructing
/// `ProcessingOptions::default()` in a test exercises the same configuration
/// the application actually ships with.
///
/// Written out by hand rather than `#[derive(Default)]` on purpose: the derive
/// would produce `target_peak_dbfs: 0.0`, `bitrate_kbps: 0`,
/// `automixer_dfn3_mix: 0.0` and `automixer_expander_safety_pct: 0.0`, none of
/// which is the real default. Tests would still pass -- they would just quietly
/// be testing peak normalization to 0 dBFS and `-b:a 0k`. The parity test in
/// `app.rs` keeps this in step with the UI.
impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            target_lufs: None,
            target_peak_dbfs: -3.0,
            automixer: false,
            automixer_spectral_gate: false,
            automixer_nn_dereverb: false,
            automixer_dfn3_dereverb: false,
            automixer_dfn3_mix: 80.0,
            automixer_dfn3_postfilter: false,
            automixer_expander: false,
            automixer_expander_safety_pct: 50.0,
            automixer_expander_reduction_profile: ReductionProfile::Recommended,
            trim_silence: false,
            trim_silence_threshold: SilenceThreshold::Recommended,
            trim_silence_pad: SilencePad::Tight,
            output_format: OutputFormat::AdpcmWav,
            bitrate_kbps: 128,
            log: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_profile_db_matches_documented_values() {
        assert_eq!(ReductionProfile::Safe.db(), -8.0);
        assert_eq!(ReductionProfile::Recommended.db(), -12.0);
        assert_eq!(ReductionProfile::Hard.db(), -18.0);
        assert_eq!(ReductionProfile::Max.db(), -32.0);
    }

    #[test]
    fn reduction_profile_all_lists_every_variant_once() {
        let all = ReductionProfile::all();
        assert_eq!(all.len(), 4);
        assert_eq!(
            all,
            &[
                ReductionProfile::Safe,
                ReductionProfile::Recommended,
                ReductionProfile::Hard,
                ReductionProfile::Max,
            ]
        );
    }

    #[test]
    fn reduction_profile_default_is_recommended() {
        assert_eq!(ReductionProfile::default(), ReductionProfile::Recommended);
    }

    #[test]
    fn silence_threshold_db_matches_documented_values() {
        assert_eq!(SilenceThreshold::Safe.db(), -60.0);
        assert_eq!(SilenceThreshold::Recommended.db(), -45.0);
        assert_eq!(SilenceThreshold::Hard.db(), -32.0);
        assert_eq!(SilenceThreshold::Max.db(), -21.0);
    }

    #[test]
    fn silence_threshold_is_ordered_from_least_to_most_aggressive() {
        // The dropdown is a risk ladder, and the UI warns on the last rung.
        // A variant reordered into the wrong slot would put the warning on the
        // wrong preset without breaking anything else.
        let dbs: Vec<f32> = SilenceThreshold::all().iter().map(|t| t.db()).collect();
        assert!(dbs.windows(2).all(|w| w[0] < w[1]), "{dbs:?}");
        assert_eq!(SilenceThreshold::all().last(), Some(&SilenceThreshold::Max));
    }

    #[test]
    fn silence_threshold_all_lists_every_variant_once() {
        assert_eq!(
            SilenceThreshold::all(),
            &[
                SilenceThreshold::Safe,
                SilenceThreshold::Recommended,
                SilenceThreshold::Hard,
                SilenceThreshold::Max,
            ]
        );
    }

    #[test]
    fn silence_threshold_default_is_recommended() {
        assert_eq!(SilenceThreshold::default(), SilenceThreshold::Recommended);
    }

    #[test]
    fn silence_pad_secs_matches_documented_values() {
        assert_eq!(SilencePad::Tight.secs(), 0.0);
        assert_eq!(SilencePad::Short.secs(), 0.25);
        assert_eq!(SilencePad::Medium.secs(), 0.5);
        assert_eq!(SilencePad::Long.secs(), 1.0);
    }

    #[test]
    fn silence_pad_all_lists_every_variant_once() {
        assert_eq!(
            SilencePad::all(),
            &[
                SilencePad::Tight,
                SilencePad::Short,
                SilencePad::Medium,
                SilencePad::Long,
            ]
        );
    }

    #[test]
    fn silence_pad_default_is_tight() {
        // Tight reproduces the behaviour that shipped before the presets
        // existed, so the default cannot silently change anyone's output.
        assert_eq!(SilencePad::default(), SilencePad::Tight);
        assert_eq!(SilencePad::default().secs(), 0.0);
    }

    #[test]
    fn output_format_extension_codec_and_bitrate_flag_are_consistent() {
        let cases: &[(OutputFormat, &str, &str, bool)] = &[
            (OutputFormat::AdpcmWav, "wav", "adpcm_ima_wav", false),
            (OutputFormat::Pcm16Wav, "wav", "pcm_s16le", false),
            (OutputFormat::Pcm24Wav, "wav", "pcm_s24le", false),
            (OutputFormat::Flac, "flac", "flac", false),
            (OutputFormat::Mp3, "mp3", "libmp3lame", true),
            (OutputFormat::Ogg, "ogg", "libvorbis", true),
        ];
        for (format, extension, codec, needs_bitrate) in cases {
            assert_eq!(format.extension(), *extension, "{format:?} extension");
            assert_eq!(format.ffmpeg_codec(), *codec, "{format:?} codec");
            assert_eq!(
                format.needs_bitrate(),
                *needs_bitrate,
                "{format:?} needs_bitrate"
            );
        }
    }

    #[test]
    fn output_format_all_lists_every_variant_once() {
        assert_eq!(OutputFormat::all().len(), 6);
    }

    #[test]
    fn output_format_default_is_adpcm_wav() {
        assert_eq!(OutputFormat::default(), OutputFormat::AdpcmWav);
    }
}
