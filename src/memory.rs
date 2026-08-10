//! Memory budgeting for the in-memory DSP path.
//!
//! The Automixer pipeline decodes a whole file into a `Vec<f32>` and keeps
//! roughly two to three copies of it alive at the peak (see [`dsp_weight`]).
//! Rayon runs `cores - 1` files at once, so the batch's real footprint is that
//! peak times the thread count. Nothing here caps either number: the budget is
//! expressed in bytes and scaled to the machine's own RAM, so a workstation
//! with headroom never notices it.
//!
//! What it does prevent is the one failure mode Rust cannot recover from. A
//! failed allocation **aborts the process** -- `catch_unwind` in the batch
//! worker does not see it, so a single oversized file takes the entire run
//! down instead of being reported and skipped.
//!
//! Two layers, in order of importance:
//!
//!   1. [`check_alloc`] and `Vec::try_reserve` on the call sites, turning an
//!      abort into an ordinary per-file error. Free, and never blocks anything.
//!   2. [`gate`], a weighted admission gate for the collective case: several
//!      files that each fit but together do not. Without it those files fail
//!      with a clear message; with it they are serialised and succeed.

use std::sync::{Condvar, Mutex, OnceLock};

/// Fraction of physical RAM the DSP path is allowed to hold at once.
///
/// Half leaves room for the OS, the GUI, ffmpeg's own buffers and the page
/// cache. It is a ceiling on *concurrent* work, not on any single file: see
/// [`MemoryGate::acquire`] for how an oversized file still gets through.
const BUDGET_FRACTION: f64 = 0.5;

/// Peak in-memory copies of the signal, as a multiple of its decoded size.
///
/// Measured, not estimated. The spectral-gate path holds the caller's buffer
/// plus the accumulator (`out = acc` is a move, not a copy), which measures at
/// just under 2x; 2.5 leaves margin for allocator fragmentation and hound's
/// own buffering, because the expensive mistake here is admitting too much,
/// not too little.
///
/// DeepFilterNet3 is heavier: `apply_dereverb_dfn3` has the input, the wet
/// signal read back from disk and the mixed output all live simultaneously,
/// which is 3x before any overhead.
pub fn dsp_weight(dfn3_enabled: bool) -> f64 {
    if dfn3_enabled {
        3.5
    } else {
        2.5
    }
}

/// Total physical RAM in bytes, or `None` if the platform will not say.
///
/// `None` disables every limit in this module rather than guessing a number.
pub fn total_physical_memory() -> Option<u64> {
    static CACHED: OnceLock<Option<u64>> = OnceLock::new();
    *CACHED.get_or_init(detect_physical_memory)
}

#[cfg(windows)]
fn detect_physical_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: MEMORYSTATUSEX is a plain C struct of integers, so an all-zero
    // bit pattern is a valid value for it. `dwLength` is the documented
    // in-parameter telling the API which version of the struct it was handed,
    // and the call only writes into the pointee.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok != 0 && status.ullTotalPhys > 0 {
        Some(status.ullTotalPhys)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn detect_physical_memory() -> Option<u64> {
    // SAFETY: `sysconf` takes an int name and returns a long. No pointers.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    if pages > 0 && page_size > 0 {
        Some(pages as u64 * page_size as u64)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn detect_physical_memory() -> Option<u64> {
    let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
    let mut out: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `mib` holds exactly the 2 name components declared by the length
    // argument, and `out`/`len` describe a correctly sized output buffer for
    // HW_MEMSIZE, which is a 64-bit quantity. The new-value pointer is null
    // with length 0, i.e. a pure read.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut out as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && out > 0 {
        Some(out)
    } else {
        None
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn detect_physical_memory() -> Option<u64> {
    None
}

/// Rejects an allocation that obviously cannot succeed, before it is attempted.
///
/// `vec![0f32; n]` hits std's `IsZero` specialisation and goes straight to
/// `alloc_zeroed`, which for a large buffer hands back already-zeroed pages
/// from the OS with no write pass at all. Replacing it with
/// `try_reserve` + `resize` would make it fallible but would also give up that
/// specialisation and add a full write over the buffer. For a buffer whose size
/// is known up front, checking the size is both fallible *and* free.
///
/// Returns `Err` only when the request exceeds the whole budget; anything
/// smaller is left to the allocator.
pub fn check_alloc(elements: usize, element_size: usize, what: &str) -> anyhow::Result<()> {
    let bytes = (elements as u64).saturating_mul(element_size as u64);
    let Some(total) = total_physical_memory() else {
        return Ok(());
    };
    let limit = (total as f64 * BUDGET_FRACTION) as u64;
    if bytes > limit {
        anyhow::bail!(
            "this file needs {:.1} GB for {} alone, more than the {:.1} GB budget \
             ({:.0}% of system RAM). Process it without Automixer, or split it into \
             shorter takes.",
            bytes as f64 / 1024.0 / 1024.0 / 1024.0,
            what,
            limit as f64 / 1024.0 / 1024.0 / 1024.0,
            BUDGET_FRACTION * 100.0,
        );
    }
    Ok(())
}

/// Weighted admission gate over a byte budget.
///
/// Not a thread limit: permits are weighted by each file's estimated footprint,
/// so a batch of short voice-over lines (a few MB at peak) never contends even
/// with every core busy, and the gate costs one uncontended mutex acquisition
/// per file.
pub struct MemoryGate {
    state: Mutex<u64>,
    space_freed: Condvar,
    /// Zero means "no budget known" -- the gate is then a pass-through.
    budget: u64,
}

impl MemoryGate {
    fn new(budget: u64) -> Self {
        Self {
            state: Mutex::new(0),
            space_freed: Condvar::new(),
            budget,
        }
    }

    /// Waits until `bytes` fits in the budget, then reserves it until the
    /// returned permit is dropped.
    ///
    /// A request larger than the entire budget is clamped rather than refused,
    /// so such a file runs alone instead of deadlocking or being rejected --
    /// there is no upper limit on file length here. `on_contended` is called at
    /// most once, and only if the caller actually has to wait, so the UI can
    /// explain a stall that would otherwise look like a hang.
    pub fn acquire(&self, bytes: u64, on_contended: impl FnOnce(String)) -> MemoryPermit<'_> {
        if self.budget == 0 || bytes == 0 {
            return MemoryPermit {
                gate: self,
                bytes: 0,
            };
        }
        let want = bytes.min(self.budget);

        let mut in_use = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // `Option::take` rather than a bool, so the borrow checker can see that
        // a `FnOnce` reached from inside a loop really is called at most once.
        let mut notify = Some(on_contended);
        // `*in_use > 0` is the anti-deadlock clause: when nothing else is
        // running, an over-budget request proceeds regardless of size.
        while *in_use > 0 && *in_use + want > self.budget {
            if let Some(notify) = notify.take() {
                notify(format!(
                    "Waiting for memory: this file needs {:.1} GB, and other files in \
                     flight are already using {:.1} GB of the {:.1} GB budget. \
                     Throughput drops until they finish -- this is not a hang.",
                    want as f64 / 1024.0 / 1024.0 / 1024.0,
                    *in_use as f64 / 1024.0 / 1024.0 / 1024.0,
                    self.budget as f64 / 1024.0 / 1024.0 / 1024.0,
                ));
            }
            in_use = self
                .space_freed
                .wait(in_use)
                .unwrap_or_else(|e| e.into_inner());
        }
        *in_use += want;
        MemoryPermit {
            gate: self,
            bytes: want,
        }
    }

    #[cfg(test)]
    fn in_use(&self) -> u64 {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Releases its reservation on drop.
///
/// RAII rather than an explicit release call because the batch worker wraps
/// each file in `catch_unwind`: a panic inside the critical section would leak
/// the reservation permanently and throttle the rest of the run down to
/// nothing.
pub struct MemoryPermit<'a> {
    gate: &'a MemoryGate,
    bytes: u64,
}

impl Drop for MemoryPermit<'_> {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut in_use = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        *in_use = in_use.saturating_sub(self.bytes);
        drop(in_use);
        self.gate.space_freed.notify_all();
    }
}

/// The process-wide gate, sized from physical RAM on first use.
pub fn gate() -> &'static MemoryGate {
    static GATE: OnceLock<MemoryGate> = OnceLock::new();
    GATE.get_or_init(|| {
        let budget = total_physical_memory()
            .map(|total| (total as f64 * BUDGET_FRACTION) as u64)
            .unwrap_or(0);
        MemoryGate::new(budget)
    })
}

/// Estimated peak bytes for running one file through the Rust DSP path.
///
/// `None` duration (ffmpeg would not report one) yields 0, which makes the gate
/// a pass-through for that file: guessing would be worse than not gating.
pub fn estimated_dsp_bytes(duration_secs: Option<f32>, sample_rate: u32, dfn3: bool) -> u64 {
    let Some(secs) = duration_secs.filter(|s| s.is_finite() && *s > 0.0) else {
        return 0;
    };
    let samples = secs as f64 * sample_rate as f64;
    (samples * 4.0 * dsp_weight(dfn3)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_weight_is_heavier_for_dfn3() {
        // DFN3 holds input + wet + mixed output simultaneously; the spectral
        // path holds input + accumulator.
        assert!(dsp_weight(true) > dsp_weight(false));
        assert!(dsp_weight(false) >= 2.0, "must cover the measured ~2x peak");
    }

    #[test]
    fn estimated_bytes_scale_with_duration_and_rate() {
        let a = estimated_dsp_bytes(Some(10.0), 48_000, false);
        let b = estimated_dsp_bytes(Some(20.0), 48_000, false);
        assert_eq!(b, 2 * a);
        let c = estimated_dsp_bytes(Some(10.0), 96_000, false);
        assert_eq!(c, 2 * a);
    }

    #[test]
    fn estimated_bytes_are_zero_when_duration_is_unknown_or_absurd() {
        assert_eq!(estimated_dsp_bytes(None, 48_000, false), 0);
        assert_eq!(estimated_dsp_bytes(Some(0.0), 48_000, false), 0);
        assert_eq!(estimated_dsp_bytes(Some(f32::NAN), 48_000, false), 0);
        assert_eq!(estimated_dsp_bytes(Some(-5.0), 48_000, false), 0);
    }

    fn noop(_: String) {}

    #[test]
    fn permit_reserves_and_releases() {
        let gate = MemoryGate::new(1000);
        {
            let _p = gate.acquire(400, noop);
            assert_eq!(gate.in_use(), 400);
        }
        assert_eq!(gate.in_use(), 0, "drop must release the reservation");
    }

    #[test]
    fn concurrent_requests_below_budget_do_not_contend() {
        let gate = MemoryGate::new(1000);
        let mut contended = false;
        let _a = gate.acquire(300, |_| panic!("must not wait"));
        let _b = gate.acquire(300, |_| contended = true);
        assert!(!contended);
        assert_eq!(gate.in_use(), 600);
    }

    #[test]
    fn a_request_larger_than_the_whole_budget_is_clamped_not_refused() {
        // The anti-deadlock property: with nothing else running, an oversized
        // file must still be admitted, alone. Refusing it would put a hard cap
        // on file length; blocking would hang the batch forever.
        let gate = MemoryGate::new(1000);
        let p = gate.acquire(9_999, noop);
        assert_eq!(gate.in_use(), 1000, "clamped to the full budget");
        drop(p);
        assert_eq!(gate.in_use(), 0);
    }

    #[test]
    fn a_zero_budget_gate_never_blocks_or_accounts() {
        // What an unknown-RAM machine gets: a pass-through.
        let gate = MemoryGate::new(0);
        let _a = gate.acquire(u64::MAX, |_| panic!("must not wait"));
        let _b = gate.acquire(u64::MAX, |_| panic!("must not wait"));
        assert_eq!(gate.in_use(), 0);
    }

    #[test]
    fn an_over_budget_request_waits_until_the_holder_releases() {
        use std::sync::Arc;
        use std::time::Duration;

        let gate = Arc::new(MemoryGate::new(1000));
        let held = gate.acquire(800, noop);

        let waiter_gate = Arc::clone(&gate);
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let mut saw_warning = false;
            let p = waiter_gate.acquire(800, |msg| {
                saw_warning = msg.contains("not a hang");
            });
            let _ = tx.send(saw_warning);
            drop(p);
        });

        // The waiter must still be blocked while the first permit is held.
        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "second acquire should block while 800/1000 is in use"
        );
        drop(held);

        let saw_warning = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter should proceed once the budget frees up");
        assert!(saw_warning, "a caller that waits must be told why");
        waiter.join().unwrap();
    }

    #[test]
    fn check_alloc_accepts_ordinary_sizes() {
        // A 10-minute 48kHz mono buffer: 28.8M samples, ~115 MB.
        assert!(check_alloc(28_800_000, 4, "test buffer").is_ok());
    }

    #[test]
    fn check_alloc_rejects_an_impossible_size() {
        if total_physical_memory().is_none() {
            return; // no budget known -> nothing to enforce
        }
        // Larger than any budget derived from a fraction of real RAM.
        assert!(check_alloc(usize::MAX / 8, 4, "test buffer").is_err());
    }
}
