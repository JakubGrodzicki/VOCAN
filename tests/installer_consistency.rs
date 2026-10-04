//! The model's URL and hash are written down in four places: `src/vad.rs`, both
//! installers, and the CI workflow. Nothing links them, so changing one and
//! forgetting another would leave a green build and an installer that downloads
//! the wrong file or rejects the right one on a user's machine.
//!
//! Plain file reads, no model, no network, no ffmpeg: this is a gate test and
//! runs with `cargo test`.

use std::path::PathBuf;
use vocan::vad::{MODEL_SHA256, MODEL_URL};

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn every_place_that_fetches_the_model_agrees_with_vad_rs() {
    for file in [
        "installWindows.ps1",
        "installMacLinux.sh",
        ".github/workflows/ci.yml",
    ] {
        let text = read(file);
        assert!(
            text.contains(MODEL_URL),
            "{file} does not contain the model URL from src/vad.rs:\n  {MODEL_URL}"
        );
        assert!(
            text.contains(MODEL_SHA256),
            "{file} does not contain the model hash from src/vad.rs:\n  {MODEL_SHA256}"
        );
    }
}

#[test]
fn no_place_names_a_different_model_release() {
    // A stale pin left next to the new one is the likeliest way to drift:
    // both strings would "contain" the right values and one would still be used.
    for file in [
        "installWindows.ps1",
        "installMacLinux.sh",
        ".github/workflows/ci.yml",
        "README.MD",
    ] {
        for (n, line) in read(file).lines().enumerate() {
            if let Some(i) = line.find("snakers4/silero-vad/raw/") {
                let after = &line[i + "snakers4/silero-vad/raw/".len()..];
                let tag = after.split('/').next().unwrap_or("");
                // The model URL and the CI fixture URL share a release tag.
                let pinned = MODEL_URL
                    .split("/raw/")
                    .nth(1)
                    .and_then(|r| r.split('/').next())
                    .unwrap();
                assert_eq!(
                    tag,
                    pinned,
                    "{file}:{} points at silero-vad {tag}, src/vad.rs pins {pinned}",
                    n + 1
                );
            }
        }
    }
}

#[test]
fn both_installers_can_skip_the_download() {
    assert!(read("installWindows.ps1").contains("NoVad"));
    assert!(read("installMacLinux.sh").contains("--no-vad"));
}

#[test]
fn both_installers_package_the_models_folder_next_to_the_executable() {
    // vad::model_path looks in `<exe dir>/models/`. An installer that downloads
    // the model into target/ but forgets to copy it into VOCAN-App leaves a
    // feature that works for the developer and is disabled for the user.
    for file in ["installWindows.ps1", "installMacLinux.sh"] {
        let text = read(file);
        assert!(
            text.contains("models"),
            "{file} never mentions the models folder"
        );
        assert!(
            text.contains("VOCAN-App"),
            "{file} no longer packages into VOCAN-App"
        );
    }
}
