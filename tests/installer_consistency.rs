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

/// The script with comment lines dropped and runs of spaces collapsed, so an
/// assertion about a line of code cannot be satisfied by a comment that
/// mentions it, or broken by realigning an `=`.
fn code_of(file: &str) -> String {
    read(file)
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_code_has(file: &str, code: &str, needle: &str, why: &str) {
    assert!(
        code.contains(needle),
        "{file}: expected this line of code:\n    {needle}\nbecause {why}"
    );
}

#[test]
fn the_windows_installer_puts_the_model_where_the_app_looks() {
    // vad::model_path looks in `<exe dir>/models/silero_vad.onnx`. These are the
    // lines that decide where the file lands, whether it is trusted, and
    // whether it reaches the folder the user runs VOCAN from. Mutating any of
    // them leaves a build that works for the developer and a disabled checkbox
    // for everyone else.
    let f = "installWindows.ps1";
    let c = code_of(f);
    assert_code_has(
        f,
        &c,
        r#"$ModelDir = Join-Path $BinDir "models""#,
        "the folder must be named models",
    );
    assert_code_has(
        f,
        &c,
        r#"$ModelDest = Join-Path $ModelDir "silero_vad.onnx""#,
        "the file name is fixed by vad::MODEL_FILE",
    );
    assert_code_has(
        f,
        &c,
        "if ($gotHash -ine $ModelSha256) {\nRemove-Item $ModelDest -Force",
        "a model that fails the hash check must be deleted in that same branch, or the app would use it",
    );
    assert_code_has(
        f,
        &c,
        r#"Copy-Item $ModelDir (Join-Path $AppDir "models") -Recurse"#,
        "the packaged app needs the models folder next to VOCAN.exe",
    );
    assert_code_has(
        f,
        &c,
        "$env:VOCAN_SILERO_MODEL = $ModelDest",
        "the test run executes from target\\deps, not from the app folder",
    );
}

#[test]
fn the_unix_installer_puts_the_model_where_the_app_looks() {
    let f = "installMacLinux.sh";
    let c = code_of(f);
    assert_code_has(
        f,
        &c,
        r#"MODEL_DIR="$BIN_DIR/models""#,
        "the folder must be named models",
    );
    assert_code_has(
        f,
        &c,
        r#"MODEL_DEST="$MODEL_DIR/silero_vad.onnx""#,
        "the file name is fixed by vad::MODEL_FILE",
    );
    assert_code_has(
        f,
        &c,
        "elif [ \"$GOT_SHA\" != \"$MODEL_SHA256\" ]; then\nrm -f \"$MODEL_DEST\"",
        "a model that fails the hash check must be deleted in that same branch, or the app would use it",
    );
    assert_code_has(
        f,
        &c,
        r#"cp -R "$MODEL_DIR" "$APP_DIR/models""#,
        "the packaged app needs the models folder next to VOCAN",
    );
    assert_code_has(
        f,
        &c,
        r#"export VOCAN_SILERO_MODEL="$MODEL_DEST""#,
        "the test run executes from target/.../deps, not from the app folder",
    );
}

#[test]
fn ci_makes_a_skipped_vad_test_a_failure() {
    // A skip is a pass. Without this variable a wrong model path in CI would
    // turn every real-model test into a green no-op.
    assert!(
        code_of(".github/workflows/ci.yml").contains("VOCAN_REQUIRE_VAD=1"),
        "ci.yml must set VOCAN_REQUIRE_VAD=1 (see tests/common/mod.rs skip_or_fail)"
    );
}

#[test]
fn model_file_name_matches_what_the_app_looks_for() {
    assert_eq!(vocan::vad::MODEL_FILE, "silero_vad.onnx");
    assert!(
        MODEL_URL.ends_with("/silero_vad.onnx"),
        "the download is saved under the name the app looks for"
    );
}
