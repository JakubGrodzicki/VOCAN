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

    // Press Stop only once the decode is really running. A fixed sleep is a
    // guess: loading the model and starting ffmpeg take seconds on a loaded CI
    // runner, and a Stop that lands before the child exists is caught by the
    // registry's latch instead, which tests something else. The wait is
    // separate from the budget below, so slow startup cannot fail the test.
    let wait_until = Instant::now() + Duration::from_secs(60);
    while vocan::proc::live_count() == 0 {
        assert!(
            Instant::now() < wait_until,
            "no ffmpeg child registered within 60 s of starting the detection"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let started_after = started.elapsed();
    // Let it get properly into the file, then press Stop.
    std::thread::sleep(Duration::from_millis(500));

    let stopped_at = Instant::now();
    vocan::proc::terminate_all();

    let result = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("detect_speech did not return after Stop");
    vocan::proc::resume();
    let stop_took = stopped_at.elapsed();

    assert!(
        result.is_err(),
        "a decode killed mid-file must be an error, not a half-read answer"
    );
    // Measured from the Stop, not from the start of the test: what is being
    // pinned is how long Stop takes to reach the decode. Fifteen minutes of
    // audio left running would take far longer than this.
    assert!(
        stop_took < Duration::from_secs(5),
        "Stop took {stop_took:?} to end the detection (the decode had been running since {started_after:?}); it was probably left running"
    );
}
