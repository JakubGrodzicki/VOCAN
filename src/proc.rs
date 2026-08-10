//! Cross-thread termination of the external processes VOCAN spawns.
//!
//! Every unit of work shells out to ffmpeg (and optionally `deep-filter`) and
//! then **blocks** waiting for it. That is the right shape for the hot path --
//! polling `try_wait()` in a sleep loop would add the poll interval to every
//! one of the thousands of short ffmpeg invocations a batch performs, which for
//! a 30 ms call costs more than the work itself.
//!
//! The consequence is that a worker thread parked in `wait()` cannot notice the
//! cancel flag on its own. Before this module, that meant:
//!
//!   * Stop only took effect *between* files, so a long file (minutes, with
//!     DeepFilterNet3) ignored it until it finished;
//!   * a genuinely wedged subprocess parked its worker thread forever, and the
//!     UI sat in "Processing" with no way out but killing the application;
//!   * closing the window left ffmpeg running, still writing to output files --
//!     Windows gives us no job object to tear the tree down.
//!
//! So the interruption comes from outside instead: children register here while
//! they run, and [`terminate_all`] kills the live ones. A blocking `wait()`
//! returns immediately once its child is gone, which unwinds the whole pipeline
//! through the normal error paths.
//!
//! Deliberately **not** a timeout. A blind timer cannot distinguish a wedged
//! ffmpeg from a legitimately slow one, and killing valid work is a worse
//! failure than the one being fixed. Termination is driven by explicit user
//! intent (Stop, or closing the window), which is what makes a hang escapable.
//!
//! There is no supervisor thread either: the UI thread already runs when Stop
//! is clicked or the window is closed, so it does the killing directly.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};

/// Exit status plus captured stderr, from [`output_supervised`].
///
/// There is no `stdout` field on purpose: every call site that used
/// `Command::output()` reads only stderr (ffmpeg's diagnostics and loudnorm
/// JSON both go there), so stdout is routed to null rather than piped into a
/// buffer nobody reads. The one place that genuinely wants a child's stdout --
/// the de-esser pass streaming f32 samples in `processing.rs` -- spawns its
/// child directly and calls [`register`] itself.
pub struct SupervisedOutput {
    pub status: ExitStatus,
    pub stderr: Vec<u8>,
}

/// Platform-specific means of terminating a registered child.
///
/// Stored as a plain integer rather than a `RawHandle` so the value is `Send`
/// without any promise on our part.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct KillToken(isize);

#[cfg(unix)]
#[derive(Clone, Copy)]
struct KillToken(u32);

#[derive(Default)]
struct RegistryState {
    next_id: u64,
    live: HashMap<u64, KillToken>,
    /// Set by [`terminate_all`]. While true, a child that registers is killed
    /// on the spot -- it raced past the cancel check and started anyway.
    terminating: bool,
}

fn state() -> &'static Mutex<RegistryState> {
    static STATE: OnceLock<Mutex<RegistryState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(RegistryState::default()))
}

/// A poisoned registry mutex would mean a panic while holding it; the map is a
/// plain `HashMap` with no invariant that a panic could have broken halfway, so
/// recovering the contents is strictly better than propagating the panic into
/// every subsequent spawn.
fn lock_state() -> std::sync::MutexGuard<'static, RegistryState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
fn kill_token(child: &Child) -> KillToken {
    use std::os::windows::io::AsRawHandle;
    KillToken(child.as_raw_handle() as isize)
}

#[cfg(unix)]
fn kill_token(child: &Child) -> KillToken {
    KillToken(child.id())
}

#[cfg(windows)]
fn terminate(token: KillToken) {
    // SAFETY: the token is only reachable while its entry is in `live`, and
    // both this call and deregistration hold the registry mutex. An entry is
    // only removed by `ChildHandle::drop`, which runs strictly before the
    // owning `Child` is dropped, so the handle is still open here and still
    // refers to our own child -- Windows keeps a process id reserved for as
    // long as any handle to the process object is open, so it cannot have been
    // recycled either. A failed call means the process already exited on its
    // own, which is exactly the outcome being asked for.
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(token.0 as _, 1);
    }
}

#[cfg(unix)]
fn terminate(token: KillToken) {
    // SAFETY: `kill` with a plain signal number touches no memory. The pid is
    // still live for the same reason as the Windows handle above: the entry is
    // present, so `Child::wait` has not returned, so the child has not been
    // reaped and its pid cannot have been recycled.
    unsafe {
        libc::kill(token.0 as libc::pid_t, libc::SIGKILL);
    }
}

/// Registers a running child so [`terminate_all`] can reach it.
///
/// **Drop order matters.** The returned guard must be dropped *before* the
/// `Child` it describes, because the guard's `Drop` is what guarantees the kill
/// token is gone from the registry while the underlying handle/pid is still
/// valid. Declaring the guard after the `Child` in the same scope gives that
/// ordering for free (locals drop in reverse declaration order); the one thing
/// to avoid is moving the `Child` somewhere that outlives the guard, such as
/// `Child::wait_with_output`, which consumes it.
#[must_use = "dropping the guard immediately would deregister the child at once"]
pub fn register(child: &Child) -> ChildHandle {
    let token = kill_token(child);
    let mut st = lock_state();
    if st.terminating {
        // Raced past the cancel check. Kill it here rather than adding it.
        drop(st);
        terminate(token);
        return ChildHandle { id: None };
    }
    let id = st.next_id;
    st.next_id += 1;
    st.live.insert(id, token);
    ChildHandle { id: Some(id) }
}

/// Deregisters its child on drop. See [`register`] for the ordering rule.
pub struct ChildHandle {
    id: Option<u64>,
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            lock_state().live.remove(&id);
        }
    }
}

/// Kills every registered child and blocks any that register afterwards.
///
/// Call [`resume`] before starting a new run.
pub fn terminate_all() {
    let mut st = lock_state();
    st.terminating = true;
    // Killing while holding the lock is what makes the tokens safe to use: a
    // concurrent `ChildHandle::drop` cannot remove an entry (and let its
    // `Child` be dropped, closing the handle / reaping the pid) in the middle
    // of this loop.
    for token in st.live.values() {
        terminate(*token);
    }
}

/// Clears the "terminating" latch set by [`terminate_all`] so a new run can
/// spawn children again.
pub fn resume() {
    lock_state().terminating = false;
}

/// Runs `cmd` to completion, capturing stderr, with the child registered for
/// termination for as long as it runs.
///
/// Drop-in replacement for `Command::output()` at every VOCAN call site. stdin
/// is null and stdout is discarded (see [`SupervisedOutput`]); stderr is read
/// to EOF before waiting, which is safe with a single pipe -- there is no
/// second pipe that could fill up and deadlock in the meantime.
pub fn output_supervised(cmd: &mut Command) -> std::io::Result<SupervisedOutput> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    // Declared after `child`, so it drops first. See `register`.
    let _guard = register(&child);

    let mut stderr = Vec::new();
    let read_result = match child.stderr.as_mut() {
        Some(pipe) => pipe.read_to_end(&mut stderr).map(|_| ()),
        None => Ok(()),
    };
    // Same reasoning as the de-esser pass in `processing.rs`: close our end
    // first so a child still writing cannot block, then always reap before
    // propagating whichever error came first.
    drop(child.stderr.take());
    let status = child.wait();

    read_result?;
    Ok(SupervisedOutput {
        status: status?,
        stderr,
    })
}

// ---------------------------------------------------------------------------
// Scratch directory
// ---------------------------------------------------------------------------

/// How long an abandoned scratch directory is left alone before [`sweep_scratch`]
/// removes it.
///
/// Generous on purpose. A running instance refreshes its directory's modified
/// time every time it creates or removes a temp file, so an active batch stays
/// well clear of this; the threshold only has to outlast a plausible gap
/// between two files in one run.
const SCRATCH_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

fn scratch_prefix() -> String {
    format!("vocan-scratch-{}", std::process::id())
}

/// This instance's scratch directory, created on first use.
///
/// The pipeline writes whole decoded signals here (see `processing.rs` and the
/// DeepFilterNet3 stage), which for a long file is gigabytes. Those are
/// `NamedTempFile`/`TempDir` values, so they clean up when dropped -- but
/// closing the window ends the process without unwinding the detached worker
/// threads, so their destructors never run and the files stay behind, run after
/// run, on the system drive.
///
/// Giving each instance its own directory is what makes cleanup safe: a blanket
/// wipe of one shared directory at startup would delete the temp files of a
/// second VOCAN that happened to be mid-batch.
pub fn scratch_dir() -> Option<&'static std::path::Path> {
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(scratch_prefix());
        std::fs::create_dir_all(&dir).ok().map(|_| dir)
    })
    .as_deref()
}

/// Removes scratch directories left behind by instances that were killed
/// before they could clean up after themselves.
///
/// Skips this process's own directory, and anything recent enough to plausibly
/// belong to another live instance.
pub fn sweep_scratch() {
    let temp = std::env::temp_dir();
    let own = scratch_prefix();
    let Ok(entries) = std::fs::read_dir(&temp) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("vocan-scratch-") || name == own {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > SCRATCH_MAX_AGE);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests share one process-global registry, so they must not run
    /// concurrently with each other. `cargo test` runs test functions on
    /// separate threads within a binary, so they serialise on this mutex.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn live_count() -> usize {
        lock_state().live.len()
    }

    fn sleeper() -> Command {
        // A process that lives long enough to be observed and killed, using
        // only what every host running these tests already has.
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    #[test]
    fn registration_is_undone_when_the_guard_drops() {
        let _t = test_guard();
        resume();
        let before = live_count();
        let mut child = sleeper().spawn().expect("spawn sleeper");
        {
            let _guard = register(&child);
            assert_eq!(live_count(), before + 1, "child should be registered");
        }
        assert_eq!(live_count(), before, "guard must deregister on drop");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn terminate_all_unblocks_a_waiting_child() {
        let _t = test_guard();
        resume();
        let mut child = sleeper().spawn().expect("spawn sleeper");
        let guard = register(&child);

        // The real shape of the bug: another thread is parked in `wait()` while
        // the UI thread asks for termination. Without it, this join would sit
        // here for the sleeper's full 30s.
        terminate_all();

        let status = child.wait().expect("wait after terminate");
        assert!(
            !status.success(),
            "a terminated child must not report success"
        );
        drop(guard);
        resume();
    }

    #[test]
    fn a_child_registering_after_terminate_all_is_killed_immediately() {
        let _t = test_guard();
        resume();
        terminate_all();

        let mut child = sleeper().spawn().expect("spawn sleeper");
        let guard = register(&child);
        let status = child.wait().expect("wait on killed child");
        assert!(!status.success());
        assert_eq!(
            live_count(),
            0,
            "a child killed at registration must not be tracked"
        );

        drop(guard);
        resume();
    }

    #[test]
    fn output_supervised_captures_stderr_and_reports_status() {
        let _t = test_guard();
        resume();
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "echo marker 1>&2"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "echo marker 1>&2"]);
            c
        };
        let out = output_supervised(&mut cmd).expect("run");
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("marker"),
            "stderr should be captured, got {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(live_count(), 0, "the child must be deregistered on return");
    }
}
