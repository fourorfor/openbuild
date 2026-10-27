//! End-to-end ACP handshake against a real openbuild kernel.
//!
//! Usage:
//!   cargo run -p openbuild-backend --example handshake -- <kernel.exe> [cwd]
//!
//! This is the M0 hard gate from the implementation plan: initialize,
//! session/new, and (when OPENBUILD_PROMPT is set) one prompt round-trip.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol as acp;
use anyhow::{Context, Result};
use openbuild_backend::spawn::SpawnSpec;
use openbuild_backend::transport::bridge_subprocess;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpClientMessage, acp_send};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let kernel = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::var("OPENBUILD_KERNEL").ok().map(PathBuf::from))
        .or_else(|| openbuild_backend::spawn::resolve_kernel_path(None))
        .context("kernel path not provided and no openbuild-kernel found")?;
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);

    println!("kernel: {}", kernel.display());
    println!("cwd:    {}", cwd.display());

    let mut spec = SpawnSpec::new(kernel);
    spec.cwd = cwd.clone();
    let cancel = CancellationToken::new();
    let bridge = bridge_subprocess(spec, cancel.clone())?;
    let mut rx = bridge.channel.rx;
    let notifications = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            match message {
                AcpClientMessage::SessionNotification(notification) => {
                    if let Ok(value) = serde_json::to_value(&notification.request) {
                        println!("NOTIFY {}", serde_json::to_string(&value)?);
                    }
                    // Session notifications are fire-and-forget; ack so grok's
                    // oneshot path never leaks.
                    let _ = notification.response_tx.send(Ok(()));
                }
                AcpClientMessage::RequestPermission(permission) => {
                    println!(
                        "PERMISSION {}",
                        serde_json::to_string(&permission.request)?
                    );
                }
                other => println!("MESSAGE {other:?}"),
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut capabilities = acp::ClientCapabilities::new()
        .fs(
            acp::FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(false),
        )
        .terminal(false);
    capabilities = capabilities.elicitation(
        acp::ElicitationCapabilities::new().form(acp::ElicitationFormCapabilities::default()),
    );
    let initialize = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(capabilities)
        .meta(
            serde_json::json!({ "clientType": "openbuild-handshake" })
                .as_object()
                .cloned(),
        );
    let response: acp::InitializeResponse = tokio::time::timeout(
        Duration::from_secs(15),
        acp_send(initialize, &bridge.channel.tx),
    )
    .await
    .context("initialize timed out")??;
    println!(
        "INITIALIZE {}",
        serde_json::to_string(&response)?
    );
    println!(
        "PROFILE {:?}",
        openbuild_backend::BackendProfile::detect(&response, None)
    );

    let new_session: acp::NewSessionResponse = tokio::time::timeout(
        Duration::from_secs(30),
        acp_send(
            acp::NewSessionRequest::new(cwd.clone()),
            &bridge.channel.tx,
        ),
    )
    .await
    .context("session/new timed out")??;
    println!(
        "SESSION {}",
        serde_json::to_string(&new_session)?
    );

    if let Ok(text) = std::env::var("OPENBUILD_PROMPT") {
        let prompt_id = format!("openbuild-handshake-{}", std::process::id());
        let meta = serde_json::json!({ "promptId": prompt_id })
            .as_object()
            .cloned();
        let prompt = acp::PromptRequest::new(
            new_session.session_id.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
        )
        .meta(meta);
        match tokio::time::timeout(
            Duration::from_secs(120),
            acp_send(prompt, &bridge.channel.tx),
        )
        .await
        {
            Ok(Ok(response)) => println!(
                "PROMPT {}",
                serde_json::to_string(&response)?
            ),
            Ok(Err(error)) => println!("PROMPT_ERROR {error}"),
            Err(_) => println!("PROMPT_TIMEOUT"),
        }
    }

    cancel.cancel();
    notifications.abort();
    let _ = bridge.thread_handle.join();
    Ok(())
}