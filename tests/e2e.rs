/// End-to-end tests for the git read-only proxy.
///
/// These tests require:
///   * `git`      — to create repos and run clone/fetch/push
///   * `python3`  — to run the lightweight git-http-backend CGI wrapper
///
/// A real bare git repository is created in a temporary directory, served by
/// the Python helper, and then accessed through the Rust proxy to verify that
/// read operations succeed and write operations are rejected with HTTP 403.
///
/// The `#[cfg(all(target_arch = "x86_64", target_os = "linux"))]` restriction
/// matches the problem-statement requirement: "Only x86_64 Linux is needed to
/// be tested."  The proxy itself compiles and runs on other platforms; only
/// the e2e tests are constrained.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod e2e {
    use std::{
        io::{BufRead, BufReader},
        net::SocketAddr,
        path::Path,
        process::{Child, Command, Stdio},
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio::time::sleep;

    // ── Git HTTP backend server ───────────────────────────────────────────────

    /// A live Python-backed git HTTP server running in a child process.
    struct GitServer {
        port: u16,
        process: Child,
        /// Kept alive so the temporary directory is not deleted while the
        /// server is running.
        _repo_dir: TempDir,
    }

    impl GitServer {
        /// Initialise a bare repository with one commit, then start the server.
        fn start() -> Self {
            let repo_dir = TempDir::new().expect("create temp dir");
            let bare = repo_dir.path().join("test.git");

            // ── Create a bare repository ──────────────────────────────────
            run_git_cmd(&["init", "--bare", bare.to_str().unwrap()], None);
            // Point HEAD at `main` so clones can check out the branch.
            run_git_cmd(
                &[
                    "-C",
                    bare.to_str().unwrap(),
                    "symbolic-ref",
                    "HEAD",
                    "refs/heads/main",
                ],
                None,
            );
            // Allow HTTP receive-pack so the git server itself won't refuse
            // pushes – the *proxy* is the one that must block them.
            run_git_cmd(
                &[
                    "-C",
                    bare.to_str().unwrap(),
                    "config",
                    "http.receivepack",
                    "true",
                ],
                None,
            );

            // ── Populate it with one commit (local push, not HTTP) ────────
            let src = TempDir::new().expect("create src dir");
            run_git_cmd(&["init", src.path().to_str().unwrap()], None);
            run_git_cmd(
                &[
                    "-C",
                    src.path().to_str().unwrap(),
                    "config",
                    "user.email",
                    "test@test.com",
                ],
                None,
            );
            run_git_cmd(
                &[
                    "-C",
                    src.path().to_str().unwrap(),
                    "config",
                    "user.name",
                    "Test",
                ],
                None,
            );
            std::fs::write(src.path().join("README.md"), "# test repo\n").expect("write README");
            run_git_cmd(&["-C", src.path().to_str().unwrap(), "add", "."], None);
            run_git_cmd(
                &[
                    "-C",
                    src.path().to_str().unwrap(),
                    "commit",
                    "-m",
                    "initial commit",
                ],
                None,
            );
            run_git_cmd(
                &[
                    "-C",
                    src.path().to_str().unwrap(),
                    "push",
                    bare.to_str().unwrap(),
                    "HEAD:main",
                ],
                None,
            );

            // ── Launch the Python git HTTP server ─────────────────────────
            let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/git_http_server.py");
            let mut process = Command::new("python3")
                .args([script, "0", repo_dir.path().to_str().unwrap()])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("start python git server");

            // Read the "LISTENING <port>" line printed by the server.
            let stdout = process.stdout.take().expect("stdout");
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read port line");
            let port: u16 = line
                .trim()
                .strip_prefix("LISTENING ")
                .expect("expected 'LISTENING <port>'")
                .parse()
                .expect("port is a number");

            GitServer {
                port,
                process,
                _repo_dir: repo_dir,
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for GitServer {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    // ── Proxy server ──────────────────────────────────────────────────────────

    struct ProxyServer {
        port: u16,
        _task: tokio::task::JoinHandle<()>,
    }

    impl ProxyServer {
        async fn start(upstream_url: &str) -> Self {
            let upstream = upstream_url.parse().expect("valid upstream URL");
            let app = git_read_only_proxy::create_router(upstream);

            // Bind on port 0 to get a random available port from the OS.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind proxy listener");
            let port = listener.local_addr().expect("local addr").port();

            let task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("proxy server error");
            });

            // Give the proxy task a moment to accept its first connection.
            wait_for_port(port).await;

            ProxyServer { port, _task: task }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for ProxyServer {
        fn drop(&mut self) {
            self._task.abort();
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Run a git command; panic on failure.
    fn run_git_cmd(args: &[&str], cwd: Option<&Path>) {
        let mut cmd = Command::new("git");
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // Suppress git's interactive credential prompts.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        let status = cmd.status().expect("run git");
        assert!(
            status.success(),
            "git {} failed with status {status}",
            args.join(" ")
        );
    }

    /// Attempt a git command; return whether it succeeded.
    fn try_git_cmd(args: &[&str], cwd: Option<&Path>) -> bool {
        let mut cmd = Command::new("git");
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    /// Poll until a TCP listener appears on `port` (max ~5 s).
    async fn wait_for_port(port: u16) {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
        panic!("Port {port} did not become reachable within 5 s");
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// `git clone` through the proxy must succeed.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_git_clone_through_proxy() {
        let git_server = GitServer::start();
        let proxy = ProxyServer::start(&git_server.url()).await;

        let clone_dir = TempDir::new().unwrap();
        let repo_url = format!("{}/test.git", proxy.url());
        run_git_cmd(
            &["clone", &repo_url, clone_dir.path().to_str().unwrap()],
            None,
        );

        // Verify the cloned content is present.
        assert!(
            clone_dir.path().join("README.md").exists(),
            "README.md should be present after clone"
        );
    }

    /// `git fetch` through the proxy must succeed after an initial clone.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_git_fetch_through_proxy() {
        let git_server = GitServer::start();
        let proxy = ProxyServer::start(&git_server.url()).await;

        let clone_dir = TempDir::new().unwrap();
        let repo_url = format!("{}/test.git", proxy.url());

        run_git_cmd(
            &["clone", &repo_url, clone_dir.path().to_str().unwrap()],
            None,
        );
        // A second fetch should also succeed.
        run_git_cmd(&["fetch", "--all"], Some(clone_dir.path()));
    }

    /// `git pull` through the proxy must succeed.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_git_pull_through_proxy() {
        let git_server = GitServer::start();
        let proxy = ProxyServer::start(&git_server.url()).await;

        let clone_dir = TempDir::new().unwrap();
        let repo_url = format!("{}/test.git", proxy.url());

        run_git_cmd(
            &["clone", &repo_url, clone_dir.path().to_str().unwrap()],
            None,
        );
        // `git pull` re-uses fetch + merge; it must succeed when there is
        // nothing new to merge.
        run_git_cmd(&["pull"], Some(clone_dir.path()));
    }

    /// `git push` through the proxy must be rejected (HTTP 403).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_git_push_is_blocked_by_proxy() {
        let git_server = GitServer::start();
        let proxy = ProxyServer::start(&git_server.url()).await;

        // Clone the repo first (directly, not via proxy) so we have a local
        // copy with push remote set to the proxy.
        let clone_dir = TempDir::new().unwrap();
        let direct_url = format!("{}/test.git", git_server.url());
        run_git_cmd(
            &["clone", &direct_url, clone_dir.path().to_str().unwrap()],
            None,
        );

        // Point the remote to the proxy.
        let proxy_url = format!("{}/test.git", proxy.url());
        run_git_cmd(
            &["remote", "set-url", "origin", &proxy_url],
            Some(clone_dir.path()),
        );

        // Commit something new so there is content to push.
        run_git_cmd(
            &[
                "-C",
                clone_dir.path().to_str().unwrap(),
                "config",
                "user.email",
                "test@test.com",
            ],
            None,
        );
        run_git_cmd(
            &[
                "-C",
                clone_dir.path().to_str().unwrap(),
                "config",
                "user.name",
                "Test",
            ],
            None,
        );
        std::fs::write(clone_dir.path().join("CHANGES.md"), "change\n").unwrap();
        run_git_cmd(&["add", "."], Some(clone_dir.path()));
        run_git_cmd(&["commit", "-m", "add change"], Some(clone_dir.path()));

        // The push must fail because the proxy blocks git-receive-pack.
        let ok = try_git_cmd(&["push", "origin", "HEAD:main"], Some(clone_dir.path()));
        assert!(
            !ok,
            "git push should have been rejected by the read-only proxy"
        );
    }

    /// Confirm that a direct `git push` to the backend (bypassing the proxy)
    /// still works, proving the server itself is not the cause of the failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_direct_push_to_backend_succeeds() {
        let git_server = GitServer::start();

        let clone_dir = TempDir::new().unwrap();
        let direct_url = format!("{}/test.git", git_server.url());
        run_git_cmd(
            &["clone", &direct_url, clone_dir.path().to_str().unwrap()],
            None,
        );
        run_git_cmd(
            &[
                "-C",
                clone_dir.path().to_str().unwrap(),
                "config",
                "user.email",
                "test@test.com",
            ],
            None,
        );
        run_git_cmd(
            &[
                "-C",
                clone_dir.path().to_str().unwrap(),
                "config",
                "user.name",
                "Test",
            ],
            None,
        );
        std::fs::write(clone_dir.path().join("EXTRA.md"), "extra\n").unwrap();
        run_git_cmd(&["add", "."], Some(clone_dir.path()));
        run_git_cmd(&["commit", "-m", "add extra"], Some(clone_dir.path()));

        // Direct push to the backend must succeed.
        run_git_cmd(&["push", "origin", "HEAD:main"], Some(clone_dir.path()));
    }
}
