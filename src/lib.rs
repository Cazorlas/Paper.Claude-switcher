// Re-export modules needed by integration tests.
// The binary entry point remains in main.rs.

mod entry;
pub use entry::run_cli;

pub mod auth;
pub mod claude_api;
pub mod claude_store;
pub mod claude_usage;
mod cache;
mod cli;
mod color;
mod commands;
mod config;
mod error;
mod launch;
mod logging;
mod output;
mod profile;
mod signals;
mod tui;
mod update;
pub mod usage;
