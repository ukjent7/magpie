#![forbid(unsafe_code)]

pub const VERSION: &str = match option_env!("MAGPIE_VERSION") {
    Some(version) => version,
    None => "dev",
};

mod accounts;
mod affinity;
pub mod agent;
pub mod backup;
pub mod catalog;
mod claudebridge;
pub mod cli;
mod codex;
mod codexcat;
pub mod config;
mod copilot;
pub mod desktop;
mod dsh;
pub mod gateway;
mod gemini;
mod grouprule;
pub mod groups;
pub mod library;
mod netproxy;
pub mod profile;
pub mod provider;
mod quota;
mod redact;
pub mod settings;
mod translation;
pub mod tui;
pub mod update;
pub mod usage;
