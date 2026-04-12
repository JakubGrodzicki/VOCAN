use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct LoudnormStats {
    pub input_i: String,
    pub input_tp: String,
    pub input_lra: String,
    pub input_thresh: String,
    pub target_offset: String,
}

/// Describes the normalization method used for a file — used for logging.
pub enum NormResult {
    /// Standard 2-pass EBU R128 (files >= ~3s).
    Standard,
    /// 2-pass EBU R128 with silence padding (files ~1-3s, returning -inf without padding).
    Padded,
    /// Peak normalization (files < 1s, too short for EBU R128 integration).
    Peak { gain_db: f32 },
    /// Conversion without normalization (extreme fallback — silent or empty signal).
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
