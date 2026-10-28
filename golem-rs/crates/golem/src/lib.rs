//! The `golem` binary's modules, as a library.
//!
//! `main.rs` is the front door and nothing else: it parses the command line
//! and calls into these. Keeping the modules here lets the integration tests
//! in `tests/` import them as `golem::<module>` instead of compiling sources
//! by path. The layers - front ends, commands, engine, foundation - are in
//! `docs/architecture.md`.

pub mod api;
#[cfg(feature = "desktop")]
pub mod app;
pub mod auto_tune;
pub mod bench;
pub mod client;
pub mod config;
pub mod desktop;
pub mod engine;
pub mod eval;
pub mod facts;
pub mod install;
pub mod launch;
pub mod logs;
pub mod machine;
pub mod measure;
pub mod models;
pub mod paths;
pub mod proc;
pub mod progress;
pub mod pull;
pub mod release;
pub mod runner;
pub mod service;
pub mod setup;
pub mod shortcut;
pub mod state;
pub mod tune;
pub mod update;
pub mod tui;
pub mod web;
