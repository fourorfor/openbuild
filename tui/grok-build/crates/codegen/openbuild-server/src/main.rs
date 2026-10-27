//! `openbuild-server` — expose an OpenBuild kernel to an external ACP client.

use std::path::PathBuf;

use clap::Parser;

/// Serve an OpenBuild kernel over ACP on stdio.
///
/// Point an ACP client (Zed, Neovim, …) at this binary the way you would point
/// it at any other ACP agent. The server starts the kernel itself and relays
/// the whole conversation.
#[derive(Debug, Parser)]
#[command(name = "openbuild-server", version, about)]
struct Args {
    /// Path to `openbuild-kernel.exe`. Defaults to `OPENBUILD_KERNEL`, then a
    /// sibling of this executable.
    #[arg(long, value_name = "PATH")]
    kernel: Option<PathBuf>,

    /// Working directory handed to the kernel. Defaults to the current one.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // The ACP connections are `!Send` (Rc + spawn_local), so they need a
    // LocalSet; the runtime alone is not enough.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move { openbuild_server::relay::run(args.kernel, args.cwd).await })
        .await
}
