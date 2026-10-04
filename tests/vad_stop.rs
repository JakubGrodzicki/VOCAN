//! Stop must reach the speech-detection decode.
//!
//! Its own test binary on purpose: `proc::terminate_all` kills every child
//! VOCAN has registered, process-wide, and `cargo test` runs the tests of one
//! binary in parallel. Sharing a binary with the other VAD tests would have this
//! one kill their ffmpegs.
//!
//! Run with:
//!
//!   cargo test --test vad_stop -- --ignored

mod common;

use common::{noise_at_dbfs, skip_if_no_vad, write_f32_wav};
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn stop_kills_the_speech_detection_decode_mid_file() {
    if skip_if_no_vad() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let take = dir.path().join("long.wav");
    // Fifteen minutes of room tone: minutes of work for the model, so that an
    // unregistered child would visibly outlive the Stop.
    write_f32_wav(&take, &noise_at_dbfs(16_000 * 60 * 15, -40.0, 5), 16_000);
    let model = vocan::vad::model_path().unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let input = take.clone();
    std::thread::spawn(move || {
        let r = vocan::vad::detect_speech(&input, &common::ffmpeg_path(), &model);
        let _ = tx.send(r.map(|_| ()).map_err(|e| e.to_string()));
    });

    // Let the decode get going, then press Stop.
    std::thread::sleep(Duration::from_millis(1500));
    vocan::proc::terminate_all();

    let result = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("detect_speech did not return after Stop");
    vocan::proc::resume();

    assert!(
        result.is_err(),
        "a decode killed mid-file must be an error, not a half-read answer"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "Stop took {:?}; the decode was probably left running",
        started.elapsed()
    );
}
