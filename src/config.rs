//! Runtime configuration. Each setting can come from a `RADIANT_*` environment variable or
//! the matching CLI flag.

use std::net::SocketAddr;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Parser)]
#[command(
    version,
    about = "Image scaling and serving, compatible with reticulum"
)]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "RADIANT_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// Log output format.
    #[arg(long, env = "RADIANT_LOG_FORMAT", value_enum, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Pretty,
    Json,
}
