//! A Rune-side OpenCode client: the server connection and event stream (`client`), and the
//! sessions and transcripts they describe (`model`).

// A browser's requests speak ureq's API too (ureq-web), so the request code is the same there.
#[cfg(target_arch = "wasm32")]
extern crate ureq_web as ureq;

pub mod client;
pub use client::browser_rpc;
pub use client::mcp as client_mcp;
pub mod integration;
pub mod local_service;
pub mod mention;
pub mod model;
pub mod pairing;
pub mod path;
pub mod persistent_pty;
pub mod plain_error;
pub mod project;
pub mod prompt;
pub mod pty;
pub mod quote;
pub mod servers;
mod session_ops;
pub mod wire_error;
