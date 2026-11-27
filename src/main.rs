use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use radiant::catalog::Catalog;
use radiant::config::{Backend, Config, LogFormat};
use radiant::http::AppState;
use radiant::service::ImageService;
use radiant::store::{BlobStore, ObjectBlobStore};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    init_tracing(config.log_format);

    let store: Arc<dyn BlobStore> = match config.backend {
        Backend::Fs => Arc::new(
            ObjectBlobStore::local(&config.fs_root)
                .with_context(|| format!("opening fs backend at {}", config.fs_root.display()))?,
        ),
    };
    let catalog = Catalog::open(&config.db_path)
        .await
        .with_context(|| format!("opening catalog at {}", config.db_path.display()))?;
    let service_config = config.service_config();
    tracing::info!(
        backend = store.backend(),
        db = %config.db_path.display(),
        upload_key_required = config.upload_key.is_some(),
        scale_concurrency = service_config.scale_concurrency,
        "starting"
    );
    let state = AppState {
        service: ImageService::new(store, catalog, service_config),
        upload_key: config.upload_key.as_deref().map(Arc::from),
        max_upload_bytes: config.max_upload_bytes,
    };

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(addr = %config.listen, "listening");

    axum::serve(listener, radiant::http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("stopped");
    Ok(())
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=debug"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
