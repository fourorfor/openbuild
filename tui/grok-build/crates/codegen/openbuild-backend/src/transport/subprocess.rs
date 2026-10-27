//! ACP over a child process's stdio (NDJSON / JSON-RPC 2.0).
//!
//! This mirrors the proven `leader_bridge` structure: a dedicated OS thread
//! with a current-thread runtime + LocalSet, two simplex pipes, and the
//! upstream `ClientSideConnection` doing JSON-RPC serialization. The only
//! difference is the byte source: child stdout/stdin instead of leader IPC.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use agent_client_protocol as acp;
use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, simplex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{
    AcpClientChannel, AcpGatewayReceiver, AcpGatewaySender, LineBufferedRead, acp_channels,
};

use crate::health::{BridgeExit, KernelHealth, StderrBuffer};
use crate::spawn::SpawnSpec;

const MAX_BUF: usize = 8 * 1024 * 1024;
const STDERR_LINES: usize = 200;
const KILL_GRACE: Duration = Duration::from_secs(3);
/// How long a stdout EOF waits for `child.wait()` to report the real exit status.
///
/// A closed pipe must never leave the TUI waiting, so the fallback cancel below
/// is unconditional — but cancelling immediately would race the status arm and
/// the crash notice would have no exit code. The child is already dead when its
/// stdout closes, so `wait()` resolves well inside this window.
const EOF_SETTLE: Duration = Duration::from_millis(250);

/// Live ACP channel plus the handles needed to stop and inspect the kernel.
pub struct SubprocessBridge {
    pub channel: AcpClientChannel,
    pub cancel: CancellationToken,
    pub thread_handle: thread::JoinHandle<Result<()>>,
    /// Recent stderr lines (bounded ring buffer) for `/debug` and crash reports.
    pub stderr: StderrBuffer,
    /// Kernel process id, available once the child has spawned.
    pub pid: Arc<AtomicU32>,
    /// Exit reason + stderr, shared with the TUI so a crash can be explained
    /// and a reconnect can reuse the same buffer.
    pub health: KernelHealth,
}

/// Spawn `spec.program` and bridge its stdio into a typed ACP client channel.
pub fn bridge_subprocess(spec: SpawnSpec, cancel: CancellationToken) -> Result<SubprocessBridge> {
    if !spec.program.is_file() {
        bail!("kernel executable not found: {}", spec.program.display());
    }

    let health = KernelHealth::new();
    let stderr = health.stderr.clone();
    let pid = Arc::new(AtomicU32::new(0));
    let (client_channel, agent_channel) = acp_channels();

    let stderr_for_thread = stderr.clone();
    let pid_for_thread = pid.clone();
    let health_for_thread = health.clone();
    let bridge_cancel = cancel.clone();
    let thread_handle = thread::Builder::new()
        .name("openbuild-kernel-bridge".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build kernel bridge runtime")?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let mut command = tokio::process::Command::new(&spec.program);
                command
                    .args(&spec.args)
                    .current_dir(&spec.cwd)
                    .envs(spec.env.iter().cloned())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);

                let mut child = command.spawn().with_context(|| {
                    format!("failed to spawn kernel {}", spec.program.display())
                })?;
                pid_for_thread.store(child.id().unwrap_or(0), Ordering::Relaxed);

                let stdin = child.stdin.take().context("kernel stdin was not piped")?;
                let stdout = child.stdout.take().context("kernel stdout was not piped")?;
                let stderr_pipe = child.stderr.take().context("kernel stderr was not piped")?;

                let stderr_lines = stderr_for_thread.clone();
                let stderr_task = tokio::task::spawn_local(async move {
                    let mut lines = BufReader::new(stderr_pipe).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let mut guard = stderr_lines.lock().expect("stderr buffer poisoned");
                        if guard.len() >= STDERR_LINES {
                            guard.pop_front();
                        }
                        guard.push_back(line);
                    }
                });

                let (incoming_read, incoming_write) = simplex(MAX_BUF);
                let (outgoing_read, outgoing_write) = simplex(MAX_BUF);

                // stdout -> incoming pipe (what ClientSideConnection reads)
                let cancel_r = bridge_cancel.clone();
                let eof_notify = Arc::new(tokio::sync::Notify::new());
                let eof_for_reader = eof_notify.clone();
                let reader_task = tokio::task::spawn_local(async move {
                    let mut incoming_write = incoming_write;
                    let mut lines = BufReader::new(stdout).lines();
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel_r.cancelled() => break,
                            line = lines.next_line() => match line {
                                Ok(Some(line)) => {
                                    if incoming_write.write_all(line.as_bytes()).await.is_err()
                                        || incoming_write.write_all(b"\n").await.is_err()
                                    {
                                        break;
                                    }
                                }
                                _ => {
                                    // The kernel's stdout closed. Report it and let
                                    // the driver below cancel: the driver is the
                                    // only place that can wait for the real exit
                                    // status, and the TUI's notice wants that code.
                                    tracing::warn!("openbuild kernel stdout closed");
                                    eof_for_reader.notify_one();
                                    break;
                                }
                            }
                        }
                    }
                });

                // outgoing pipe -> child stdin (what ClientSideConnection writes)
                let cancel_w = bridge_cancel.clone();
                let writer_task = tokio::task::spawn_local(async move {
                    let mut stdin = stdin;
                    let mut reader = BufReader::new(outgoing_read);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        tokio::select! {
                            biased;
                            _ = cancel_w.cancelled() => break,
                            result = reader.read_line(&mut line) => match result {
                                Ok(0) => break,
                                Ok(_) => {
                                    if stdin.write_all(line.as_bytes()).await.is_err() {
                                        break;
                                    }
                                    // NDJSON requires a flush per frame.
                                    if stdin.flush().await.is_err() {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    // Closing stdin is the kernel's ownership lease end.
                    let _ = stdin.shutdown().await;
                });

                let gw_tx = AcpGatewaySender::new(agent_channel.tx).with_tracing(true);
                let incoming = LineBufferedRead::spawn_local(incoming_read.compat());
                let (conn, handle_io) = acp::ClientSideConnection::new(
                    gw_tx,
                    outgoing_write.compat_write(),
                    incoming,
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                let gw_rx = AcpGatewayReceiver::new(agent_channel.rx, conn).with_tracing(true);
                tokio::task::spawn_local(handle_io);
                tokio::task::spawn_local(gw_rx.run());
                tokio::task::yield_now().await;

                tokio::select! {
                    biased;
                    _ = bridge_cancel.cancelled() => {}
                    _ = eof_notify.notified() => {
                        // Give `wait()` a moment to reap the already-dead child so
                        // the crash notice can name the exit code.
                        let status = tokio::time::timeout(EOF_SETTLE, child.wait()).await;
                        let code = match status {
                            Ok(Ok(status)) => status.code(),
                            Ok(Err(e)) => {
                                tracing::warn!(error = %e, "openbuild kernel wait failed");
                                None
                            }
                            Err(_) => None,
                        };
                        tracing::warn!(code, "openbuild kernel stdout closed");
                        health_for_thread.record(BridgeExit::KernelExited { code });
                        bridge_cancel.cancel();
                    }
                    status = child.wait() => {
                        tracing::warn!(?status, "openbuild kernel exited");
                        // `child.wait()` resolves with the real status, so refine
                        // any placeholder recorded by the reader above.
                        health_for_thread.record(BridgeExit::KernelExited {
                            code: status.ok().and_then(|s| s.code()),
                        });
                        bridge_cancel.cancel();
                    }
                }

                reader_task.abort();
                writer_task.abort();
                stderr_task.abort();
                // The writer task owns stdin; aborting it drops the handle and
                // closes the pipe. Give the kernel a moment to exit cleanly,
                // then kill it so Windows never leaves an orphan.
                match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                    Ok(Ok(status)) => {
                        // Only refine a status we already classified as a crash:
                        // a user-initiated stop stays `UserCancel` even though the
                        // killed child also exits with a code.
                        if health_for_thread
                            .read()
                            .is_some_and(BridgeExit::is_kernel_exit)
                        {
                            health_for_thread.record(BridgeExit::KernelExited {
                                code: status.code(),
                            });
                        }
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "kernel wait failed"),
                    Err(_) => {
                        let _ = child.kill().await;
                    }
                }
                // A still-unset reason means the stop was initiated from the
                // outside (TUI shutdown or a listener restart), not a crash.
                if health_for_thread.read().is_none() {
                    health_for_thread.record(BridgeExit::UserCancel);
                }
                Ok(())
            })
        })
        .context("spawn kernel bridge thread")?;

    Ok(SubprocessBridge {
        channel: client_channel,
        cancel,
        thread_handle,
        stderr,
        pid,
        health,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_program_fails_before_spawning_a_thread() {
        let spec = SpawnSpec::new("definitely-not-a-real-kernel-xyz");
        let error = bridge_subprocess(spec, CancellationToken::new())
            .err()
            .expect("missing kernel must fail");
        assert!(error.to_string().contains("not found"));
    }
}
