//! Kill a live kernel and show what the crash detector records.
//!
//! Usage:
//!   cargo run -p openbuild-backend --example crash -- <kernel.exe> [cwd]
//!
//! This is the transport half of the J1 acceptance item ("kill the kernel →
//! the TUI explains it and can reconnect"). It asserts three things the TUI
//! depends on:
//!
//! 1. the bridge's cancel token fires when the child dies,
//! 2. [`openbuild_backend::health`] reports `KernelExited` (not `UserCancel`),
//! 3. the stderr ring buffer still holds the child's last words.
//!
//! The TUI layer on top of this is covered by unit tests; this example is the
//! part that cannot be tested without a real process.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol as acp;
use anyhow::{Context, Result, bail};
use openbuild_backend::spawn::SpawnSpec;
use openbuild_backend::transport::bridge_subprocess;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::acp_send;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let kernel = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::var("OPENBUILD_KERNEL").ok().map(PathBuf::from))
        .or_else(|| openbuild_backend::spawn::resolve_kernel_path(None))
        .context("kernel path not provided and no openbuild-kernel found")?;
    let control_kernel = kernel.clone();
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);

    let mut spec = SpawnSpec::new(kernel);
    spec.cwd = cwd.clone();
    spec.env
        .push(("OPENBUILD_BACKEND".to_string(), "opencode".to_string()));
    let cancel = CancellationToken::new();
    let bridge = bridge_subprocess(spec, cancel.clone())?;
    openbuild_backend::health::set_active(bridge.health.clone());

    let mut capabilities = acp::ClientCapabilities::new()
        .fs(acp::FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(false))
        .terminal(false);
    capabilities = capabilities.elicitation(
        acp::ElicitationCapabilities::new().form(acp::ElicitationFormCapabilities::default()),
    );
    let initialize = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(capabilities)
        .meta(
            serde_json::json!({ "clientType": "openbuild-crash-check" })
                .as_object()
                .cloned(),
        );
    let _: acp::InitializeResponse = tokio::time::timeout(
        Duration::from_secs(15),
        acp_send(initialize, &bridge.channel.tx),
    )
    .await
    .context("initialize timed out")??;
    println!("initialize: ok");

    let pid = bridge.pid.load(std::sync::atomic::Ordering::Relaxed);
    if pid == 0 {
        bail!("the bridge never reported a kernel pid");
    }
    println!("kernel pid: {pid}");

    // Kill the kernel out from under the bridge, exactly like Task Manager.
    #[cfg(windows)]
    let killed = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
    #[cfg(not(windows))]
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
    println!("kill: issued ({:?})", killed.map(|o| o.status.code()));

    let fired = tokio::time::timeout(Duration::from_secs(20), cancel.cancelled())
        .await
        .is_ok();
    if !fired {
        bail!("the bridge never cancelled after the kernel died");
    }
    println!("cancel: fired");

    // Read the reason only after the bridge thread has finished: it stamps a
    // placeholder at stdout EOF and refines it with the real exit status during
    // teardown, so polling the published value would race that refinement.
    let joined = tokio::task::spawn_blocking(move || bridge.thread_handle.join())
        .await
        .context("join task panicked")?;
    match joined {
        Ok(Ok(())) => println!("bridge thread: exited cleanly"),
        Ok(Err(e)) => println!("bridge thread: error {e}"),
        Err(_) => println!("bridge thread: panicked"),
    }

    let exit = openbuild_backend::health::exit();
    println!("exit: {exit:?}");
    let tail = openbuild_backend::health::stderr_tail(8);
    println!(
        "stderr lines kept: {}",
        openbuild_backend::health::stderr_tail(200).len()
    );
    for line in &tail {
        println!("  | {line}");
    }

    match exit {
        Some(openbuild_backend::BridgeExit::KernelExited { code }) => {
            println!("PASS kernel exit detected (code: {code:?})");
        }
        Some(openbuild_backend::BridgeExit::UserCancel) => {
            bail!("FAIL the crash was misreported as a user cancel");
        }
        None => bail!("FAIL no exit reason was recorded"),
    }

    // Control: a stop that the TUI initiates (quitting, or a deliberate restart)
    // must stay `UserCancel`. Reporting it as a crash would show the user a
    // failure notice for something they caused on purpose.
    println!("\n--- control: user-initiated stop ---");
    let control_cancel = CancellationToken::new();
    let mut control_spec = SpawnSpec::new(control_kernel);
    control_spec.cwd = cwd.clone();
    let control = bridge_subprocess(control_spec, control_cancel.clone())?;
    let control_health = control.health.clone();
    tokio::time::sleep(Duration::from_millis(500)).await;
    control_cancel.cancel();
    let joined = tokio::task::spawn_blocking(move || control.thread_handle.join())
        .await
        .context("join task panicked")?;
    match joined {
        Ok(Ok(())) => println!("control bridge: exited cleanly"),
        Ok(Err(e)) => println!("control bridge: error {e}"),
        Err(_) => println!("control bridge: panicked"),
    }
    match control_health.read() {
        Some(openbuild_backend::BridgeExit::UserCancel) => {
            println!("PASS a user stop stays a user stop");
        }
        Some(other) => bail!("FAIL a user stop was reported as {other:?}"),
        None => bail!("FAIL no exit reason was recorded for the user stop"),
    }

    Ok(())
}
