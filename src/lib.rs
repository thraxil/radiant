/// The release tag this binary was built as (e.g. `r12`, set by the release workflow), or the
/// crate version for local builds.
pub fn version() -> &'static str {
    match option_env!("RADIANT_RELEASE") {
        Some(r) if !r.is_empty() => r,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

pub mod catalog;
pub mod config;
pub mod format;
pub mod hash;
pub mod http;
pub mod imaging;
pub mod request;
pub mod service;
pub mod spec;
pub mod store;
pub mod telemetry;
