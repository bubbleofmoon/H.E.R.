#!/usr/bin/env python3
"""Exercise the read-only worker boundary with local fixtures only."""
import contextlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

LAUNCHER = Path(__file__).resolve().parents[1] / "readonly-agent.py"
PYTHON = Path("/usr/bin/python3")
CREDENTIAL = "synthetic-coordinator-value"
MODEL_KEY = "synthetic-provider-value"

WORKER = r"""
import http.client
import os
from pathlib import Path
import subprocess
import sys
from urllib.parse import urlsplit

case, argument = sys.argv[1:]
if case == "credentials":
    try:
        credential = Path.home().joinpath(".git-credentials").read_text()
    except (FileNotFoundError, PermissionError):
        credential = None
    assert credential is None, "worker read the host credential file"
    assert "GH_TOKEN" not in os.environ, "host credential environment is visible"
    assert "HER_OPENAI_API_KEY" not in os.environ, "provider credential environment is visible"
    assert "GIT_CONFIG_COUNT" not in os.environ, "host Git configuration is visible"
    assert not Path.home().joinpath(".gitconfig").exists(), "host Git config file is visible"
elif case == "push":
    result = subprocess.run(["git", "push", argument, "HEAD:refs/heads/worker"],
                            capture_output=True, text=True, timeout=5)
    assert result.returncode != 0, "worker pushed to the host Git endpoint"
elif case in ("codex", "claude"):
    if case == "codex":
        import tomllib
        provider = tomllib.loads(Path.home().joinpath(".codex/config.toml").read_text())
        base = provider["model_providers"]["her_relay"]["base_url"]
        assert not provider["model_providers"]["her_relay"]["supports_websockets"]
        routes = [base + "/responses", base + "/responses/compact"]
    else:
        base = os.environ["ANTHROPIC_BASE_URL"]
        assert os.environ["ANTHROPIC_API_KEY"] == "local-relay"
        routes = [base + "/v1/messages?beta=true", base + "/v1/messages/count_tokens"]
    first = None
    for route in routes:
        url = urlsplit(route)
        with_headers = {"Content-Type": "application/json", "Authorization": "Bearer local-relay",
                        "Anthropic-Version": "local-version", "OpenAI-Organization": "local-org"}
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=5)
        connection.request("POST", url.path + ("?" + url.query if url.query else ""),
                           body=b'{"model":"fixture","stream":true}', headers=with_headers)
        response = connection.getresponse()
        assert response.status == 200, response.status
        assert response.getheader("Content-Type") == "text/event-stream"
        assert response.readline() == b"data: fixture-event\n"
        if first is None:
            first = connection, response
        else:
            assert response.read() == b"\n"
            connection.close()
    connection, response = first
    assert response.read() == b"\ndata: fixture-end\n\n"
    connection.close()
    blocked = [
        ("/unapproved", "POST", "{}"),
        ("http://example.invalid/v1/responses", "POST", "{}"),
        (url.path, "CONNECT", "{}"),
        (url.path, "POST", '{"tools":[{"type":"mcp","server_url":"https://example.invalid"}]}'),
        (url.path, "POST", '{"payload":{"type":"url","url":"https://example.invalid"}}'),
    ]
    if case == "claude":
        blocked.append((url.path, "POST", '{"mcp_servers":[{"url":"https://example.invalid"}]}'))
    for target, method, body in blocked:
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=5)
        connection.request(method, target, body, headers=with_headers)
        response = connection.getresponse()
        assert response.status >= 400, response.status
        connection.close()
elif case in ("review", "plan"):
    def git(*args):
        return subprocess.check_output(["git", "-C", "/repo", *args], text=True).strip()
    assert git("log", "--format=%s").splitlines() == ["head", "base"]
    assert git("remote", "-v") == ""
    assert git("for-each-ref", "--format=%(refname)") == ""
    assert subprocess.run(["git", "config", "--get-regexp", r"^remote\."]).returncode == 1
    assert subprocess.run(["git", "-C", "/repo", "cat-file", "-e", argument]).returncode != 0
    assert Path("/inputs/issue.md").read_text() == "issue and comments"
    assert Path("/inputs/spec.md").read_text() == "approved excerpt"
    assert sys.stdin.read() == "staged prompt"
    assert not Path("/repo/untracked").exists()
    assert not Path("/repo/.git/objects/info/alternates").exists()
    for path in ("/repo/example", "/inputs/issue.md"):
        try:
            Path(path).write_text("forbidden")
        except OSError:
            pass
        else:
            raise AssertionError("read-only input is writable")
    if case == "plan":
        assert Path.cwd() == Path("/scratch")
        assert not Path("/scratch/.git").exists()
        Path("/scratch/example").write_text("planned change")
        Path("/plan-out/plan.md").write_text("plan")
    else:
        assert Path.cwd() == Path("/repo")
        assert not Path("/scratch").exists()
        assert not Path("/plan-out").exists()
print("worker completed", case)
"""


@contextlib.contextmanager
def server(handler):
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler) as listener:
        thread = threading.Thread(target=listener.serve_forever)
        thread.start()
        try:
            yield listener.server_port
        finally:
            listener.shutdown()
            thread.join()


class QuietHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass


class IsolationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "coordinator"
        self.home.mkdir()
        (self.home / ".git-credentials").write_text(CREDENTIAL)
        (self.home / ".gitconfig").write_text("[user]\n\tname = Coordinator Fixture\n")
        self.env = {
            "HOME": str(self.home), "PATH": "/usr/bin:/bin", "GH_TOKEN": CREDENTIAL,
            "HER_OPENAI_API_KEY": MODEL_KEY, "HER_ANTHROPIC_API_KEY": MODEL_KEY,
            "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "remote.unrelated.url",
            "GIT_CONFIG_VALUE_0": "https://example.invalid/private",
        }
        self.repo = self.root / "source"
        self.git("init", str(self.repo))
        self.git("config", "user.name", "Fixture Author")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("remote", "add", "origin", "https://example.invalid/private")
        (self.repo / "example").write_text("base")
        self.git("add", "example")
        self.git("commit", "-m", "base")
        self.base = self.git("rev-parse", "HEAD")
        (self.repo / "example").write_text("head")
        self.git("commit", "-am", "head")
        self.head = self.git("rev-parse", "HEAD")
        self.git("checkout", "--detach", self.base)
        (self.repo / "example").write_text("unrelated")
        self.git("commit", "-am", "unrelated")
        self.git("branch", "unrelated", "HEAD")
        self.unrelated = self.git("rev-parse", "HEAD")
        self.git("checkout", "--detach", self.head)
        (self.repo / "untracked").write_text(CREDENTIAL)
        self.inputs = self.root / "approved"
        self.inputs.mkdir()
        (self.inputs / "issue.md").write_text("issue and comments")
        (self.inputs / "spec.md").write_text("approved excerpt")
        (self.inputs / "prompt.md").write_text("staged prompt")
        (self.inputs / "worker.py").write_text(WORKER)
        self.stage = self.root / "ticket" / "run"
        self.stage.parent.mkdir()

    def git(self, *args, cwd=None):
        if cwd is None:
            cwd = self.repo if self.repo.exists() else self.root
        return subprocess.check_output(["git", *args], cwd=cwd, text=True,
                                       stderr=subprocess.PIPE).strip()

    def launch(self, case, *, argument=None, port=1, provider="codex", mode="review", env=None):
        # Inject the upstream in trusted test code, never through the launcher CLI or environment.
        harness = """
import http.client
import runpy
import sys
launcher, port = sys.argv[1:3]
sys.argv = [launcher, *sys.argv[3:]]
module = runpy.run_path(launcher)
raise SystemExit(module["main"](lambda _host: http.client.HTTPConnection(
    "127.0.0.1", int(port), timeout=3)))
"""
        return subprocess.run([sys.executable, "-I", "-c", harness, str(LAUNCHER), str(port),
                               "--repo", str(self.repo), "--head", self.head, "--base", self.base,
                               "--inputs", str(self.inputs), "--stage", str(self.stage),
                               "--provider", provider, "--mode", mode, "--exec", str(PYTHON),
                               "--prompt-file", "prompt.md",
                               "--timeout", "12", "--", "-I", "/inputs/worker.py", case,
                               argument or self.unrelated],
                              env=self.env if env is None else env, capture_output=True,
                              text=True, timeout=20)

    def completed(self, result, case):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("worker completed " + case, result.stdout)

    def test_worker_cannot_read_host_credentials(self):
        self.completed(self.launch("credentials"), "credentials")

    def test_worker_cannot_push_to_host_git_endpoint(self):
        remote = self.root / "receiver.git"
        self.git("init", "--bare", str(remote))

        class GitHandler(QuietHandler):
            def do_GET(self):
                self.respond()

            def do_POST(self):
                self.respond()

            def respond(self):
                path, _, query = self.path.partition("?")
                length = int(self.headers.get("Content-Length", "0"))
                env = dict(os.environ, GIT_PROJECT_ROOT=str(remote.parent), GIT_HTTP_EXPORT_ALL="1",
                           PATH_INFO=path, QUERY_STRING=query, REQUEST_METHOD=self.command,
                           CONTENT_TYPE=self.headers.get("Content-Type", ""), CONTENT_LENGTH=str(length))
                result = subprocess.check_output(["git", "-c", "http.receivepack=true", "http-backend"],
                                                 env=env, input=self.rfile.read(length), timeout=5)
                headers, body = result.split(b"\r\n\r\n", 1)
                self.send_response(200)
                for line in headers.decode().splitlines():
                    name, value = line.split(":", 1)
                    self.send_header(name, value.strip())
                self.end_headers()
                self.wfile.write(body)

        with server(GitHandler) as port:
            url = f"http://127.0.0.1:{port}/receiver.git"
            self.git("push", url, "HEAD:refs/heads/host-control")
            self.completed(self.launch("push", argument=url), "push")
        refs = self.git("for-each-ref", "--format=%(refname)", cwd=remote)
        self.assertEqual(refs, "refs/heads/host-control")

    def test_model_relay_codex(self):
        self.check_model_relay("codex", ["/v1/responses", "/v1/responses/compact"], "Authorization",
                               "Bearer " + MODEL_KEY)

    def test_model_relay_claude(self):
        self.check_model_relay("claude", ["/v1/messages?beta=true", "/v1/messages/count_tokens"],
                               "X-Api-Key", MODEL_KEY)

    def check_model_relay(self, provider, paths, header, key):
        requests = []
        next_request = threading.Event()

        class ModelHandler(QuietHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                requests.append((self.path, self.headers, body))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(b"data: fixture-event\n\n")
                if self.path == paths[0]:
                    if next_request.wait(6):
                        self.wfile.write(b"data: fixture-end\n\n")
                else:
                    next_request.set()

        with server(ModelHandler) as port:
            self.completed(self.launch(provider, provider=provider, port=port), provider)
        self.assertEqual([request[0] for request in requests], paths)
        for _, headers, body in requests:
            self.assertEqual(headers[header], key)
            self.assertNotIn("OpenAI-Organization", headers)
            expected = {"model": "fixture", "stream": True}
            if provider == "codex":
                expected["store"] = False
            self.assertEqual(json.loads(body), expected)
            if provider == "claude":
                self.assertEqual(headers["Anthropic-Version"], "local-version")
                self.assertNotIn("Authorization", headers)

    def test_review_uses_sanitized_history_and_read_only_inputs(self):
        self.completed(self.launch("review"), "review")

    def test_plan_writes_only_its_scratch_and_plan_output(self):
        self.completed(self.launch("plan", mode="plan"), "plan")
        self.assertEqual((self.stage / "scratch/example").read_text(), "planned change")
        self.assertEqual((self.stage / "plan-out/plan.md").read_text(), "plan")
        self.assertEqual((self.repo / "example").read_text(), "head")

    def test_missing_model_key_refuses_to_start(self):
        env = dict(self.env)
        del env["HER_OPENAI_API_KEY"]
        result = self.launch("review", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HER_OPENAI_API_KEY", result.stderr)
        self.assertFalse(self.stage.exists())

    def test_bad_approved_input_refuses_to_start(self):
        (self.inputs / "linked").symlink_to(self.home / ".git-credentials")
        result = self.launch("review")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("approved input must be a regular file", result.stderr)
        self.assertNotIn("worker completed", result.stdout)


if __name__ == "__main__":
    if not sys.platform.startswith("linux"):
        raise SystemExit("read-only launcher tests require Linux")
    if not shutil.which("bwrap"):
        raise SystemExit("read-only launcher tests require bubblewrap")
    unittest.main(verbosity=2)
