use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    Router,
};
use std::sync::Arc;
use url::Url;

/// Shared state cloned into every request handler.
#[derive(Clone)]
pub struct ProxyState {
    /// Base URL of the upstream git server (no trailing slash).
    pub upstream: Url,
    /// Reusable HTTP client for upstream connections.
    pub client: reqwest::Client,
}

/// Build an [`axum::Router`] that proxies all requests to `upstream`.
///
/// The router uses a catch-all fallback so that every path and method is
/// forwarded (subject to the read-only filter).
pub fn create_router(upstream: Url) -> Router {
    let client = reqwest::Client::builder()
        // Do not follow redirects automatically; let the git client decide.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Failed to build HTTP client");

    let state = Arc::new(ProxyState { upstream, client });

    Router::new().fallback(proxy_handler).with_state(state)
}

/// Axum handler: validate, then forward each request to the upstream server.
async fn proxy_handler(State(state): State<Arc<ProxyState>>, req: Request<Body>) -> Response<Body> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    // ── Read-only enforcement ────────────────────────────────────────────────
    if crate::filter::is_write_operation(&uri) {
        tracing::info!(
            method = %method,
            path = %uri.path(),
            "Blocked write operation"
        );
        return plain_response(
            StatusCode::FORBIDDEN,
            "Push operations are not supported by this read-only proxy\n",
        );
    }

    // ── Build upstream URL ───────────────────────────────────────────────────
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let upstream_base = state.upstream.as_str().trim_end_matches('/');
    let upstream_url = format!("{upstream_base}{path_and_query}");

    // ── Convert method ───────────────────────────────────────────────────────
    let reqwest_method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(e) => {
            return plain_response(
                StatusCode::BAD_REQUEST,
                format!("Invalid HTTP method: {e}\n"),
            );
        }
    };

    let mut req_builder = state.client.request(reqwest_method, &upstream_url);

    // ── Forward request headers ──────────────────────────────────────────────
    for (name, value) in &headers {
        let n = name.as_str().to_ascii_lowercase();
        // Skip hop-by-hop headers and the original Host (we set our own below).
        if n == "host" || is_hop_by_hop(&n) {
            continue;
        }
        req_builder = req_builder.header(name.as_str(), value.as_bytes());
    }

    // Set Host to match the upstream.
    if let Some(host) = state.upstream.host_str() {
        let host_header = match state.upstream.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        req_builder = req_builder.header("host", host_header);
    }

    // ── Stream / buffer request body ─────────────────────────────────────────
    // Git fetch request bodies (the "want" list) are small; cap at 50 MiB.
    const MAX_REQUEST_BODY: usize = 50 * 1024 * 1024;
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("Failed to read request body: {e}");
            return plain_response(
                StatusCode::BAD_REQUEST,
                format!("Failed to read request body: {e}\n"),
            );
        }
    };
    if !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes);
    }

    // ── Send to upstream ─────────────────────────────────────────────────────
    tracing::debug!(url = %upstream_url, "Forwarding request to upstream");

    let upstream_resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Upstream request failed: {e}");
            return plain_response(
                StatusCode::BAD_GATEWAY,
                format!("Upstream request failed: {e}\n"),
            );
        }
    };

    // ── Build response ───────────────────────────────────────────────────────
    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut builder = Response::builder().status(status);

    for (name, value) in upstream_resp.headers() {
        let n = name.as_str().to_ascii_lowercase();
        if !is_hop_by_hop(&n) {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
    }

    // Stream the (potentially large) response body back to the client.
    let body = Body::from_stream(upstream_resp.bytes_stream());

    builder.body(body).unwrap_or_else(|_| {
        plain_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error\n")
    })
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn plain_response(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(body.into()))
        .expect("static response is always valid")
}

/// Returns `true` for HTTP/1.1 hop-by-hop headers that must not be forwarded.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_detected() {
        assert!(is_hop_by_hop("connection"));
        assert!(is_hop_by_hop("transfer-encoding"));
        assert!(is_hop_by_hop("keep-alive"));
        assert!(!is_hop_by_hop("content-type"));
        assert!(!is_hop_by_hop("authorization"));
        assert!(!is_hop_by_hop("content-length"));
    }
}
