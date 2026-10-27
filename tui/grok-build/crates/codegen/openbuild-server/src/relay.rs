//! The ACP relay: an agent-facing server that owns one kernel.
//!
//! Request/response traffic is forwarded verbatim. Notifications from the
//! kernel (`session/update`, and the kernel's own extension notifications) are
//! pushed to the external client, and permission requests are re-issued to it
//! so a human answers where they are typing.
//!
//! This deliberately does not translate update shapes. The TUI's renderer
//! adaptation lives in `openbuild-backend::adapter`; an external ACP client has
//! its own renderer and should see the kernel's own wire format.
//!
//! ## Why a direct `ClientSideConnection`
//!
//! `xai-acp-lib`'s typed message enum carries only the methods the TUI uses, so
//! `session/list`, `session/fork`, `session/resume`, and `session/close` cannot
//! travel over it. An external client expects the full advertised surface, so
//! this crate drives the ACP SDK connection directly.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;

use agent_client_protocol as acp;
use anyhow::{Context, Result, bail};
use tokio::io::BufReader;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Shared handle to the external client's connection.
///
/// The two connections are mutually referential (agent forwards to kernel,
/// client forwards back to agent), so exactly one of them is created first and
/// installed here before traffic starts.
type ExternalSlot = Rc<RefCell<Option<Rc<acp::AgentSideConnection>>>>;

/// Run the server: spawn a kernel, then serve ACP on stdio until the client
/// disconnects.
pub async fn serve(kernel: Option<PathBuf>, cwd: Option<PathBuf>) -> Result<()> {
    if let openbuild_backend::plugin::InstallOutcome::Failed { path, error } =
        openbuild_backend::plugin::ensure_uncensored_plugin()
    {
        tracing::warn!(
            path = path.as_ref().map(|path| path.display().to_string()),
            %error,
            "uncensored plugin install failed"
        );
    }
    let mut spec = openbuild_backend::spawn::default_spec(kernel).context(
        "openbuild kernel not found; set OPENBUILD_KERNEL or put openbuild-kernel.exe next to this executable",
    )?;
    if let Some(cwd) = cwd {
        spec.cwd = cwd;
    }

    let mut command = tokio::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(spec.env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn kernel {}", spec.program.display()))?;
    tracing::info!(pid = child.id(), "openbuild-server: kernel started");

    let kernel_stdin = child.stdin.take().context("kernel stdin was not piped")?;
    let kernel_stdout = child.stdout.take().context("kernel stdout was not piped")?;

    let external: ExternalSlot = Rc::new(RefCell::new(None));

    let (kernel_conn, kernel_io) = acp::ClientSideConnection::new(
        RelayClient {
            external: external.clone(),
        },
        kernel_stdin.compat_write(),
        Box::pin(BufReader::new(kernel_stdout)).compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    let kernel_conn = Rc::new(kernel_conn);

    let (ext_conn, ext_io) = acp::AgentSideConnection::new(
        RelayAgent {
            kernel: kernel_conn,
            cwd: spec.cwd.clone(),
        },
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    *external.borrow_mut() = Some(Rc::new(ext_conn));

    let kernel_io = tokio::task::spawn_local(kernel_io);
    let ext_io = tokio::task::spawn_local(ext_io);

    // The conversation ends when either side closes. The kernel exiting is the
    // usual cause (its stdin is owned by the client connection's writer).
    tokio::select! {
        status = child.wait() => {
            tracing::info!(?status, "openbuild-server: kernel exited");
        }
        _ = ext_io => {
            tracing::info!("openbuild-server: external client disconnected");
        }
        _ = kernel_io => {
            tracing::info!("openbuild-server: kernel transport closed");
        }
    }
    // Closing stdin is the kernel's ownership-lease signal; killing it makes
    // that immediate on Windows, where an orphaned console child would survive.
    let _ = child.kill().await;
    Ok(())
}

/// Kernel-facing client. Everything the kernel asks a client to do lands here.
struct RelayClient {
    external: ExternalSlot,
}

impl RelayClient {
    fn external(&self) -> Option<Rc<acp::AgentSideConnection>> {
        self.external.borrow().clone()
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for RelayClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let Some(external) = self.external() else {
            // The external side already went away: answer "cancelled" so the
            // kernel's tool call stops instead of hanging forever.
            return Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ));
        };
        external.request_permission(args).await
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let Some(external) = self.external() else {
            return Ok(());
        };
        external.session_notification(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        // The kernel's extension notifications (`x.ai/session_notification`,
        // `opencode/session/child_update`, …) carry conversation content, so
        // they are forwarded rather than dropped.
        let Some(external) = self.external() else {
            return Ok(());
        };
        external.ext_notification(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        let Some(external) = self.external() else {
            return Err(acp::Error::method_not_found());
        };
        external.ext_method(args).await
    }

    // The kernel owns the working tree and runs its own tools, so the client
    // filesystem and terminal surface stays closed.
    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(
        &self,
        _args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }
}

/// External-client-facing agent. Every request is forwarded to the kernel.
struct RelayAgent {
    kernel: Rc<acp::ClientSideConnection>,
    cwd: PathBuf,
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for RelayAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        tracing::info!(
            protocol_version = ?args.protocol_version,
            client = ?args.client_info.as_ref().map(|info| info.name.clone()),
            "openbuild-server: initialize"
        );
        let mut request = args;
        // The kernel executes tools itself; never grant it the external
        // editor's filesystem or terminal.
        request.client_capabilities.fs.read_text_file = false;
        request.client_capabilities.fs.write_text_file = false;
        request.client_capabilities.terminal = false;
        self.kernel.initialize(request).await
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        self.kernel.authenticate(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        let mut request = args;
        // An external client may omit a cwd; the server's own is the fallback
        // so the kernel always has one.
        if request.cwd.as_os_str().is_empty() {
            request.cwd = self.cwd.clone();
        }
        self.kernel.new_session(request).await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        self.kernel.load_session(args).await
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        self.kernel.prompt(args).await
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        self.kernel.cancel(args).await
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        self.kernel.set_session_mode(args).await
    }

    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> acp::Result<acp::SetSessionConfigOptionResponse> {
        self.kernel.set_session_config_option(args).await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> acp::Result<acp::ListSessionsResponse> {
        self.kernel.list_sessions(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        self.kernel.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        self.kernel.ext_notification(args).await
    }
}

/// Entry point used by `main`.
pub async fn run(kernel: Option<PathBuf>, cwd: Option<PathBuf>) -> Result<()> {
    if let Some(path) = kernel.as_ref()
        && !path.is_file()
    {
        bail!("kernel executable not found: {}", path.display());
    }
    serve(kernel, cwd).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_kernel_path_is_reported_before_spawning() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = rt
            .block_on(run(
                Some(PathBuf::from("definitely-not-a-real-kernel-xyz")),
                None,
            ))
            .err()
            .expect("a missing kernel must fail");
        assert!(error.to_string().contains("not found"));
    }
}
