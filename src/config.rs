//! Runtime configuration. Each setting can come from a `RADIANT_*` environment variable or
//! the matching CLI flag.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use crate::imaging::{EncodeSettings, Limits};
use crate::service::ServiceConfig;

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

    /// SQLite catalog path. It's a cache that can be rebuilt, so it needs no backups.
    #[arg(long, env = "RADIANT_DB_PATH", default_value = "data/radiant.db")]
    pub db_path: PathBuf,

    /// Storage backend.
    #[arg(long, env = "RADIANT_BACKEND", value_enum, default_value_t = Backend::Fs)]
    pub backend: Backend,

    /// Root directory for the `fs` backend.
    #[arg(long, env = "RADIANT_FS_ROOT", default_value = "data/blobs")]
    pub fs_root: PathBuf,

    /// If set, uploads must include it as the `key` form field.
    #[arg(long, env = "RADIANT_UPLOAD_KEY", hide_env_values = true)]
    pub upload_key: Option<String>,

    /// Largest accepted upload, in bytes.
    #[arg(long, env = "RADIANT_MAX_UPLOAD_BYTES", default_value_t = 50 * 1024 * 1024)]
    pub max_upload_bytes: usize,

    /// Largest accepted source image, in pixels (width × height).
    #[arg(long, env = "RADIANT_MAX_INPUT_PIXELS", default_value_t = 100_000_000)]
    pub max_input_pixels: u64,

    /// Largest width or height a scaled image may have.
    #[arg(long, env = "RADIANT_MAX_OUTPUT_DIMENSION", default_value_t = 4096)]
    pub max_output_dimension: u32,

    /// Maximum concurrent scaling jobs. Defaults to the number of CPUs.
    #[arg(long, env = "RADIANT_SCALE_CONCURRENCY")]
    pub scale_concurrency: Option<usize>,

    /// Seconds a request waits for scaling before returning 503. Scaling carries on and the
    /// result is stored.
    #[arg(long, env = "RADIANT_SCALE_TIMEOUT_SECS", default_value_t = 30)]
    pub scale_timeout_secs: u64,

    /// JPEG quality, 1–100.
    #[arg(long, env = "RADIANT_JPEG_QUALITY", default_value_t = 90,
          value_parser = clap::value_parser!(u8).range(1..=100))]
    pub jpeg_quality: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// Local filesystem.
    Fs,
}

impl Config {
    pub fn service_config(&self) -> ServiceConfig {
        let defaults = ServiceConfig::default();
        ServiceConfig {
            limits: Limits {
                max_input_pixels: self.max_input_pixels,
                max_output_dimension: self.max_output_dimension,
            },
            encode: EncodeSettings {
                jpeg_quality: self.jpeg_quality,
                ..EncodeSettings::default()
            },
            scale_concurrency: self.scale_concurrency.unwrap_or(defaults.scale_concurrency),
            scale_timeout: Duration::from_secs(self.scale_timeout_secs),
        }
    }
}
