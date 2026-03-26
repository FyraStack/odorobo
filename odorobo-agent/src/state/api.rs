use http_body_util::BodyExt;
use hyper::{Method, Request, header::CONTENT_TYPE};
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
    let normalized_path = normalize_api_path(path)?;
    let api_path = format!("/api/v1{normalized_path}");
    let uri: hyper::Uri = hyperlocal::Uri::new(socket_path, &api_path).into();
    let client = hyper_util::client::legacy::Client::unix();
    let request_body = body.map(Value::to_string).unwrap_or_default();

    let mut request = Request::builder().method(method).uri(uri);

    if body.is_some() {
        request = request.header(CONTENT_TYPE, "application/json");
    }

    let request = request
        .body(request_body)
        .wrap_err("Failed to build CH API request")?;

    let response = client
        .request(request)
        .await
        .wrap_err("Failed to send CH API request over unix socket")?;

    let status = response.status();
    let response_body = response
        .into_body()
        .collect()
        .await
        .wrap_err("Failed to read CH API response body")?
        .to_bytes();

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
