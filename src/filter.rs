use axum::http::Uri;

/// Returns `true` when the request targets a git *write* operation
/// (`git-receive-pack`) and should therefore be blocked by the proxy.
///
/// Write-operation detection is based solely on the request URI because the
/// git smart-HTTP protocol identifies push operations by path and query string,
/// not by HTTP method:
///
/// | Request | Purpose |
/// |---------|---------|
/// | `GET  /repo.git/info/refs?service=git-receive-pack` | push advertisement |
/// | `POST /repo.git/git-receive-pack`                   | push data          |
///
/// All other requests (upload-pack / fetch, dumb-HTTP static files, …) are
/// considered read-only and are allowed through.
pub fn is_write_operation(uri: &Uri) -> bool {
    let path = uri.path();
    let query = uri.query().unwrap_or("");

    // Block any request to the receive-pack endpoint.
    if path.ends_with("/git-receive-pack") {
        return true;
    }

    // Block the push discovery request (?service=git-receive-pack).
    if query
        .split('&')
        .any(|param| param == "service=git-receive-pack")
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn uri(s: &str) -> Uri {
        s.parse().expect("valid URI")
    }

    #[test]
    fn fetch_info_refs_is_allowed() {
        assert!(!is_write_operation(&uri(
            "/repo.git/info/refs?service=git-upload-pack"
        )));
    }

    #[test]
    fn upload_pack_post_is_allowed() {
        assert!(!is_write_operation(&uri("/repo.git/git-upload-pack")));
    }

    #[test]
    fn static_file_get_is_allowed() {
        assert!(!is_write_operation(&uri("/repo.git/HEAD")));
    }

    #[test]
    fn receive_pack_post_is_blocked() {
        assert!(is_write_operation(&uri("/repo.git/git-receive-pack")));
    }

    #[test]
    fn receive_pack_info_refs_is_blocked() {
        assert!(is_write_operation(&uri(
            "/repo.git/info/refs?service=git-receive-pack"
        )));
    }

    #[test]
    fn receive_pack_with_multiple_params_is_blocked() {
        assert!(is_write_operation(&uri(
            "/repo.git/info/refs?foo=bar&service=git-receive-pack"
        )));
    }

    #[test]
    fn upload_pack_service_in_path_is_allowed() {
        // Path contains "git-receive-pack" only as a repo name prefix, not as
        // the final path segment → must NOT be blocked.
        assert!(!is_write_operation(&uri(
            "/git-receive-pack-mirror.git/info/refs?service=git-upload-pack"
        )));
    }

    #[test]
    fn root_path_is_allowed() {
        assert!(!is_write_operation(&uri("/")));
    }
}
