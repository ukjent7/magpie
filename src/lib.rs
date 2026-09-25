#![forbid(unsafe_code)]

pub const VERSION: &str = match option_env!("MAGPIE_VERSION") {
    Some(version) => version,
    None => "dev",
};

mod affinity;
pub mod agent;
pub mod backup;
pub mod catalog;
pub mod cli;
mod codex;
pub mod config;
mod copilot;
pub mod gateway;
pub mod groups;
mod netproxy;
pub mod profile;
pub mod provider;
pub mod settings;
mod translation;
pub mod tui;
pub mod update;
pub mod usage;
