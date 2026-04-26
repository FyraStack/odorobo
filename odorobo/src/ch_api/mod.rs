//! REST Management API for odorobo-agent
mod ch;
mod console;
mod error;
mod vm;

// pub fn router(port: u16) -> axum::Router<()> {
//     let info_route = axum::Router::new()
//         .route("/info", axum::routing::get(info))
//         .with_state(port);

//     axum::Router::new()
//         .layer(
//             TraceLayer::new_for_http()
//                 .on_request(DefaultOnRequest::new())
//                 .on_response(DefaultOnResponse::new())
//         )
//         .route("/", axum::routing::get(root))
//         .route("/health", axum::routing::get(health))
//         .merge(info_route)
//         .nest("/vms", vm::router())
// }

// async fn root() -> &'static str {
//     env!("CARGO_PKG_VERSION")
// }

// async fn health() -> &'static str {
//     ""
// }

// #[derive(Serialize)]
// struct AgentInfo {
//     version: &'static str,
//     listening_addresses: Vec<String>,
// }

// async fn info(State(port): State<u16>) -> Json<AgentInfo> {
//     let listening_addresses = if_addrs::get_if_addrs()
//         .unwrap_or_default()
//         .into_iter()
//         .filter(|i| !i.is_loopback())
//         .map(|i| format!("{}:{}", i.ip(), port))
//         .collect();
//     Json(AgentInfo {
//         version: env!("CARGO_PKG_VERSION"),
//         listening_addresses,
//     })
// }
