//! HTTP server exposing the `/metrics` endpoint.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::encoding;
use crate::registry::Registry;

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
    config: Arc<Config>,
}

pub async fn serve(
    listen_address: &str,
    telemetry_path: &str,
    registry: Registry,
    config: Config,
) -> anyhow::Result<()> {
    let state = AppState {
        registry: Arc::new(registry),
        config: Arc::new(config),
    };
    let landing = build_landing(telemetry_path);
    let app = Router::new()
        .route(telemetry_path, get(metrics_handler))
        .route(
            "/",
            get(move || async move { axum::response::Html(landing) }),
        )
        .with_state(state);

    let listener = TcpListener::bind(listen_address).await?;
    let local = listener.local_addr()?;
    tracing::info!(address = %local, path = telemetry_path, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn metrics_handler(State(state): State<AppState>) -> Response {
    // Collectors do blocking /proc reads. They're fast (microseconds) but to
    // keep the async runtime responsive under load we still hop to the
    // blocking pool.
    let registry = state.registry.clone();
    let config = state.config.clone();
    let body = match tokio::task::spawn_blocking(move || {
        encoding::encode(&registry.gather(&config))
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "gather task panicked");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(encoding::CONTENT_TYPE),
    );
    resp
}

fn build_landing(telemetry_path: &str) -> String {
    format!(
        "<html>\n<head><title>Merlion Node Exporter</title></head>\n\
         <body>\n<h1>Merlion Node Exporter</h1>\n\
         <p><a href=\"{telemetry_path}\">Metrics</a></p>\n\
         </body>\n</html>\n"
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
