#!/usr/bin/env python3
"""Opt-in Claude binary regression: synthetic local API, no account or transcript.

Run with --claude /path/to/claude. This exercises the same headless control
interrupt used by VS Code, without driving or altering an existing IDE session.
Only runtime metadata is printed. No real API request or model billing occurs.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--claude", default=shutil.which("claude"))
    args = parser.parse_args()
    if not args.claude:
        parser.error("provide --claude /path/to/claude")
    binary = str(Path(args.claude).resolve())
    request_started = threading.Event()
    finished = threading.Event()

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if "count_tokens" in self.path:
                data = b'{"input_tokens":10}'
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            request_started.set()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            message = {
                "type": "message_start",
                "message": {
                    "id": "msg_test", "type": "message", "role": "assistant",
                    "model": "claude-sonnet-4-6", "content": [], "stop_reason": None,
                    "usage": {"input_tokens": 10, "output_tokens": 0},
                },
            }
            try:
                self.wfile.write(("event: message_start\ndata: " + json.dumps(message) + "\n\n").encode())
                self.wfile.flush()
                while not finished.wait(0.1):
                    self.wfile.write(b'event: ping\ndata: {"type":"ping"}\n\n')
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="agentsmon-claude-probe-") as directory:
            # Do not inherit credentials, provider selection or Claude settings.
            env = {k: os.environ[k] for k in ("PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "SYSTEMROOT", "WINDIR") if k in os.environ}
            env.update(
                CLAUDE_CONFIG_DIR=directory,
                ANTHROPIC_API_KEY="synthetic-local-fixture",
                ANTHROPIC_BASE_URL=f"http://127.0.0.1:{server.server_port}",
                CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
            )
            process = subprocess.Popen([
                binary, "--bare", "-p", "--input-format", "stream-json",
                "--output-format", "stream-json", "--verbose", "--no-session-persistence",
                "--tools", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
                "--system-prompt", "Synthetic interruption fixture.",
                "--session-id", str(uuid.uuid4()),
            ], cwd=directory, env=env, stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True)
            path = Path(directory) / "sessions" / f"{process.pid}.json"

            def send(message):
                process.stdin.write(json.dumps(message) + "\n")
                process.stdin.flush()

            def status():
                try:
                    data = json.loads(path.read_text())
                    return {k: data.get(k) for k in ("status", "statusUpdatedAt", "version", "kind")}
                except (OSError, ValueError):
                    return {}

            def wait_for(expected, timeout):
                deadline = time.monotonic() + timeout
                result = {}
                while time.monotonic() < deadline:
                    result = status()
                    if result.get("status") == expected and request_started.is_set():
                        return result
                    if process.poll() is not None:
                        break
                    time.sleep(0.05)
                raise AssertionError(f"Expected {expected}; last metadata={result}; local API reached={request_started.is_set()}; process exit={process.poll()}")

            try:
                send({"type": "user", "message": {"role": "user", "content": "Synthetic interruption test."}})
                before = wait_for("busy", 20)
                print("Before interruption:", before, flush=True)
                send({"type": "control_request", "request_id": "interrupt-test", "request": {"subtype": "interrupt"}})
                after = wait_for("idle", 5)
                print("After interruption:", after, flush=True)
                assert after["statusUpdatedAt"] >= before["statusUpdatedAt"]
                print("PASS: explicit busy → idle during a synthetic model request.")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                process.stdin.close()
    finally:
        finished.set()
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
