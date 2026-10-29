//! End-to-end check for the HTTP side channel.
//!
//! `cargo run -p openbuild-backend --example http_feed -- <kernel> <cwd>`
//!
//! The example drives the real path the TUI uses:
//!
//! 1. initialize the kernel with `clientCapabilities._meta["openbuild/http-feed"]`;
//! 2. read the endpoint the kernel advertises for the server that owns the ACP
//!    session (a side-car `serve` process would have its own event bus);
//! 3. subscribe to that endpoint and wait for `server.connected`;
//! 4. create a session over ACP and assert that a `session.*` event arrives on
//!    the HTTP feed. That is the proof the feed shares the conversation's bus,
//!    not merely that a server accepts the password.
//!
//! A prompt round-trip is optional: set `OPENBUILD_PROMPT` when model auth is
//! configured. The session event above does not need a model call.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol as acp;
use anyhow::{Context, Result, bail};
use openbuild_backend::http_feed::{self, Endpoint};
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

    let mut spec = openbuild_backend::spawn::default_spec(Some(kernel))
        .context("kernel spec could not be resolved")?;
    spec.cwd = cwd.clone();
    let cancel = CancellationToken::new();
    let bridge = bridge_subprocess(spec, cancel.clone())?;
    let mut rx = bridge.channel.rx;
    // ACP notifications are fire-and-forget; drain them so the bridge never
    // blocks on a full channel while this example waits on the HTTP feed.
    let notifications = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if let AcpClientMessage::SessionNotification(notification) = message {
                let _ = notification.response_tx.send(Ok(()));
            }
        }
    });

    let capabilities = acp::ClientCapabilities::new()
        .fs(acp::FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(false))
        .terminal(false)
        .meta(
            serde_json::json!({ "openbuild/http-feed": true })
                .as_object()
                .cloned(),
        );
    let initialize = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(capabilities)
        .meta(
            serde_json::json!({ "clientType": "openbuild-http-feed-example" })
                .as_object()
                .cloned(),
        );
    let response: acp::InitializeResponse = tokio::time::timeout(
        Duration::from_secs(15),
        acp_send(initialize, &bridge.channel.tx),
    )
    .await
    .context("initialize timed out")??;

    let endpoint: Endpoint = response
        .meta
        .as_ref()
        .and_then(|meta| meta.get("openbuild/httpFeed"))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .context(
            "the kernel did not advertise openbuild/httpFeed; rebuild the kernel with the opt-in patch",
        )?;
    println!("endpoint: {}", endpoint.base_url);

    let mut feed = http_feed::connect(endpoint, None)
        .await
        .context("connect to the kernel event feed")?;
    let auth_prefix = {
        let header = feed.auth_header();
        header[..header.len().min(12)].to_owned()
    };
    println!("auth:     {auth_prefix}…");

    let events = feed.take_events().expect("events");
    let first = tokio::time::timeout(Duration::from_secs(15), events.recv())
        .await
        .context("no event arrived within 15s")?;
    let first = first.context("the event stream closed without sending anything")?;
    let first_kind = first
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("<untyped>");
    println!("EVENT {first_kind}");
    if first_kind != "server.connected" {
        println!("note: first event was not server.connected");
    }

    let new_session: acp::NewSessionResponse = tokio::time::timeout(
        Duration::from_secs(30),
        acp_send(acp::NewSessionRequest::new(cwd.clone()), &bridge.channel.tx),
    )
    .await
    .context("session/new timed out")??;
    println!("SESSION {}", new_session.session_id.0);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw_session = false;
    while tokio::time::Instant::now() < deadline {
        let event = tokio::time::timeout_at(deadline, events.recv()).await;
        let Ok(Some(event)) = event else {
            break;
        };
        let kind = event
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("<untyped>");
        println!("EVENT {kind}");
        if kind.starts_with("session.") {
            saw_session = true;
            break;
        }
    }
    if !saw_session {
        bail!(
            "no session.* event arrived after session/new; the feed is not subscribed to the ACP session's event bus"
        );
    }
    println!("PASS the HTTP side channel streams the ACP session's events");

    if let Ok(text) = std::env::var("OPENBUILD_PROMPT") {
        let prompt = acp::PromptRequest::new(
            new_session.session_id.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
        )
        .meta(
            serde_json::json!({ "promptId": format!("http-feed-{}", std::process::id()) })
                .as_object()
                .cloned(),
        );
        match tokio::time::timeout(
            Duration::from_secs(120),
            acp_send(prompt, &bridge.channel.tx),
        )
        .await
        {
            Ok(Ok(response)) => println!("PROMPT {:?}", response.stop_reason),
            Ok(Err(error)) => println!("PROMPT_ERROR {error}"),
            Err(_) => println!("PROMPT_TIMEOUT"),
        }
    }

    let status = feed.status();
    println!(
        "STATUS {} received={} dropped={} reconnects={} last={:?}",
        status.state.label(),
        status.received,
        status.dropped,
        status.reconnects,
        status.last_event
    );
    feed.shutdown();
    cancel.cancel();
    notifications.abort();
    let _ = bridge.thread_handle.join();
    Ok(())
}
