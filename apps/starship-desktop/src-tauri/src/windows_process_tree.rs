//! Windows process-tree reclamation.
//!
//! `Child::kill()` terminates the direct child only. Long-lived children -
//! the SSH tunnel in particular - spawn grandchildren, and those survive the
//! shell. The result is orphaned processes that keep holding ports, handles
//! and console hosts after the client is gone, which shows up as a later
//! start that cannot bind its port, a slow cold start, or a stray terminal
//! window flashing on screen.
//!
//! `taskkill /T` walks the live tree and terminates it whole, so a single
//! call replaces "kill the child and hope its children notice".
//!
//! The kernel-guaranteed alternative is a job object created with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which also covers the case where the
//! shell itself is killed outright and no `Drop` runs at all. `windows_job`
//! already carries that raw FFI pattern; moving every long-lived spawn site
//! onto one shared job is the follow-up, and this module is the seam it will
//! land in.

use std::process::Child;

/// Terminates `child` together with every process below it, then reaps it.
///
/// Returns `true` when the child was reaped. A `taskkill` that cannot run is
/// not fatal: the plain `kill` fallback still runs, so a caller is never left
/// worse off than the previous behaviour.
pub(crate) fn kill_tree(child: &mut Child) -> bool {
    #[cfg(target_os = "windows")]
    {
        let pid = child.id().to_string();
        // `/T` = the process and its descendants, `/F` = force.
        let reclaimed = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", pid.as_str()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !reclaimed {
            // The child may already be gone, or taskkill may be unavailable.
            // Either way the direct kill is still worth attempting.
            let _ = child.kill();
        }
        return child.wait().is_ok();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = child.kill();
        child.wait().is_ok()
    }
}
