//! lakitu — a live TUI cockpit plus the MCP server + coordination daemon for a
//! fleet of coordinating Claude Code agents.
//!
//! This crate produces a single `lakitu` binary with subcommands (see
//! `src/main.rs`): the default is the TUI cockpit; `lakitu mcp` is the stdio MCP
//! server, `lakitu serve` the HTTP daemon, and `lakitu install-hooks` the
//! installer. The modules below are shared by those entry points.

// Nested `if`s and the markdown-list doc comments are deliberate for
// readability, and `nonminimal_bool` lets `set_state`'s guard stay written the
// way the shell hook expresses it — we keep them rather than reflow on clippy's
// say-so. (Union of the allow-lists from both former binaries.)
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::doc_lazy_continuation,
    clippy::nonminimal_bool
)]

pub mod app;
pub mod client;
pub mod daemon;
pub mod event;
pub mod fleet;
pub mod gh;
pub mod install;
pub mod log;
pub mod persona;
pub mod remote;
pub mod rest;
pub mod server;
pub mod store;
pub mod ui;
pub mod web;
pub mod wire;
pub mod work;
