//! REST Management API for odorobo-agent
mod ch;
mod console;
mod error;
use tower_http::trace::TraceLayer;
mod vm;
pub fn router() -> axum::Router<()> {
    axum::Router::new()
        .layer(TraceLayer::new_for_http())
        .route("/", axum::routing::get(root))
        .route("/health", axum::routing::get(health))
        .nest("/vms", vm::router())
}

async fn root() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

async fn health() -> &'static str {
    ""
}
