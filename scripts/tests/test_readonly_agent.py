#!/usr/bin/env python3
"""Exercise the read-only worker boundary with local fixtures only."""
import contextlib
import hashlib
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
CODEX_NAMESPACE = {"type": "namespace", "name": "fixture", "tools": [
    {"type": "function", "name": "read", "parameters": {
        "type": "object", "properties": {"image_url": {"type": "string"}}}}
]}

WORKER = "CODEX_NAMESPACE = " + repr(CODEX_NAMESPACE) + "\n" + r"""
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
from urllib.parse import urlsplit

case, argument = sys.argv[1:]
if case == "credentials":
    host_home = Path(argument)
    assert host_home.is_absolute() and host_home != Path.home()
    for name in (".git-credentials", ".codex/auth.json", ".ssh/id_fixture"):
        try:
            with (host_home / name).open("rb"):
                raise AssertionError(f"worker read the host credential file: {name}")
        except (FileNotFoundError, PermissionError):
            pass
    assert "GH_TOKEN" not in os.environ, "host credential environment is visible"
    assert "HER_OPENAI_API_KEY" not in os.environ, "provider credential environment is visible"
    assert "GIT_CONFIG_COUNT" not in os.environ, "host Git configuration is visible"
    assert not Path.home().joinpath(".gitconfig").exists(), "host Git config file is visible"
elif case == "tls":
    certs = Path("/etc/ssl/certs")
    bundle = certs / "ca-certificates.crt"
    assert b"-----BEGIN CERTIFICATE-----" in bundle.read_bytes(), "worker cannot read the CA bundle"
    assert not os.access(bundle, os.W_OK), "worker can write the CA bundle"
    try:
        with (certs / argument).open("rb"):
            raise AssertionError("worker read the PEM beside the CA bundle")
    except (FileNotFoundError, PermissionError):
        pass
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
        payload = {"model": "fixture", "stream": True}
        if case == "codex":
            payload["tools"] = [CODEX_NAMESPACE]
        connection.request("POST", url.path + ("?" + url.query if url.query else ""),
                           body=json.dumps(payload), headers=with_headers)
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
    else:
        blocked.extend((url.path, "POST", body) for body in (
            '{"prompt":{"id":"pmpt_fixture"}}', '{"prompt":null}',
            '{"tools":[{"type":"namespace","tools":[{"type":"web_search"}]}]}',
            '{"input":[{"type":"input_image","image_url":"https://example.invalid"}]}'))
    for target, method, body in blocked:
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=5)
        connection.request(method, target, body, headers=with_headers)
        response = connection.getresponse()
        assert response.status >= 400, response.status
        connection.close()
elif case == "tmpfs":
    for path in (Path.home(), Path("/tmp")):
        mount = os.statvfs(path)
        assert mount.f_blocks * mount.f_frsize == 64 * 1024 * 1024, path
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


class WorkerFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "coordinator"
        self.home.mkdir()
        for name in (".git-credentials", ".codex/auth.json", ".ssh/id_fixture"):
            fixture = self.home / name
            fixture.parent.mkdir(exist_ok=True)
            fixture.write_text(CREDENTIAL)
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

    def launch(self, case, *, argument=None, port=1, provider="codex", mode="review", env=None, ca_bundle=None):
        # Inject the upstream in trusted test code, never through the launcher CLI or environment.
        harness = """
import http.client
from pathlib import Path
import runpy
import sys
launcher, port, ca_bundle = sys.argv[1:4]
sys.argv = [launcher, *sys.argv[4:]]
module = runpy.run_path(launcher)
if ca_bundle:
    module["worker_command"].__globals__["CA_BUNDLE"] = Path(ca_bundle)
raise SystemExit(module["main"](lambda _host: http.client.HTTPConnection(
    "127.0.0.1", int(port), timeout=3)))
"""
        return subprocess.run([sys.executable, "-I", "-c", harness, str(LAUNCHER), str(port),
                               str(ca_bundle) if ca_bundle else "",
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

class IsolationTests(WorkerFixture):
    def test_worker_cannot_read_host_credentials(self):
        self.completed(self.launch("credentials", argument=str(self.home)), "credentials")

    def test_worker_cannot_read_pem_beside_ca_bundle(self):
        certs = self.root / "certs"
        certs.mkdir()
        bundle = certs / "ca-certificates.crt"
        shutil.copyfile("/etc/ssl/certs/ca-certificates.crt", bundle)
        pem = certs / "fixture-key.pem"
        subprocess.run(["/usr/bin/openssl", "genpkey", "-algorithm", "ED25519", "-out", str(pem)],
                       check=True, timeout=5)
        self.completed(self.launch("tls", argument=pem.name, ca_bundle=bundle), "tls")

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
                expected["tools"] = [CODEX_NAMESPACE]
            self.assertEqual(json.loads(body), expected)
            if provider == "claude":
                self.assertEqual(headers["Anthropic-Version"], "local-version")
                self.assertNotIn("Authorization", headers)

    def test_review_uses_sanitized_history_and_read_only_inputs(self):
        self.completed(self.launch("review"), "review")

    def test_worker_temporary_filesystems_have_size_limits(self):
        self.completed(self.launch("tmpfs"), "tmpfs")

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


ROUTED_CLIENT = r"""
import http.client
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import time
import tomllib
from urllib.parse import urlsplit

provider, mode, action, requests = json.load(sys.stdin)
assert Path("/inputs/issue.md").read_text() == "issue and comments"
assert Path("/inputs/spec.md").read_text() == "approved excerpt"
assert "GH_TOKEN" not in os.environ
assert not (Path.home() / ".git-credentials").exists()
assert Path.cwd() == Path("/scratch" if mode == "plan" else "/repo")
try:
    Path("/repo/example").write_text("unapproved write")
except OSError:
    pass
else:
    raise AssertionError("routed client can write the repository export")
assert Path("/repo/example").read_text() == ("base" if mode == "plan" else "head")
assert not Path("/repo/untracked").exists()
if provider == "codex":
    assert sys.argv[sys.argv.index("-m") + 1] == "gpt-6.2-sol"
    assert 'model_reasoning_effort="high"' in sys.argv
    config = tomllib.loads((Path.home() / ".codex/config.toml").read_text())
    url = urlsplit(config["model_providers"]["her_relay"]["base_url"] + "/responses")
    if action == "runtime-files":
        assert Path(sys.argv[0]) == Path("/client/bin/codex"), "Codex lost its package layout"
        for name in ("bin/codex-exec-server", "bin/codex-code-mode-host", "codex-path/rg"):
            path = Path("/client") / name
            assert path.read_text() == "runtime fixture", name
            assert not os.access(path, os.W_OK), name
        assert shutil.which("rg") == "/client/codex-path/rg"
        assert not Path("/client/coordinator").exists()
else:
    assert "--print" in sys.argv and "--strict-mcp-config" in sys.argv
    url = urlsplit(os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages")
for _ in range(requests):
    connection = http.client.HTTPConnection(url.hostname, url.port, timeout=4)
    connection.request("POST", url.path, body=b'{"model":"fixture"}')
    connection.getresponse().read()
    connection.close()
if action == "timeout":
    time.sleep(15)
if action == "killed":
    os.kill(os.getpid(), signal.SIGKILL)
if mode == "plan":
    assert not Path("/scratch/.git").exists()
    assert Path("/scratch/example").read_text() == "base"
    Path("/scratch/example").write_text("edited by " + provider)
    plan = Path("/plan-out/plan.md")
    if action == "directory":
        plan.mkdir()
    elif action == "hardlink":
        other = plan.with_name("other")
        other.write_text("linked plan")
        plan.hardlink_to(other)
    elif action == "symlink":
        plan.symlink_to("/inputs/spec.md")
    elif action != "no-plan":
        plan.write_text("plan by " + provider)
    if action == "unsafe-scratch":
        Path("/scratch/link").symlink_to("/inputs/spec.md")
else:
    assert not Path("/scratch").exists() and not Path("/plan-out").exists()
report = "fixture report" if action != "no-report" else ""
if action == "report-api-error":
    report = "API Error: example from the model"
if action == "stderr-error" or (provider == "codex" and action == "error-result"):
    print("API Error: provider failed", file=sys.stderr)
if action in ("log-limit", "large-logs"):
    sys.stderr.write("x" * (1024 * 1024 + (action == "large-logs")))
if provider == "codex":
    Path(sys.argv[sys.argv.index("-o") + 1]).write_text(report)
    if action == "transcript-api-error":
        print("API Error: example from the model")
else:
    envelope = json.dumps({"type": "result", "subtype": "success", "result": report,
                           "is_error": action == "error-result"})
    if action in ("log-limit", "large-logs"):
        envelope = envelope.ljust(1024 * 1024 + (action == "large-logs"))
    sys.stdout.write(envelope)
if action == "nonzero":
    sys.exit(3)
# Simulate a client emitting a success-shaped result even after a provider error.
"""


class RoutingTests(WorkerFixture):
    def setUp(self):
        super().setUp()
        self.scripts = self.repo / "scripts"
        self.scripts.mkdir()
        for name in ("codex-task.sh", "codex-work.sh", "readonly-task.py"):
            source = LAUNCHER.with_name(name)
            if source.is_file():
                shutil.copy2(source, self.scripts / name)
        shutil.copytree(LAUNCHER.parents[1] / ".claude/agents", self.repo / ".claude/agents")
        (self.scripts / "readonly-agent.py").write_text(
            "import http.client, os, runpy, sys\n"
            "if 'FIXTURE_TIMEOUT' in os.environ:\n"
            "    sys.argv[sys.argv.index('--timeout') + 1] = os.environ['FIXTURE_TIMEOUT']\n"
            f"module = runpy.run_path({str(LAUNCHER)!r})\n"
            "raise SystemExit(module['main'](lambda _host: http.client.HTTPConnection("
            "'127.0.0.1', int(os.environ['FIXTURE_PORT']), timeout=3)))\n"
        )
        self.ticket = self.stage.parent / "85"
        self.out = self.ticket / "prompts"
        self.out.mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("codex", "claude"):
            client = self.bin / name
            client.write_text("#!/usr/bin/python3\n" + ROUTED_CLIENT)
            client.chmod(0o755)
        self.env.update(PATH=f"{self.bin}:/usr/bin:/bin", HER_ORCH=str(self.ticket.parent),
                        HER_READONLY_INPUTS=str(self.inputs), HER_READONLY_BASE=self.base)
        (self.inputs / "worker.py").unlink()
        (self.inputs / "prompt.md").unlink()

    def run_route(self, provider, mode, *, action="pass", status=200, drop=None, ticket="85"):
        prompt = self.out / "brief.md"
        statuses = status if isinstance(status, tuple) else (status,)
        prompt.write_text(json.dumps([provider, mode, action, len(statuses)]))
        name = "fixture"
        if provider == "claude":
            kind = self.repo if mode == "review" else ticket
            argv = [sys.executable, "-I", str(self.scripts / "readonly-task.py"),
                    provider, mode, name, str(kind), str(prompt)]
        elif mode == "plan":
            argv = ["bash", str(self.scripts / "codex-work.sh"), "plan", ticket, name, str(prompt)]
        else:
            argv = ["bash", str(self.scripts / "codex-task.sh"), name, str(self.repo), str(prompt)]
        expected_route = "/v1/responses" if provider == "codex" else "/v1/messages"
        self.requests = []
        requests = self.requests

        class ModelHandler(QuietHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                requests.append((self.path, self.headers.get("Authorization"), self.headers.get("X-Api-Key")))
                self.send_response(statuses[len(requests) - 1])
                self.end_headers()
                self.wfile.write(b"fixture")

        with server(ModelHandler) as port:
            env = dict(self.env, FIXTURE_PORT=str(port))
            if drop:
                del env[drop]
            if action == "timeout":
                env["FIXTURE_TIMEOUT"] = "2"
            result = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=10)
        if requests:
            expected = (expected_route, "Bearer " + MODEL_KEY, None) if provider == "codex" else (
                expected_route, None, MODEL_KEY)
            self.assertEqual(requests, [expected] * len(statuses))
        return result

    def receipt(self, suffix):
        return (self.out / f"fixture.{suffix}").read_text().strip()

    def assert_success(self, result):
        diagnostics = result.stdout + result.stderr
        log = self.out / "fixture.err.log"
        if log.exists():
            diagnostics += log.read_text()
        self.assertEqual(result.returncode, 0, diagnostics)
        self.assertEqual(self.receipt("exit.txt"), "0")
        self.assertEqual(self.receipt("report.md"), "fixture report")
        self.assertEqual(len(self.requests), 1)

    def test_codex_review_and_investigation_use_read_only_export(self):
        result = self.run_route("codex", "review")
        log = self.out / "fixture.err.log"
        if log.exists():
            self.assertNotIn("routed client can write the repository export", log.read_text())
        self.assert_success(result)
        self.assertEqual((self.repo / "example").read_text(), "head")

    def test_claude_review_collects_report(self):
        self.assert_success(self.run_route("claude", "review"))

    def test_codex_package_runtime_is_mounted_read_only(self):
        for name in ("bin/codex-exec-server", "bin/codex-code-mode-host", "codex-path/rg"):
            path = self.bin.parent / name
            path.parent.mkdir(exist_ok=True)
            path.write_text("runtime fixture")
            path.chmod(0o755)
        self.assert_success(self.run_route("codex", "review", action="runtime-files"))

    def test_both_planners_install_edits_and_run_fresh_rounds(self):
        for provider in ("codex", "claude", "claude"):
            with self.subTest(provider=provider):
                self.assert_success(self.run_route(provider, "plan"))
                self.assertEqual(self.receipt("plan.txt"), "installed")
                self.assertEqual((self.ticket / "scratch/example").read_text(), "edited by " + provider)
                plan = self.ticket / "plan.md"
                self.assertEqual(plan.read_text(), "plan by " + provider)
                self.assertEqual(plan.stat().st_nlink, 1)
                digest = hashlib.sha256(plan.read_bytes()).hexdigest()
                self.assertEqual((self.ticket / "plan.sha256").read_text().strip(), f"{digest}  plan.md")

    def test_model_failure_cannot_publish_success_shaped_report(self):
        for provider in ("codex", "claude"):
            for status, receipt in ((401, "no-report"), (429, "quota"), (503, "no-report")):
                with self.subTest(provider=provider, status=status):
                    result = self.run_route(provider, "review", status=status)
                    self.assertNotEqual(result.returncode, 0, "model failure was reported as success")
                    self.assertEqual(self.receipt("exit.txt"), receipt)
                    self.assertFalse((self.out / "fixture.report.md").exists())
                    self.assertEqual(len(self.requests), 1)

    def test_failed_run_after_success_and_last_primary_quota(self):
        for provider in ("codex", "claude"):
            for action in ("nonzero", "error-result", "no-report"):
                with self.subTest(provider=provider, action=action):
                    result = self.run_route(provider, "review", action=action, status=(200, 429))
                    self.assertEqual(result.returncode, 75)
                    self.assertEqual(self.receipt("exit.txt"), "quota")
                    self.assertFalse((self.out / "fixture.report.md").exists())
                    self.assertEqual(len(self.requests), 2)

    def test_earlier_quota_and_successful_run_do_not_report_quota(self):
        result = self.run_route("codex", "review", action="nonzero", status=(429, 200))
        self.assertEqual(result.returncode, 3)
        self.assertEqual(self.receipt("exit.txt"), "3")
        result = self.run_route("codex", "review", status=(429, 200))
        self.assertEqual(result.returncode, 0)
        self.assertEqual(self.receipt("exit.txt"), "0")
        self.assertEqual(self.receipt("report.md"), "fixture report")
        self.assertEqual(len(self.requests), 2)

    def test_final_primary_failure_rejects_partial_report(self):
        for provider in ("codex", "claude"):
            for status, code, receipt in ((429, 75, "quota"), (503, 69, "no-report")):
                with self.subTest(provider=provider, status=status):
                    result = self.run_route(provider, "review", status=(200, status))
                    self.assertEqual(len(self.requests), 2)
                    self.assertEqual(result.returncode, code, "final model failure was reported as success")
                    self.assertEqual(self.receipt("exit.txt"), receipt)
                    self.assertFalse((self.out / "fixture.report.md").exists())

    def test_missing_prerequisites_refuse_to_start_the_client(self):
        for key in ("HER_OPENAI_API_KEY", "HER_READONLY_INPUTS", "HER_READONLY_BASE"):
            with self.subTest(key=key):
                result = self.run_route("codex", "review", drop=key)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(key, result.stderr + self.receipt("err.log"))
                self.assertEqual(self.requests, [])

    def test_client_errors_and_empty_results_fail(self):
        for provider in ("codex", "claude"):
            for action in ("no-report", "error-result", "stderr-error", "nonzero"):
                with self.subTest(provider=provider, action=action):
                    result = self.run_route(provider, "review", action=action)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(len(self.requests), 1)

    def test_model_text_starting_with_api_error_is_valid_report_content(self):
        self.assert_success(self.run_route("codex", "review", action="transcript-api-error"))
        for provider in ("codex", "claude"):
            with self.subTest(provider=provider):
                result = self.run_route(provider, "review", action="report-api-error")
                self.assertEqual(result.returncode, 0)
                self.assertEqual(self.receipt("report.md"), "API Error: example from the model")

    def test_capture_logs_are_bounded(self):
        limit = 1024 * 1024
        self.assert_success(self.run_route("claude", "review", action="log-limit"))
        for suffix in ("out.log", "err.log"):
            self.assertEqual((self.out / f"fixture.{suffix}").stat().st_size, limit)
        result = self.run_route("claude", "review", action="large-logs")
        for suffix in ("out.log", "err.log"):
            self.assertEqual((self.out / f"fixture.{suffix}").stat().st_size, limit)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.receipt("exit.txt"), "no-report")
        self.assertFalse((self.out / "fixture.report.md").exists())

    def test_killed_workers_have_distinct_receipts(self):
        for provider in ("codex", "claude"):
            with self.subTest(provider=provider):
                result = self.run_route(provider, "review", action="killed")
                self.assertEqual(result.returncode, 137)
                self.assertEqual(self.receipt("exit.txt"), "killed")
                self.assertFalse((self.out / "fixture.report.md").exists())
                self.assertEqual(len(self.requests), 1)

    def test_timeouts_leave_no_report(self):
        for provider in ("codex", "claude"):
            with self.subTest(provider=provider):
                result = self.run_route(provider, "review", action="timeout")
                self.assertEqual(result.returncode, 124)
                self.assertEqual(self.receipt("exit.txt"), "timeout")
                self.assertFalse((self.out / "fixture.report.md").exists())
                self.assertEqual(len(self.requests), 1)

    def test_invalid_plan_keeps_previous_scratch_and_sha(self):
        self.assert_success(self.run_route("codex", "plan"))
        old = (self.ticket / "plan.sha256").read_bytes()
        for action in ("hardlink", "directory", "symlink", "no-plan", "unsafe-scratch"):
            with self.subTest(action=action):
                result = self.run_route("claude", "plan", action=action)
                self.assertNotEqual(result.returncode, 0, "worker-controlled plan was installed")
                self.assertEqual(self.receipt("plan.txt"), "no-plan")
                self.assertEqual((self.ticket / "plan.sha256").read_bytes(), old)
                self.assertEqual((self.ticket / "plan.md").read_text(), "plan by codex")
                self.assertEqual((self.ticket / "scratch/example").read_text(), "edited by codex")

    def test_install_refuses_non_file_sha_destination(self):
        (self.ticket / "plan.sha256").mkdir()
        result = self.run_route("codex", "plan")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.receipt("plan.txt"), "no-plan")
        self.assertFalse((self.ticket / "plan.md").exists())
        self.assertFalse((self.ticket / "scratch").exists())

    def test_plan_refuses_a_linked_scratch_destination(self):
        (self.ticket / "scratch").symlink_to(self.repo, target_is_directory=True)
        result = self.run_route("codex", "plan")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.receipt("plan.txt"), "no-plan")
        self.assertEqual((self.repo / "example").read_text(), "head")

    def test_plan_refuses_non_numeric_ticket(self):
        for value in ("x", "7/../8", "07x"):
            with self.subTest(ticket=value):
                result = self.run_route("codex", "plan", ticket=value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("positive ticket number", result.stderr)
                self.assertEqual(self.requests, [])


if __name__ == "__main__":
    if not sys.platform.startswith("linux"):
        raise SystemExit("read-only launcher tests require Linux")
    if not shutil.which("bwrap"):
        raise SystemExit("read-only launcher tests require bubblewrap")
    unittest.main(verbosity=2)
