//! End-to-end check: drive `openbuild-server` as an ACP client and confirm the
//! relay reaches a real kernel.
//!
//! `cargo run -p openbuild-server --example relay -- <kernel> <cwd>`
//!
//! Asserts, in order:
//!   1. `initialize` answers with the kernel's own agent info;
//!   2. `session/new` returns a session id and the kernel's `configOptions`;
//!   3. a prompt streams at least one `session/update` back through the relay.
//!
//! Step 3 needs a working model/auth. When it fails the example still reports
//! steps 1-2 and exits 0, so this stays usable as a smoke test in a bare repo.

use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;

use acp::Agent;
use agent_client_protocol as acp;
use anyhow::{Context, Result, bail};
use tokio::io::BufReader;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Collects what the relayed kernel sends us.
#[derive(Default)]
struct Recorder {
    updates: std::cell::RefCell<Vec<String>>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for Recorder {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        // A smoke test cannot answer a prompt, so it cancels: the point is to
        // prove the request reached us through the relay.
        self.updates.borrow_mut().push("request_permission".into());
        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.updates.borrow_mut().push(format!("{:?}", args.update));
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local.run_until(run_relay()).await
}

async fn run_relay() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let kernel = args.next().map(PathBuf::from);
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir().context("current directory must be known")?);

    let server = std::env::var("OPENBUILD_SERVER").unwrap_or_else(|_| {
        let exe = std::env::current_exe().expect("current exe");
        exe.with_file_name(format!("openbuild-server{}", std::env::consts::EXE_SUFFIX))
            .display()
            .to_string()
    });
    println!("server: {server}");
    println!("cwd:    {}", cwd.display());

    let mut command = tokio::process::Command::new(&server);
    if let Some(kernel) = kernel.as_ref() {
        command.arg("--kernel").arg(kernel);
    }
    command
        .arg("--cwd")
        .arg(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {server}"))?;
    let stdin = child.stdin.take().context("server stdin was not piped")?;
    let stdout = child.stdout.take().context("server stdout was not piped")?;

    let recorder = Rc::new(Recorder::default());
    let (conn, io) = acp::ClientSideConnection::new(
        recorder.clone(),
        stdin.compat_write(),
        Box::pin(BufReader::new(stdout)).compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    tokio::task::spawn_local(io);

    let init = conn
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_info(acp::Implementation::new("openbuild-relay-smoke", "0.1.0")),
        )
        .await
        .context("initialize through the relay")?;
    println!(
        "INITIALIZE agent={:?} version={:?} loadSession={}",
        init.agent_info.as_ref().map(|info| info.name.clone()),
        init.agent_info.as_ref().map(|info| info.version.clone()),
        init.agent_capabilities.load_session,
    );
    if init.agent_info.is_none() {
        bail!("the relay did not forward the kernel's agent info");
    }

    let session = conn
        .new_session(acp::NewSessionRequest::new(&cwd))
        .await
        .context("session/new through the relay")?;
    println!(
        "SESSION {} configOptions={}",
        session.session_id.0,
        session
            .config_options
            .as_ref()
            .map(Vec::len)
            .unwrap_or_default()
    );

    let prompt = acp::PromptRequest::new(
        session.session_id.clone(),
        vec![acp::ContentBlock::Text(acp::TextContent::new(
            "Reply with exactly: relay ok",
        ))],
    );
    match conn.prompt(prompt).await {
        Ok(response) => {
            let updates = recorder.updates.borrow();
            println!(
                "PROMPT stopReason={:?} updates={} permissionPauses={}",
                response.stop_reason,
                updates.len(),
                updates
                    .iter()
                    .filter(|u| *u == "request_permission")
                    .count()
            );
            if updates.is_empty() {
                bail!("the relay forwarded no session/update messages");
            }
            println!("PASS the relay streams kernel updates to the external client");
        }
        Err(error) => {
            // Auth-free environments legitimately cannot prompt. Steps 1-2 are
            // the relay contract; say so instead of failing the smoke test.
            println!("PROMPT skipped: {error}");
            println!("PASS the relay forwards initialize and session/new");
        }
    }
    Ok(())
}
