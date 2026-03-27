use http_body_util::BodyExt;
use hyper::{
    Method, Request, Response,
    body::Bytes,
    header::{CONTENT_LENGTH, CONTENT_TYPE, HOST},
};
use hyperlocal::UnixClientExt;
use serde_json::Value;
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use std::path::Path;

pub async fn call(
    socket_path: &Path,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<()> {
    let request_body = body
        .map(|value| Bytes::from(value.to_string()))
        .unwrap_or_default();
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(request_body)
        .wrap_err("Failed to build CH API request")?;

    if body.is_some() {
        request
            .headers_mut()
            .insert(CONTENT_TYPE, "application/json".parse().unwrap());
    }

    let response = call_request(socket_path, request).await?;
    let status = response.status();
    let response_body = response.into_body();

    if !status.is_success() {
        let response_body = String::from_utf8_lossy(&response_body);
        return Err(eyre!(
            "CH API returned {} for {}: {}",
            status,
            path,
            response_body
        ));
    }

    Ok(())
}

pub async fn call_request(socket_path: &Path, request: Request<Bytes>) -> Result<Response<Bytes>> {
    let (parts, body) = request.into_parts();
    let api_path = build_api_path(parts.uri.path(), parts.uri.query())?;
    let uri: hyper::Uri = hyperlocal::Uri::new(socket_path, &api_path).into();
    let client = hyper_util::client::legacy::Client::unix();

    let mut request = Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(http_body_util::Full::new(body))
        .wrap_err("Failed to build CH API request")?;

    for (name, value) in &parts.headers {
        if name == HOST || name == CONTENT_LENGTH {
            continue;
        }

        request.headers_mut().append(name, value.clone());
    }

    let response = client
        .request(request)
        .await
        .wrap_err("Failed to send CH API request over unix socket")?;

    let (parts, body) = response.into_parts();
    let response_body = body
        .collect()
        .await
        .wrap_err("Failed to read CH API response body")?
        .to_bytes();

    let mut response = Response::builder()
        .status(parts.status)
        .body(response_body)
        .wrap_err("Failed to build CH API response")?;
    *response.headers_mut() = parts.headers;

    Ok(response)
}

fn build_api_path(path: &str, query: Option<&str>) -> Result<String> {
    let normalized_path = normalize_api_path(path)?;

    match query.filter(|query| !query.is_empty()) {
        Some(query) => Ok(format!("/api/v1{normalized_path}?{query}")),
        None => Ok(format!("/api/v1{normalized_path}")),
    }
}

fn normalize_api_path(path: &str) -> Result<String> {
    let path = path.trim();

    if path.is_empty() {
        return Err(eyre!("CH API path cannot be empty"));
    }

    let path = path
        .strip_prefix("/api/v1")
        .or_else(|| path.strip_prefix("api/v1"))
        .unwrap_or(path)
        .trim_start_matches('/');

    if path.is_empty() {
        return Err(eyre!("CH API path cannot point to the API root only"));
    }

    Ok(format!("/{path}"))
}
