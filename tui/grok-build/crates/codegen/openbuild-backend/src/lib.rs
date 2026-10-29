//! Backend adapter for OpenBuild.
//!
//! The grok TUI is a pure ACP client. This crate supplies the missing pieces
//! for talking to an out-of-process ACP agent:
//!
//! - [`profile`]: backend capabilities derived from `initialize`
//! - [`spawn`]: kernel discovery and process launch
//! - [`transport`]: NDJSON subprocess bridge into the existing typed ACP channel
//! - [`health`]: kernel liveness, crash reason and reconnect requests
//! - [`adapter`]: wire-shape normalization for grok's renderer
//! - [`config`]: session `configOptions` ⇄ model / effort / mode selections
//! - [`data_migration`]: one-time copy of the kernel's old `opencode` data directories
//! - [`http_feed`]: optional read-only HTTP/SSE side channel into the kernel

pub mod adapter;
pub mod config;
pub mod data_migration;
pub mod health;
pub mod http_feed;
pub mod plugin;
pub mod profile;
pub mod providers;
pub mod proxy;
pub mod spawn;
pub mod transport;

pub use config::{ConfigChange, ConfigOptions, SelectOption, SelectValue};
pub use health::{BridgeExit, KernelHealth};
pub use profile::{BackendKind, BackendProfile, Capabilities};
