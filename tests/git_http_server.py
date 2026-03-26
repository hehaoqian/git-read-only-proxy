#!/usr/bin/env python3
"""
Minimal git HTTP server for integration tests.

Wraps ``git http-backend`` (the standard CGI helper shipped with git) in a
plain Python HTTP server so the Rust e2e tests have a real git remote to talk
to without requiring Apache / nginx / lighttpd.

Usage
-----
    python3 git_http_server.py <port> <git-project-root>

The server prints one line to stdout once it is ready::

    LISTENING <actual-port>

It then serves repositories found under *git-project-root*.  For example, if
the root is ``/tmp/repos`` and you push to ``http://…/hello.git``, the server
expects a bare repository at ``/tmp/repos/hello.git``.
"""

import os
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class GitHTTPRequestHandler(BaseHTTPRequestHandler):
    """Dispatch every request to ``git http-backend`` via CGI-style env vars."""

    git_root: str = ""

    # ------------------------------------------------------------------ #
    # HTTP methods                                                          #
    # ------------------------------------------------------------------ #

    def do_GET(self) -> None:
        self._dispatch()

    def do_POST(self) -> None:
        self._dispatch()

    # ------------------------------------------------------------------ #
    # Core dispatch                                                         #
    # ------------------------------------------------------------------ #

    def _dispatch(self) -> None:
        content_length = int(self.headers.get("Content-Length", 0))
        request_body = self.rfile.read(content_length) if content_length > 0 else b""

        path, _, query = self.path.partition("?")

        env = os.environ.copy()
        env["GIT_PROJECT_ROOT"] = self.git_root
        env["GIT_HTTP_EXPORT_ALL"] = "1"
        env["REQUEST_METHOD"] = self.command
        env["PATH_INFO"] = path
        env["QUERY_STRING"] = query
        env["CONTENT_TYPE"] = self.headers.get("Content-Type", "")
        env["CONTENT_LENGTH"] = str(content_length)
        env["SERVER_PROTOCOL"] = "HTTP/1.1"
        env["SERVER_SOFTWARE"] = "git-http-test-server/1.0"

        auth = self.headers.get("Authorization", "")
        if auth:
            env["HTTP_AUTHORIZATION"] = auth

        try:
            result = subprocess.run(
                ["git", "http-backend"],
                input=request_body,
                capture_output=True,
                env=env,
                timeout=60,
            )
        except FileNotFoundError:
            self._send_plain(500, b"git not found in PATH\n")
            return
        except subprocess.TimeoutExpired:
            self._send_plain(504, b"git http-backend timed out\n")
            return

        if result.returncode != 0:
            self._send_plain(
                500,
                b"git http-backend exited with code "
                + str(result.returncode).encode()
                + b"\n"
                + result.stderr,
            )
            return

        self._forward_cgi_response(result.stdout)

    # ------------------------------------------------------------------ #
    # CGI response parsing                                                  #
    # ------------------------------------------------------------------ #

    def _forward_cgi_response(self, output: bytes) -> None:
        # CGI output: headers, blank line, body.
        for sep in (b"\r\n\r\n", b"\n\n"):
            idx = output.find(sep)
            if idx != -1:
                header_block = output[:idx].decode("utf-8", errors="replace")
                body = output[idx + len(sep) :]
                break
        else:
            self._send_plain(500, b"Malformed git http-backend output\n")
            return

        status = 200
        headers: list[tuple[str, str]] = []

        for line in header_block.splitlines():
            line = line.strip()
            if not line:
                continue
            if line.lower().startswith("status:"):
                try:
                    status = int(line.split(":", 1)[1].strip().split()[0])
                except (ValueError, IndexError):
                    pass
            elif ":" in line:
                name, _, value = line.partition(":")
                headers.append((name.strip(), value.strip()))

        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    # ------------------------------------------------------------------ #
    # Helpers                                                               #
    # ------------------------------------------------------------------ #

    def _send_plain(self, status: int, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt: str, *args: object) -> None:
        """Silence the default per-request access log output."""
        pass


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    git_root = sys.argv[2] if len(sys.argv) > 2 else os.getcwd()

    GitHTTPRequestHandler.git_root = git_root
    server = HTTPServer(("127.0.0.1", port), GitHTTPRequestHandler)
    actual_port = server.server_address[1]
    # Signal readiness to the parent process.
    print(f"LISTENING {actual_port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
