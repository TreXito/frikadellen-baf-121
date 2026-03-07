pub mod routes;
pub mod state;

pub use state::{WebEventLog, WebState};

use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::info;

/// Start the Axum web server on the configured port.
///
/// This function runs forever (or until the runtime shuts down).
pub async fn start_web_server(state: Arc<WebState>, port: u16) {
    let app = Router::new()
        .route("/", get(routes::index))
        .route("/api/status", get(routes::status))
        .route("/api/inventory", get(routes::inventory))
        .route("/api/events", get(routes::events))
        .route("/api/command", post(routes::send_command))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind web GUI port");

    info!("Web GUI running at http://localhost:{}", port);
    axum::serve(listener, app).await.unwrap();
}
