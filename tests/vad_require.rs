//! `VOCAN_REQUIRE_VAD` is what stops a wrong model path in CI from turning every
//! real-model test into a green no-op, so the switch itself is tested.
//!
//! Its own binary and a single test: it sets process-wide environment variables,
//! which would race with any other test reading them in the same process.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};

#[test]
fn a_vad_test_that_cannot_run_skips_normally_and_fails_when_required() {
    if !vocan::vad::ENGINE_AVAILABLE {
        // No engine on this target: the skip is unconditional by design.
        return;
    }

    // A model path that does not exist, and no model next to this test binary.
    std::env::set_var(
        vocan::vad::MODEL_ENV,
        std::env::temp_dir().join("vocan-no-such-model.onnx"),
    );

    std::env::remove_var("VOCAN_REQUIRE_VAD");
    assert!(
        common::skip_if_no_vad(),
        "without VOCAN_REQUIRE_VAD a test that cannot run must skip"
    );
    assert!(common::skip_if_no_vad_fixture());

    // An empty value counts as unset, the same as everywhere else it is read.
    std::env::set_var("VOCAN_REQUIRE_VAD", "");
    assert!(common::skip_if_no_vad());

    std::env::set_var("VOCAN_REQUIRE_VAD", "1");
    let outcome = catch_unwind(AssertUnwindSafe(common::skip_if_no_vad));
    let message = match outcome {
        Ok(skipped) => panic!("VOCAN_REQUIRE_VAD=1 must make the skip fail, got skip={skipped}"),
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default(),
    };
    assert!(
        message.contains("VOCAN_REQUIRE_VAD"),
        "the failure should say why: {message:?}"
    );
}
