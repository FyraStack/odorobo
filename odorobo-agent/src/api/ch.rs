use axum::{
    body::Body,
    extract::{Path, Request},
    response::Response,
};
use http_body_util::BodyExt;

use super::error::ApiError;
use crate::state::VMInstance;

pub async fn passthrough(
    Path((vmid, path)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let vm = VMInstance::get(&vmid).ok_or_else(|| ApiError::VmNotFound { vmid: vmid.clone() })?;

    let (mut parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|e| ApiError::PassthroughFailed { msg: e.to_string() })?
        .to_bytes();

    let path_and_query = match parts.uri.query() {
        Some(query) => format!("/{path}?{query}"),
        None => format!("/{path}"),
    };
    parts.uri = path_and_query
        .parse()
        .map_err(|e| ApiError::PassthroughFailed { msg: format!("URI parse error: {}", e) })?;

    let response = vm
        .call_request(hyper::Request::from_parts(parts, body))
        .await
        .map_err(|e| ApiError::PassthroughFailed { msg: e.to_string() })?;

    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(parts, Body::from(body)))
}
