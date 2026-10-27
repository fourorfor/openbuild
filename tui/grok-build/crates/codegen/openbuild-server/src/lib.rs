//! ACP server that fronts an OpenBuild kernel.
//!
//! The TUI is an ACP *client* of the kernel. This crate adds the opposite
//! edge: it speaks ACP as an *agent* to an external client (Zed, Neovim, any
//! ACP client), and forwards each request to a kernel it owns over the same
//! subprocess bridge the TUI uses.
//!
//! ```text
//! external ACP client  ──stdio──▶  openbuild-server  ──stdio──▶  openbuild-kernel.exe
//! ```
//!
//! ## What is forwarded
//!
//! Everything the kernel advertises: `initialize`, `session/new`,
//! `session/load`, `session/list`, `session/fork`, `session/resume`,
//! `session/close`, `session/set_mode`, `session/set_config_option`,
//! `session/prompt`, `session/cancel`. Kernel `session/update` notifications
//! flow back verbatim, so the external client sees the same tool calls,
//! thoughts, and plans the TUI does.
//!
//! ## Permission requests
//!
//! The kernel asks *its client* (this server) for permission. Those requests
//! are re-issued to the external client, so a human in Zed answers them exactly
//! as they would in the TUI. A denial the client never answers is answered as
//! "cancelled" rather than left hanging.
//!
//! ## Deliberate limits
//!
//! - One kernel per server process, one external client per kernel.
//! - The `_meta` stamps the TUI relies on (`promptId`, `eventSeq`,
//!   `isReplay`) are passed through untouched; this server never rewrites
//!   update shapes. Renderer adaptation stays in `openbuild-backend`'s
//!   `adapter` module, which the TUI owns.

pub mod relay;
