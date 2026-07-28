use serde::Deserialize;

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
        &[
            Self::Safe,
            Self::Recommended,
            Self::Hard,
            Self::Max,
        ]
    }
}

#[derive(Deserialize, Debug)]
pub struct LoudnormStats {
    pub input_i: String,
    pub input_tp: String,
    pub input_lra: String,
    pub input_thresh: String,
    pub target_offset: String,
}

/// Describes the normalization method used for a file -- used for logging.
pub enum NormResult {
    /// Standard 2-pass EBU R128 (files >= ~3s).
    Standard,
    /// 2-pass EBU R128 with silence padding (files ~1-3s, returning -inf without padding).
    Padded,
    /// Peak normalization (files < 1s, too short for EBU R128 integration).
    Peak { gain_db: f32 },
    /// Conversion without normalization (extreme fallback -- silent or empty signal).
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
    /// 32-bit float WAV -- lossless, ideal for re-normalization.
    Pcm32fWav,
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
            Self::Pcm32fWav => "pcm_f32le",
            Self::Flac => "flac",
            Self::Mp3 => "libmp3lame",
            Self::Ogg => "libvorbis",
        }
    }

    /// Returns the file extension (without leading dot) for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::AdpcmWav | Self::Pcm16Wav | Self::Pcm32fWav => "wav",
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
            Self::Pcm32fWav => "32-bit float WAV (lossless)",
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
            Self::Pcm32fWav,
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
    pub output_format: OutputFormat,
    /// Bitrate in kbps for lossy formats (MP3, OGG). Ignored for lossless.
    pub bitrate_kbps: u32,
}