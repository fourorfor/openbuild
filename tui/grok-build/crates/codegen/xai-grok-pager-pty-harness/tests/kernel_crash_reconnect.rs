//! J1 (§9.2): killing the kernel must not quit or freeze the TUI.
//!
//! The transport half (does the bridge notice?) is covered by
//! `openbuild-backend --example crash`. This test covers the part only a real
//! TUI can show: the screen keeps responding, a diagnosis appears, and
//! `/reconnect` brings the session back.
//!
//! Opt-in, like the rest of this crate's e2e family: it spawns a real terminal,
//! a real Rust TUI and a real Bun kernel. Run it with
//! `cargo test -p xai-grok-pager-pty-harness --test kernel_crash_reconnect -- --ignored --nocapture`.

#[path = "pty_e2e/common.rs"]
mod common;

use common::*;

/// Markers the crash notice must carry.
const CRASH_MARKER: &str = "OpenBuild kernel exited";
const RECONNECT_HINT: &str = "/reconnect";
const RESTART_TOAST: &str = "Restarting OpenBuild kernel";
/// Cold start of a debug TUI plus a Bun kernel: much longer than the
/// mock-backed suites get, and long enough to absorb a first-run kernel import.
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

fn find_kernel_binary() -> Option<PathBuf> {
    std::env::var("OPENBUILD_KERNEL")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            // The pager binary sits in `target/debug`; the kernel is copied to
            // the repo's `dist/` by build.ps1.
            let exe = pager_binary().ok()?;
            let mut dir = exe.parent()?;
            for _ in 0..6 {
                let candidate = dir.join("dist").join("openbuild-kernel.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
                dir = dir.parent()?;
            }
            None
        })
}

/// All `openbuild-kernel.exe` pids that are descendants of `root`.
#[cfg(windows)]
fn kernel_children(root: u32) -> Vec<u32> {
    let output = match std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Get-CimInstance Win32_Process -Filter \"Name='openbuild-kernel.exe'\" | \
                 Where-Object {{ $_.ParentProcessId -eq {root} }} | \
                 Select-Object -ExpandProperty ProcessId"
            ),
        ])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(not(windows))]
fn kernel_children(_root: u32) -> Vec<u32> {
    Vec::new()
}

/// Kill every kernel child of the pager process.
fn kill_kernel(root: u32) -> usize {
    let pids = kernel_children(root);
    for pid in &pids {
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
        #[cfg(not(windows))]
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
    pids.len()
}

#[test]
#[ignore = "spawns a real TUI + kernel; run explicitly for the J1 acceptance"]
fn killing_the_kernel_explains_itself_and_reconnect_restores_the_session() {
    let Some(kernel) = find_kernel_binary() else {
        eprintln!("skipping: no openbuild-kernel.exe found (set OPENBUILD_KERNEL)");
        return;
    };
    let binary = pager_binary().expect("pager binary");
    let kernel_str = kernel.to_string_lossy().to_string();
    let env: Vec<(&str, &str)> = vec![
        ("OPENBUILD_BACKEND", "opencode"),
        ("OPENBUILD_KERNEL", kernel_str.as_str()),
    ];

    let mut harness = PtyHarness::new_inherited_env(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &[],
        &env,
        None,
    )
    .expect("spawn the pager in a PTY");

    // Boot: the welcome screen renders the product name once the connection is
    // up, which proves the handshake completed against the real kernel. A debug
    // build plus a Bun kernel needs a generous window on a cold start.
    if let Err(e) = harness.wait_for_text("OpenBuild", BOOT_TIMEOUT) {
        panic!(
            "{e}\n--- raw output ---\n{}",
            String::from_utf8_lossy(harness.raw_output())
        );
    }

    let pager_pid = harness.child_pid().expect("the PTY child must have a pid");
    let kernel_pids = {
        let mut found = Vec::new();
        for _ in 0..50 {
            found = kernel_children(pager_pid);
            if !found.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        found
    };
    assert!(
        !kernel_pids.is_empty(),
        "no openbuild-kernel child process was found under the pager"
    );

    let killed = kill_kernel(pager_pid);
    assert!(killed > 0, "no kernel process could be killed");

    // The TUI must survive and explain itself. `wait_for_text` failing means the
    // PTY closed (the TUI quit) or the notice never rendered.
    harness
        .wait_for_text(CRASH_MARKER, Duration::from_secs(45))
        .expect("the crash notice must appear after the kernel dies");
    assert!(
        harness.contains_text(RECONNECT_HINT),
        "the crash notice must point at /reconnect; screen:\n{}",
        harness.screen_contents()
    );
    assert!(
        harness.is_running().expect("is_running"),
        "the TUI must not exit when the kernel dies"
    );

    // The composer must still accept input: the loop is alive, not frozen.
    harness
        .inject_keys(b"/reconn")
        .expect("type after the crash");
    harness
        .wait_for_text("/reconn", Duration::from_secs(10))
        .expect("typed input must echo, proving the event loop still runs");

    // Finish the command and let the restart run.
    harness
        .inject_keys(b"ect\r")
        .expect("submit /reconnect");
    harness
        .wait_for_text(RESTART_TOAST, Duration::from_secs(30))
        .expect("the restart must announce itself");

    // The new kernel is up: the toast reports the outcome, and typing works.
    harness
        .wait_until(
            "kernel restart to resolve",
            Duration::from_secs(120),
            |h| {
                h.contains_text("Kernel restarted")
                    || h.contains_text("Could not restart the OpenBuild kernel")
            },
        )
        .expect("the restart must resolve one way or the other");
    assert!(
        harness.contains_text("Kernel restarted"),
        "the restart failed; screen:\n{}",
        harness.screen_contents()
    );

    harness.inject_keys(keys::CTRL_C).ok();
}
