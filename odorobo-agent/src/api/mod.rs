//! REST Management API for odorobo-agent
mod console;
mod error;
pub use error::ApiError;

mod vm;
pub fn router() -> axum::Router<()> {
    axum::Router::new()
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
