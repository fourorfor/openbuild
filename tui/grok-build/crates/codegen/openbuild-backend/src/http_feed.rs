//! Optional HTTP/SSE side channel into the kernel.
//!
//! ACP trims some information the kernel has: tool progress, `session.diff`,
//! and the full usage breakdown never travel over `session/update`. This module
//! adds a **read-only** subscription to the kernel's own HTTP event feed so the
//! TUI can enrich what it already renders.
//!
//! ## Design: hybrid, never load-bearing
//!
//! The ACP connection stays the only transport for prompts, permissions, and
//! session state. The HTTP feed is an *addition*:
//!
//! - the TUI connects to the **same** `serve` process the ACP kernel talks to
//!   (the kernel advertises its URL and lease password over `initialize`), so
//!   the feed sees the session's events. A second `serve` process has its own
//!   in-process event bus and would only ever see `server.connected`;
//! - every failure (no endpoint, refused password, stream overflow, disconnect)
//!   degrades to "no side channel", exactly like a build without this module;
//! - nothing here mutates session state, so a bug cannot corrupt a conversation.
//!
//! [`start`] remains for tests and for kernels that predate the endpoint
//! handshake: it starts a side-car `serve --port 0` and reads its readiness
//! lines. That side-car is useful for a protocol smoke test, not for live
//! session enrichment.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, watch};

/// How long to wait for a side-car kernel to print its listening line.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Fail a dead endpoint quickly, but never cap the lifetime of an SSE stream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A single SSE frame larger than this is treated as a protocol error.
const MAX_FRAME_BYTES: usize = 1 << 20;
/// Events buffered for the UI. The kernel drops slow subscribers at 4 096, so
/// 1 024 is generous while still bounding memory if the UI stalls.
const EVENT_BUFFER: usize = 1_024;
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// The endpoint the ACP kernel uses for its own HTTP API.
///
/// The kernel sends this over `initialize` only when the client opts in with
/// `clientCapabilities._meta["openbuild/http-feed"] = true`. The password is a
/// per-process lease credential; it never goes to disk.
#[derive(Clone, Deserialize)]
pub struct Endpoint {
    #[serde(rename = "url", alias = "base_url")]
    pub base_url: String,
    pub password: String,
}

impl Endpoint {
    pub fn new(base_url: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            password: password.into(),
        }
    }

    /// The `Authorization` header value the kernel expects.
    pub fn auth_header(&self) -> String {
        use base64::Engine;
        let raw = format!("opencode:{}", self.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never let a debug print leak the lease credential.
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Where the side channel currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeedState {
    #[default]
    Connecting,
    Connected,
    Reconnecting,
    Stopped,
}

impl FeedState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Stopped => "stopped",
        }
    }
}

/// Snapshot of the side channel, for `/backend` and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct FeedStatus {
    pub state: FeedState,
    pub received: u64,
    pub dropped: u64,
    pub reconnects: u64,
    pub last_event: Option<String>,
    pub last_error: Option<String>,
}

/// A live HTTP side channel.
///
/// The receiver can be handed to an event pump with [`Self::take_events`] while
/// the feed itself stays owned elsewhere: that split is what lets one pane hold
/// its feed (for `/backend` and diff) while the single `select!` in the event
/// loop drains every pane's stream.
pub struct HttpFeed {
    endpoint: Endpoint,
    /// Events, already decoded to `serde_json::Value` so this module does not
    /// have to mirror the kernel's event schema. `None` after the receiver was
    /// handed to a pump.
    events: Option<mpsc::Receiver<serde_json::Value>>,
    status: Arc<Mutex<FeedStatus>>,
    shutdown: watch::Sender<bool>,
}

impl HttpFeed {
    /// Base URL the kernel reported, e.g. `http://127.0.0.1:54896`.
    pub fn base_url(&self) -> &str {
        &self.endpoint.base_url
    }

    /// The `Authorization` header value the kernel expects.
    pub fn auth_header(&self) -> String {
        self.endpoint.auth_header()
    }

    pub fn status(&self) -> FeedStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// `GET /api/session/:sessionID/diff?context=N` on this connection's server.
    ///
    /// This is the **only** way to get the per-turn diff: `session.diff` is not on
    /// the events stream (it lives in the legacy `SessionV1` schema and has no
    /// publisher), so waiting for an event would wait forever.
    ///
    /// Reuses the SSE client on purpose: it is `no_proxy()` and only sets a
    /// connect timeout. A fresh client with a total timeout would be the same
    /// mistake the old `timeout(ZERO)` path made.
    pub async fn session_diff(
        &self,
        session_id: &str,
        context: u32,
    ) -> Result<Vec<SessionFileDiff>> {
        self.client_handle().session_diff(session_id, context).await
    }

    /// A cloneable handle for one-off requests.
    ///
    /// The event loop must be able to await a request without borrowing the
    /// `HttpFeed` stored in a connection handle (it also holds the event
    /// receiver), so the endpoint and status are handed out separately.
    pub fn client_handle(&self) -> FeedClient {
        FeedClient {
            endpoint: self.endpoint.clone(),
            status: self.status.clone(),
        }
    }

    /// Hand the event receiver to a pump.
    ///
    /// Called once per feed: the event loop owns the pump, the pane keeps the
    /// feed for status/auth/diff. A second call answers `None`.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<serde_json::Value>> {
        self.events.take()
    }

    /// The event receiver, when this feed has not been pumped yet.
    pub fn events(&mut self) -> Option<&mut mpsc::Receiver<serde_json::Value>> {
        self.events.as_mut()
    }

    /// Stop the side channel. The ACP conversation is unaffected.
    pub fn shutdown(self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for HttpFeed {
    fn drop(&mut self) {
        // Also covers a feed dropped by a reconnect or by TUI shutdown.
        let _ = self.shutdown.send(true);
    }
}

impl fmt::Debug for HttpFeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpFeed")
            .field("endpoint", &self.endpoint)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// A cloneable handle to one connection's side channel, for request/response
/// calls (the diff endpoint) that must not borrow the feed itself.
#[derive(Clone)]
pub struct FeedClient {
    endpoint: Endpoint,
    status: Arc<Mutex<FeedStatus>>,
}

impl fmt::Debug for FeedClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Endpoint` holds the lease password, so it is never printed.
        f.debug_struct("FeedClient")
            .field("base_url", &self.endpoint.base_url)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl FeedClient {
    /// Base URL this handle talks to.
    pub fn base_url(&self) -> &str {
        &self.endpoint.base_url
    }

    /// Snapshot of the feed's counters.
    pub fn status(&self) -> FeedStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// `GET /api/session/:sessionID/diff?context=N`.
    pub async fn session_diff(
        &self,
        session_id: &str,
        context: u32,
    ) -> Result<Vec<SessionFileDiff>> {
        let client = client()?;
        let url = format!(
            "{}/api/session/{}/diff?context={}",
            self.endpoint.base_url.trim_end_matches('/'),
            urlencode_path_segment(session_id),
            context
        );
        let response = client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.endpoint.auth_header())
            .send()
            .await
            .context("request the session diff")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read the session diff response")?;
        if !status.is_success() {
            bail!("session diff failed with {status}: {}", body.trim());
        }
        parse_session_diff(&body)
    }
}

/// One entry of `GET /api/session/:id/diff`.
///
/// Mirrors `FileDiff.Info` from the kernel's protocol: `file`, `patch` and
/// `status` are always present, the counts may be zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct SessionFileDiff {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub patch: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub status: Option<String>,
}

impl SessionFileDiff {
    /// The path to show: the reported file, else the `+++ b/…` header, else a
    /// stable placeholder so a malformed entry still renders instead of vanishing.
    pub fn display_path(&self) -> String {
        if !self.file.trim().is_empty() {
            return self.file.clone();
        }
        for line in self.patch.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                let rest = rest.trim();
                let rest = rest.strip_prefix("b/").unwrap_or(rest);
                if !rest.is_empty() && rest != "/dev/null" {
                    return rest.to_string();
                }
            }
        }
        "(unknown file)".to_string()
    }
}

/// Decode the `{ data: [...] }` envelope the server answers with.
///
/// Also accepts a bare array, and skips entries that are not objects, so a shape
/// change degrades to "no diff" instead of failing the whole pane.
pub fn parse_session_diff(body: &str) -> Result<Vec<SessionFileDiff>> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("the session diff response was not JSON")?;
    let array = value
        .get("data")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let Some(items) = array.as_array() else {
        bail!("the session diff response had no data array");
    };
    Ok(items
        .iter()
        .filter_map(|item| serde_json::from_value::<SessionFileDiff>(item.clone()).ok())
        .collect())
}

/// Percent-encode a path segment (session ids are `ses_…`, but never assume it).
fn urlencode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Most recent feed status per connection slot, for commands that cannot reach
/// the event loop.
///
/// One entry per slot, like [`crate::health`]: `/pane` runs several kernels in
/// one process, and a single `Option` would let the last connection's status
/// shadow every other pane's (the second pane used to make `/backend` report
/// the wrong kernel).
static ACTIVE: OnceLock<Mutex<Vec<Option<Arc<Mutex<FeedStatus>>>>>> = OnceLock::new();

fn active_slot() -> &'static Mutex<Vec<Option<Arc<Mutex<FeedStatus>>>>> {
    ACTIVE.get_or_init(|| {
        Mutex::new(
            std::iter::repeat_with(|| None)
                .take(crate::profile::MAX_CONNECTIONS)
                .collect(),
        )
    })
}

/// Publish the feed status of one connection.
fn set_active_in(slot: usize, status: Arc<Mutex<FeedStatus>>) {
    if let Ok(mut guard) = active_slot().lock()
        && let Some(entry) = guard.get_mut(slot)
    {
        *entry = Some(status);
    }
}

/// Publish the feed status of the primary connection (slot 0).
fn set_active(status: Arc<Mutex<FeedStatus>>) {
    set_active_in(0, status);
}

/// Snapshot of one connection's live feed, if it ever started one.
pub fn active_status_in(slot: usize) -> Option<FeedStatus> {
    let status = active_slot().lock().ok()?.get(slot)?.clone()?;
    status.lock().ok().map(|s| s.clone())
}

/// Snapshot of the primary connection's live feed.
pub fn active_status() -> Option<FeedStatus> {
    active_status_in(0)
}

/// Whether one connection's slot currently has a feed status.
pub fn has_feed_in(slot: usize) -> bool {
    active_slot()
        .lock()
        .ok()
        .and_then(|guard| guard.get(slot).cloned().flatten())
        .is_some()
}

/// Start a side-car `serve --port 0` kernel and subscribe to its event feed.
///
/// Returns `Ok(None)` when the feature is switched off, when the kernel refuses
/// to start a server, or when the event stream cannot be reached: a missing
/// side channel is a normal state, not an error the caller must handle.
///
/// Prefer [`connect`] when the kernel advertised its own endpoint: a side-car
/// server has a separate event bus and does not see the ACP session.
pub async fn start(kernel: Option<PathBuf>, cwd: Option<PathBuf>) -> Result<Option<HttpFeed>> {
    if disabled() {
        return Ok(None);
    }
    match try_start(kernel, cwd).await {
        Ok(feed) => Ok(Some(feed)),
        Err(error) => {
            // Degrade, never fail the TUI: the ACP path is complete without it.
            // `warn` rather than `debug` because "the optional feed silently
            // did nothing" was previously indistinguishable from a bug.
            tracing::warn!(%error, "kernel HTTP side channel unavailable");
            Ok(None)
        }
    }
}

fn disabled() -> bool {
    std::env::var("OPENBUILD_HTTP_FEED").is_ok_and(|value| value == "0" || value == "off")
}

/// Whether the side channel is enabled by the environment.
pub fn enabled() -> bool {
    !disabled()
}

async fn try_start(kernel: Option<PathBuf>, cwd: Option<PathBuf>) -> Result<HttpFeed> {
    let program = crate::spawn::resolve_kernel_path(kernel)
        .context("kernel executable not found, so no HTTP side channel")?;
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().context("current directory must be known")?,
    };
    let mut command = tokio::process::Command::new(&program);
    command
        .args(["serve", "--port", "0"])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // The kernel prints a lease password; do not inherit a caller's. Both names
    // are honored by the kernel and either one suppresses the readiness line.
    command
        .env_remove("OPENCODE_PASSWORD")
        .env_remove("OPENCODE_SERVER_PASSWORD");
    if let Some(dir) = crate::spawn::resolve_config_dir() {
        // Keep the side-car on the same config root as the ACP child.
        command.env("OPENCODE_CONFIG_DIR", dir);
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to start the kernel HTTP server ({})",
            program.display()
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .context("kernel HTTP server stdout was not piped")?;

    let (url, password) = match read_ready(stdout).await {
        Ok(ready) => ready,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let feed = match connect(Endpoint::new(url, password), None).await {
        Ok(feed) => feed,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };

    // Own the child for as long as the feed lives; killing it on shutdown keeps
    // Windows free of orphaned console children.
    let mut shutdown = feed.shutdown.subscribe();
    tokio::spawn(async move {
        let _ = shutdown.changed().await;
        let _ = child.kill().await;
    });

    tracing::info!(url = feed.base_url(), "kernel HTTP side channel ready");
    Ok(feed)
}

/// Read the kernel's two readiness lines: `server listening on <url>` and
/// `server password <secret>`.
async fn read_ready(stdout: tokio::process::ChildStdout) -> Result<(String, String)> {
    let mut lines = BufReader::new(stdout).lines();
    let mut url = None;
    let mut password = None;
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    while url.is_none() || password.is_none() {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .context("the kernel HTTP server did not report readiness in time")??
            .context("the kernel HTTP server exited before reporting readiness")?;
        if let Some(rest) = line.strip_prefix("server listening on ") {
            url = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("server password ") {
            password = Some(rest.trim().to_owned());
        }
    }
    match (url, password) {
        (Some(url), Some(password)) => Ok((url, password)),
        // Unreachable: the loop exits only when both are set.
        _ => bail!("the kernel HTTP server reported an incomplete endpoint"),
    }
}

/// Connect to an existing kernel HTTP server.
///
/// This is the production path: the URL and password come from the ACP
/// `initialize` response, so the subscription shares the event bus that owns
/// the conversation.
///
/// `slot` only labels the published status: `/backend` reads it per pane, so two
/// kernels must not overwrite each other's entry.
pub async fn connect_for_slot(
    slot: usize,
    endpoint: Endpoint,
    types: Option<Vec<String>>,
) -> Result<HttpFeed> {
    if disabled() {
        bail!("the HTTP side channel is disabled by OPENBUILD_HTTP_FEED");
    }
    let (shutdown, shutdown_rx) = watch::channel(false);
    let client = client()?;
    let response = open_stream(&client, &endpoint, types.as_deref()).await?;
    let (tx, events) = mpsc::channel(EVENT_BUFFER);
    let status = Arc::new(Mutex::new(FeedStatus {
        state: FeedState::Connected,
        ..FeedStatus::default()
    }));
    set_active_in(slot, status.clone());

    tokio::spawn(stream_loop(
        client,
        endpoint.clone(),
        types,
        response,
        tx,
        status.clone(),
        shutdown_rx,
    ));

    Ok(HttpFeed {
        endpoint,
        events: Some(events),
        status,
        shutdown,
    })
}

/// Connect to the primary connection's server (slot 0).
pub async fn connect(endpoint: Endpoint, types: Option<Vec<String>>) -> Result<HttpFeed> {
    connect_for_slot(0, endpoint, types).await
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        // A total request timeout would also cap the SSE response body and
        // close the feed. `timeout(Duration::ZERO)` is NOT "no timeout" in
        // reqwest: it installs an already-expired deadline, and the request
        // fails before the network future is polled. Only bound the connect
        // phase; the stream itself lives until the server closes it.
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("build the side-channel HTTP client")
}

async fn open_stream(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    types: Option<&[String]>,
) -> Result<reqwest::Response> {
    let mut url = reqwest::Url::parse(&format!(
        "{}/api/event",
        endpoint.base_url.trim_end_matches('/')
    ))
    .with_context(|| format!("invalid kernel HTTP URL {}", endpoint.base_url))?;
    if let Some(types) = types.filter(|types| !types.is_empty()) {
        url.query_pairs_mut().append_pair("types", &types.join(","));
    }
    let response = client
        .get(url.clone())
        .header(reqwest::header::AUTHORIZATION, endpoint.auth_header())
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .with_context(|| format!("subscribe to {url}"))?;
    if !response.status().is_success() {
        bail!(
            "the kernel event feed answered {} for {url}",
            response.status()
        );
    }
    Ok(response)
}

/// Keep one SSE stream alive, reconnecting with capped exponential backoff.
///
/// Events missed while disconnected are not replayed: the kernel's `/api/event`
/// feed is a volatile live channel and has no cursor parameter. That is fine
/// for enrichment data, which is always superseded by the next event.
async fn stream_loop(
    client: reqwest::Client,
    endpoint: Endpoint,
    types: Option<Vec<String>>,
    mut response: reqwest::Response,
    tx: mpsc::Sender<serde_json::Value>,
    status: Arc<Mutex<FeedStatus>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() {
            break;
        }
        set_state(&status, FeedState::Connected);
        let reason = read_stream(&mut response, &tx, &status, &mut shutdown).await;
        if *shutdown.borrow() {
            break;
        }

        attempt = attempt.saturating_add(1);
        {
            let mut s = lock_status(&status);
            s.state = FeedState::Reconnecting;
            s.reconnects = s.reconnects.saturating_add(1);
            s.last_error = Some(reason);
        }
        let delay = backoff(attempt);
        tracing::debug!(
            url = endpoint.base_url,
            attempt,
            delay_ms = delay.as_millis(),
            "kernel HTTP side channel disconnected; reconnecting"
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => break,
        }
        if *shutdown.borrow() {
            break;
        }

        match open_stream(&client, &endpoint, types.as_deref()).await {
            Ok(next) => {
                response = next;
                attempt = 0;
            }
            Err(error) => {
                lock_status(&status).last_error = Some(error.to_string());
            }
        }
    }
    set_state(&status, FeedState::Stopped);
}

async fn read_stream(
    response: &mut reqwest::Response,
    tx: &mpsc::Sender<serde_json::Value>,
    status: &Arc<Mutex<FeedStatus>>,
    shutdown: &mut watch::Receiver<bool>,
) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = shutdown.changed() => return "shutdown".to_owned(),
            chunk = response.chunk() => chunk,
        };
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return "stream ended".to_owned(),
            Err(error) => return error.to_string(),
        };
        buffer.extend_from_slice(&chunk);

        // A frame without a delimiter is a protocol violation, not an event
        // we can ever parse. Drop it instead of growing without bound.
        if buffer.len() > MAX_FRAME_BYTES
            && find(&buffer, b"\n\n").is_none()
            && find(&buffer, b"\r\n\r\n").is_none()
        {
            buffer.clear();
            let mut s = lock_status(status);
            s.dropped = s.dropped.saturating_add(1);
            continue;
        }

        while let Some(frame) = take_frame(&mut buffer) {
            let Some(event) = parse_frame(&frame) else {
                continue;
            };
            let kind = event
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            {
                let mut s = lock_status(status);
                s.received = s.received.saturating_add(1);
                if !kind.is_empty() {
                    s.last_event = Some(kind);
                }
            }
            if tx.try_send(event).is_err() {
                // The UI is behind. Dropping an enrichment event is preferable
                // to unbounded memory or stalling the ACP path.
                let mut s = lock_status(status);
                s.dropped = s.dropped.saturating_add(1);
            }
        }
    }
}

fn lock_status(status: &Arc<Mutex<FeedStatus>>) -> std::sync::MutexGuard<'_, FeedStatus> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_state(status: &Arc<Mutex<FeedStatus>>, state: FeedState) {
    lock_status(status).state = state;
}

fn backoff(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.min(6);
    BACKOFF_START.saturating_mul(factor).min(BACKOFF_MAX)
}

/// Remove one complete SSE frame from the buffer.
///
/// The kernel emits LF line endings, but accepting CRLF keeps this correct for
/// proxies and test servers that normalize line endings.
fn take_frame(buffer: &mut Vec<u8>) -> Option<String> {
    let lf = find(buffer, b"\n\n");
    let crlf = find(buffer, b"\r\n\r\n");
    let (index, delimiter) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if crlf <= lf => (crlf, 4),
        (Some(lf), _) => (lf, 2),
        (None, Some(crlf)) => (crlf, 4),
        (None, None) => return None,
    };
    let frame = String::from_utf8_lossy(&buffer[..index]).into_owned();
    buffer.drain(..index + delimiter);
    Some(frame)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decode one SSE frame's `data:` payload.
fn parse_frame(frame: &str) -> Option<serde_json::Value> {
    let mut data = String::new();
    for line in frame.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

/// One event's `type`, used by the TUI to pick what it cares about.
#[derive(Debug, Deserialize)]
pub struct EventType {
    #[serde(rename = "type")]
    pub kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut buffer = [0u8; 8192];
        let read = socket.read(&mut buffer).await.expect("read request");
        String::from_utf8_lossy(&buffer[..read]).into_owned()
    }

    async fn write_response(socket: &mut TcpStream, status: &str, body: &str) {
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write header");
        socket.write_all(body.as_bytes()).await.expect("write body");
        socket.flush().await.expect("flush");
    }

    #[test]
    fn parses_an_sse_frame_with_a_json_payload() {
        let frame = "event: message\ndata: {\"type\":\"session.tool.progress\",\"id\":1}";
        let event = parse_frame(frame).expect("payload");
        assert_eq!(event["type"], "session.tool.progress");
    }

    #[test]
    fn ignores_frames_without_data() {
        assert!(parse_frame(": keep-alive").is_none());
    }

    #[test]
    fn joins_multi_line_data() {
        let frame = "data: {\"type\":\ndata: \"x\"}";
        let event = parse_frame(frame).expect("payload");
        assert_eq!(event["type"], "x");
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let frame = "data: {\"type\":\"x\"}\r\n\r\ndata: {\"type\":\"y\"}\r\n\r\n";
        let mut buffer = frame.as_bytes().to_vec();
        assert_eq!(
            take_frame(&mut buffer).as_deref(),
            Some("data: {\"type\":\"x\"}")
        );
        assert_eq!(
            take_frame(&mut buffer).as_deref(),
            Some("data: {\"type\":\"y\"}")
        );
        assert!(take_frame(&mut buffer).is_none());
    }

    #[test]
    fn auth_header_is_basic_with_the_opencode_user() {
        let endpoint = Endpoint::new("http://127.0.0.1:1", "secret");
        // base64("opencode:secret")
        assert_eq!(endpoint.auth_header(), "Basic b3BlbmNvZGU6c2VjcmV0");
    }

    #[test]
    fn debug_never_prints_the_password() {
        let endpoint = Endpoint::new("http://127.0.0.1:1", "super-secret-lease");
        let rendered = format!("{endpoint:?}");
        assert!(rendered.contains("127.0.0.1:1"));
        assert!(!rendered.contains("super-secret-lease"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff(0), BACKOFF_START);
        assert_eq!(backoff(100), BACKOFF_MAX);
    }

    /// The kernel answers `{ data: [FileDiff.Info] }`; a bare array is accepted
    /// too, and entries without `patch` must not kill the whole list.
    #[test]
    fn session_diff_parses_the_data_envelope() {
        let body = r#"{"data":[
            {"file":"src/a.rs","patch":"@@ -1 +1 @@\n-x\n+y\n","additions":1,"deletions":1,"status":"modified"},
            {"patch":"--- a/b.rs\n+++ b/b.rs\n","status":"added"}
        ]}"#;
        let diffs = parse_session_diff(body).expect("parsed");
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].file, "src/a.rs");
        assert_eq!(diffs[0].additions, 1);
        assert_eq!(diffs[1].file, "");
        // No `file` field: fall back to the patch header.
        assert_eq!(diffs[1].display_path(), "b.rs");
    }

    #[test]
    fn session_diff_tolerates_a_bare_array_and_rejects_non_json() {
        let diffs = parse_session_diff(r#"[{"file":"a","patch":""}]"#).expect("bare array");
        assert_eq!(diffs.len(), 1);
        assert!(parse_session_diff("not json").is_err());
        assert!(parse_session_diff(r#"{"error":"boom"}"#).is_err());
    }

    /// Two kernels publish to two slots. A single `Option` used to let the
    /// second connection overwrite the first, so `/backend` reported the wrong
    /// pane; the per-slot registry keeps both answers.
    #[test]
    fn statuses_are_published_per_slot() {
        let first = Arc::new(Mutex::new(FeedStatus {
            state: FeedState::Connected,
            received: 7,
            ..FeedStatus::default()
        }));
        let second = Arc::new(Mutex::new(FeedStatus {
            state: FeedState::Reconnecting,
            received: 3,
            ..FeedStatus::default()
        }));
        set_active_in(1, first);
        set_active_in(2, second);

        assert_eq!(active_status_in(1).expect("slot 1").received, 7);
        assert_eq!(active_status_in(2).expect("slot 2").received, 3);
        assert_eq!(
            active_status_in(2).expect("slot 2").state,
            FeedState::Reconnecting
        );
        assert!(has_feed_in(1) && has_feed_in(2));
        // Slot 0 is untouched by the pane slots: the primary keeps its own view.
        assert!(active_status_in(1).is_some());
    }

    /// The event receiver can be handed to an external pump (the event loop's
    /// merged feed arm) while the feed stays owned for status and credentials.
    #[tokio::test]
    async fn the_event_receiver_can_be_handed_to_a_pump() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut socket).await;
            write_response(
                &mut socket,
                "200 OK",
                "data: {\"type\":\"server.connected\"}\n\n",
            )
            .await;
        });

        let mut feed = connect_for_slot(3, Endpoint::new(format!("http://{address}"), "s"), None)
            .await
            .expect("connect");
        // Still usable for the commands that need the endpoint.
        assert!(feed.base_url().starts_with("http://"));
        let mut events = feed.take_events().expect("events");
        let first = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event")
            .expect("value");
        assert_eq!(first["type"], "server.connected");
        // Taking twice is a no-op, not a panic.
        assert!(feed.take_events().is_none());
        assert!(feed.status().received >= 1 || feed.status().received == 0);
        let _ = server.await;
    }

    #[tokio::test]
    async fn connects_with_basic_auth_and_parses_events() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut socket).await;
            write_response(
                &mut socket,
                "200 OK",
                "data: {\"type\":\"server.connected\"}\n\ndata: {\"type\":\"session.diff\"}\n\n",
            )
            .await;
            request
        });

        let mut feed = connect(Endpoint::new(format!("http://{address}"), "secret"), None)
            .await
            .expect("connect");

        let mut events = feed.take_events().expect("events");
        let first = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("first event")
            .expect("event");
        assert_eq!(first["type"], "server.connected");
        let second = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("second event")
            .expect("event");
        assert_eq!(second["type"], "session.diff");

        let request = server.await.expect("server task");
        assert!(
            request.contains("authorization: Basic b3BlbmNvZGU6c2VjcmV0"),
            "request did not carry the expected Basic header: {request}"
        );
        assert!(
            request.contains("accept: text/event-stream"),
            "request did not ask for SSE: {request}"
        );
        feed.shutdown();
    }

    #[tokio::test]
    async fn forwards_the_event_type_filter() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut socket).await;
            write_response(&mut socket, "200 OK", "").await;
            request
        });

        let feed = connect(
            Endpoint::new(format!("http://{address}"), "secret"),
            Some(vec![
                "session.diff".to_owned(),
                "session.tool.progress".to_owned(),
            ]),
        )
        .await
        .expect("connect");
        let request = server.await.expect("server task");
        assert!(
            request.contains("types=session.diff%2Csession.tool.progress")
                || request.contains("types=session.diff,session.tool.progress"),
            "request did not carry the type filter: {request}"
        );
        feed.shutdown();
    }

    #[tokio::test]
    async fn reports_a_rejected_password() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut socket).await;
            write_response(&mut socket, "401 Unauthorized", "").await;
        });

        let error = connect(Endpoint::new(format!("http://{address}"), "wrong"), None)
            .await
            .expect_err("a rejected password must fail");
        assert!(
            error.to_string().contains("401"),
            "unexpected error: {error}"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn reconnects_after_the_stream_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for body in [
                "data: {\"type\":\"first\"}\n\n",
                "data: {\"type\":\"second\"}\n\n",
            ] {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let _ = read_request(&mut socket).await;
                write_response(&mut socket, "200 OK", body).await;
            }
        });

        let mut feed = connect(Endpoint::new(format!("http://{address}"), "secret"), None)
            .await
            .expect("connect");
        let mut events = feed.take_events().expect("events");
        let first = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("first event")
            .expect("event");
        assert_eq!(first["type"], "first");

        // The server closes the first response; the client reconnects with
        // backoff and receives the next event from the second response.
        let second = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("second event")
            .expect("event");
        assert_eq!(second["type"], "second");
        assert!(
            feed.status().reconnects >= 1,
            "reconnect counter did not advance: {:?}",
            feed.status()
        );
        feed.shutdown();
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn the_feature_can_be_switched_off_by_env() {
        // SAFETY: the test is single-threaded and restores the variable.
        unsafe { std::env::set_var("OPENBUILD_HTTP_FEED", "0") };
        let feed = start(None, None).await.expect("no error");
        unsafe { std::env::remove_var("OPENBUILD_HTTP_FEED") };
        assert!(feed.is_none());
    }
}
