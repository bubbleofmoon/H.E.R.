#!/usr/bin/env python3
"""Stage and launch one Linux review or planning worker."""
import argparse
import asyncio
import contextlib
import ctypes
import errno
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import socket
import socketserver
import stat
import subprocess
import sys
import tempfile
import threading
import time

PYTHON = "/usr/bin/python3"
GIT = "/usr/bin/git"
BWRAP = "/usr/bin/bwrap"
CA_BUNDLE = Path("/etc/ssl/certs/ca-certificates.crt")
MODEL_SOCKET = "/model.sock"
CODEX_REPORT = "/tmp/her-report.md"
MAX_REPORT = 1024 * 1024
TMPFS_SIZE = str(64 * 1024 * 1024)
WORKER_ENV = {
    "HOME": "/home/agent", "PATH": "/usr/bin:/bin", "TMPDIR": "/tmp",
    "LANG": "C.UTF-8", "USER": "agent", "LOGNAME": "agent",
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
}
PROVIDERS = {
    "codex": ("api.openai.com", "HER_OPENAI_API_KEY", {"/v1/responses", "/v1/responses/compact"}),
    "claude": ("api.anthropic.com", "HER_ANTHROPIC_API_KEY",
               {"/v1/messages", "/v1/messages?beta=true", "/v1/messages/count_tokens",
                "/v1/messages/count_tokens?beta=true"}),
}
MAX_BODY = 32 * 1024 * 1024
IDLE_TIMEOUT = 300


def upstream_connection(host):
    return http.client.HTTPSConnection(host, timeout=IDLE_TIMEOUT)


def has_remote_input(value):
    if isinstance(value, list):
        return any(map(has_remote_input, value))
    if isinstance(value, dict):
        return (value.get("type") in ("url", "file", "input_file")
                or ("image_url" in value and not str(value["image_url"]).startswith("data:"))
                or any(map(has_remote_input, value.values())))
    return False


def local_openai_tool(tool, allow_namespace=False):
    if not isinstance(tool, dict):
        return False
    kind = tool.get("type")
    if kind == "namespace" and allow_namespace:
        children = tool.get("tools")
        return (isinstance(children, list) and all(map(local_openai_tool, children))
                and not has_remote_input({key: value for key, value in tool.items() if key != "tools"}))
    if kind == "function":
        # Function parameters describe a JSON schema, where image_url can be a property name.
        return not has_remote_input({key: value for key, value in tool.items() if key != "parameters"})
    return kind == "custom" and not has_remote_input(tool)


def checked_model_body(body, provider):
    payload = json.loads(body)
    if not isinstance(payload, dict):
        raise ValueError
    openai = provider == "codex"
    tool_fields = ("tools", "additional_tools")
    for field in tool_fields:
        tools = payload.get(field)
        if tools is None:
            continue
        if not isinstance(tools, list) or not all(
            local_openai_tool(tool, allow_namespace=True) if openai else
            isinstance(tool, dict) and tool.get("type", "custom") == "custom" and "input_schema" in tool
            for tool in tools
        ):
            raise ValueError
    if has_remote_input({key: value for key, value in payload.items() if not openai or key not in tool_fields}):
        raise ValueError
    blocked = ("conversation", "background", "previous_response_id") if openai else ("mcp_servers", "container")
    if (openai and "prompt" in payload) or any(payload.get(field) for field in blocked):
        raise ValueError
    if openai:
        payload["store"] = False
    # Send the same JSON object we checked, with a single representation of each key.
    return json.dumps(payload, allow_nan=False, separators=(",", ":")).encode()


class ModelRelay(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True
    request_queue_size = 8

    def __init__(self, path, provider, key, connect):
        self.provider = provider
        self.host, _, self.routes = PROVIDERS[provider]
        self.auth = {"Authorization": "Bearer " + key} if provider == "codex" else {"X-Api-Key": key}
        self.connect = connect
        self.inference_status = None
        self.inference_succeeded = False
        self.slots = threading.BoundedSemaphore(self.request_queue_size)
        super().__init__(path, ModelRequest)

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            request.settimeout(IDLE_TIMEOUT)
            super().process_request_thread(request, address)
        finally:
            self.slots.release()

    def handle_error(self, request, address):
        # Tracebacks could capture provider traffic. Clients receive a closed stream.
        print("readonly-agent: model relay connection failed", file=sys.stderr)


class ModelRequest(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        self.close_connection = True
        if self.path not in self.server.routes:
            self.send_error(404)
            return
        lengths = self.headers.get_all("Content-Length", [])
        if self.headers.get("Transfer-Encoding") or len(lengths) != 1 or not re.fullmatch(r"[0-9]{1,9}", lengths[0]):
            self.send_error(411)
            return
        length = int(lengths[0])
        if length > MAX_BODY:
            self.send_error(413)
            return
        try:
            body = self.rfile.read(length)
            if len(body) != length:
                raise ValueError
            body = checked_model_body(body, self.server.provider)
        except (ValueError, RecursionError):
            self.send_error(400, "Unsupported model request")
            return
        headers = {"Content-Type": "application/json", **self.server.auth}
        for name in ("Accept", "Anthropic-Version", "Anthropic-Beta"):
            if name in self.headers:
                headers[name] = self.headers[name]
        connection = self.server.connect(self.server.host)
        with contextlib.closing(connection):
            primary = self.path in ("/v1/responses", "/v1/messages", "/v1/messages?beta=true")
            try:
                connection.request("POST", self.path, body, headers)
                response = connection.getresponse()
            except (http.client.HTTPException, OSError, ValueError):
                if primary:
                    self.server.inference_status = 502
                self.send_error(502)
                return
            if primary:
                self.server.inference_status = response.status
            # Route credentials stay at the fixed destination even if it responds with a redirect.
            if 300 <= response.status < 400:
                self.send_error(502)
                return
            self.send_response(response.status)
            for name in ("Content-Type", "Content-Encoding", "Retry-After", "Request-Id", "X-Request-Id"):
                if name in response.headers:
                    self.send_header(name, response.headers[name])
            self.send_header("Connection", "close")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            while chunk := response.read1(65536):
                self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
            if response.length in (None, 0):
                if primary and 200 <= response.status < 300:
                    self.server.inference_succeeded = True
                self.wfile.write(b"0\r\n\r\n")


def git(repo, *args, **kwargs):
    env = {"PATH": WORKER_ENV["PATH"], "LANG": "C.UTF-8", "GIT_CONFIG_NOSYSTEM": "1",
           "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_NO_REPLACE_OBJECTS": "1", "GIT_TERMINAL_PROMPT": "0",
           "GIT_NO_LAZY_FETCH": "1"}
    return subprocess.run([GIT, "-C", str(repo), "-c", "core.hooksPath=/dev/null",
                           "-c", "core.fsmonitor=false", "-c", "protocol.allow=never", *args],
                          env=env, check=True, **kwargs)


def export_history(source, checkout, head, base):
    git(source, "merge-base", "--is-ancestor", base, head)
    checkout.mkdir()
    git(checkout, "init", "--quiet", "--template=")
    # Copy reachable objects only. Preserve commit IDs for the review brief and leave HEAD detached.
    with tempfile.TemporaryFile(dir=checkout.parent) as pack:
        git(source, "pack-objects", "--revs", "--stdout", input=(head + "\n").encode(), stdout=pack)
        pack.seek(0)
        git(checkout, "index-pack", "--stdin", stdin=pack, stdout=subprocess.DEVNULL)
    # A shallow or otherwise incomplete source must never look like a full review.
    git(checkout, "rev-list", "--objects", "--missing=error", head, stdout=subprocess.DEVNULL)
    git(checkout, "checkout", "--quiet", "--detach", head)


def stage_run(args):
    inputs = args.stage / "inputs"
    checkout = args.stage / "repo"
    args.stage.mkdir(mode=0o700)
    inputs.mkdir()
    for source in args.inputs.iterdir():
        info = source.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise ValueError("approved input must be a regular file with one link")
        shutil.copyfile(source, inputs / source.name)
    if not (inputs / "issue.md").is_file():
        raise ValueError("approved inputs must include issue.md")
    if args.prompt_file and not (inputs / args.prompt_file).is_file():
        raise ValueError("prompt file must be among the approved inputs")
    passwd = f"agent:x:{os.getuid()}:{os.getgid()}:HER worker:/home/agent:/bin/sh\n"
    (args.stage / "passwd").write_text(passwd)
    (args.stage / "group").write_text(f"agent:x:{os.getgid()}:\n")
    (args.stage / "hosts").write_text("127.0.0.1 localhost her-worker\n::1 localhost\n")
    export_history(args.repo, checkout, args.head, args.base)
    if args.mode == "plan":
        shutil.copytree(checkout, args.stage / "scratch", symlinks=True,
                        ignore=lambda path, names: {".git"} if Path(path) == checkout else set())
        (args.stage / "plan-out").mkdir()


class SyscallArgument(ctypes.Structure):
    _fields_ = [("arg", ctypes.c_uint), ("op", ctypes.c_int),
                ("datum_a", ctypes.c_uint64), ("datum_b", ctypes.c_uint64)]


def export_seccomp(output):
    # Socket families such as AF_VSOCK are outside the network namespace's IP boundary.
    seccomp = ctypes.CDLL("libseccomp.so.2")
    seccomp.seccomp_init.argtypes = [ctypes.c_uint32]
    seccomp.seccomp_init.restype = ctypes.c_void_p
    seccomp.seccomp_release.argtypes = [ctypes.c_void_p]
    seccomp.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    seccomp.seccomp_rule_add_array.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int,
                                             ctypes.c_uint, ctypes.POINTER(SyscallArgument)]
    seccomp.seccomp_export_bpf.argtypes = [ctypes.c_void_p, ctypes.c_int]
    rules = seccomp.seccomp_init(0x7FFF0000)  # SCMP_ACT_ALLOW
    if not rules:
        raise ValueError("cannot initialize worker syscall filter")
    try:
        # Compat ABIs terminate under libseccomp's default bad-architecture action.
        for syscall in (b"socket", b"socketpair", b"io_uring_setup"):
            number = seccomp.seccomp_syscall_resolve_name(syscall)
            comparisons = None
            if syscall != b"io_uring_setup":
                # libseccomp permits one comparison per argument. Block the ranges between allowed families.
                comparisons = [
                    SyscallArgument(0, 2, socket.AF_UNIX, 0),  # SCMP_CMP_LT
                    *(SyscallArgument(0, 4, family, 0) for family in range(socket.AF_INET + 1, socket.AF_INET6)),
                    SyscallArgument(0, 6, socket.AF_INET6, 0),  # SCMP_CMP_GT
                ]
            for comparison in comparisons or [None]:
                pointer = ctypes.byref(comparison) if comparison is not None else None
                if number < 0 or seccomp.seccomp_rule_add_array(
                    rules, 0x50000 | errno.EPERM, number, int(comparison is not None), pointer
                ) != 0:
                    raise ValueError("cannot install worker syscall rule")
        if seccomp.seccomp_export_bpf(rules, output.fileno()) != 0:
            raise ValueError("cannot export worker syscall filter")
        output.seek(0)
    finally:
        seccomp.seccomp_release(rules)


def client_runtime(executable, provider):
    bin_dir = executable.parent.name
    if provider != "codex" or executable.name != "codex" or bin_dir not in ("bin", "codex"):
        return "/agent", ["--ro-bind", str(executable), "/agent"]
    package = executable.parent.parent
    client = f"/client/{bin_dir}/codex"
    mounts = ["--ro-bind", str(executable), client]
    for relative in (f"{bin_dir}/codex-exec-server", f"{bin_dir}/codex-code-mode-host",
                     "codex-package.json", "codex-path/rg", "path/rg"):
        source = package / relative
        if source.is_file() and source.resolve() == source:
            mounts += ["--ro-bind", str(source), f"/client/{relative}"]
    return client, mounts


def worker_command(args, relay_path, filter_fd):
    client, client_mounts = client_runtime(args.executable, args.provider)
    command = [
        BWRAP, "--new-session", "--die-with-parent", "--unshare-user", "--unshare-ipc",
        "--unshare-pid", "--unshare-uts", "--unshare-cgroup", "--disable-userns",
        "--unshare-net",
        "--cap-drop", "ALL", "--seccomp", str(filter_fd),
        "--clearenv", "--hostname", "her-worker",
        "--ro-bind", "/usr/bin", "/usr/bin", "--ro-bind", "/usr/lib", "/usr/lib",
        "--symlink", "usr/bin", "/bin",
        "--proc", "/proc", "--dev", "/dev", "--dir", "/etc",
        "--ro-bind", str(args.stage / "passwd"), "/etc/passwd",
        "--ro-bind", str(args.stage / "group"), "/etc/group",
        "--ro-bind", str(args.stage / "hosts"), "/etc/hosts",
        "--ro-bind-try", str(CA_BUNDLE), "/etc/ssl/certs/ca-certificates.crt",
        "--size", TMPFS_SIZE, "--tmpfs", "/home/agent",
        "--size", TMPFS_SIZE, "--tmpfs", "/tmp",
        "--ro-bind", str(args.stage / "repo"), "/repo",
        "--ro-bind", str(args.stage / "inputs"), "/inputs",
        "--ro-bind", str(relay_path), MODEL_SOCKET,
        "--ro-bind", str(Path(__file__).resolve()), "/launcher.py",
        *client_mounts,
    ]
    for name in ("lib", "lib64"):
        loader_dir = Path("/", name)
        if loader_dir.is_dir():
            usr_dir = Path("/usr", name)
            if loader_dir.is_symlink() and loader_dir.resolve() == usr_dir:
                if name != "lib":
                    command += ["--ro-bind", str(usr_dir), str(usr_dir)]
                command += ["--symlink", f"usr/{name}", str(loader_dir)]
            else:
                command += ["--ro-bind", str(loader_dir), str(loader_dir)]
    if args.mode == "plan":
        for name in ("scratch", "plan-out"):
            command += ["--bind", str(args.stage / name), f"/{name}"]
    for key, value in WORKER_ENV.items():
        command += ["--setenv", key, value]
    cwd = "/scratch" if args.mode == "plan" else "/repo"
    return command + ["--remount-ro", "/", "--chdir", cwd, "--", PYTHON, "-I", "/launcher.py",
                      "__worker", client, args.provider, args.prompt_file,
                      "capture" if args.capture_result else "", *args.command]


async def bridge(reader, writer):
    async def copy(source, destination):
        while data := await source.read(65536):
            destination.write(data)
            await destination.drain()
        destination.write_eof()

    try:
        upstream_reader, upstream_writer = await asyncio.open_unix_connection(MODEL_SOCKET)
        try:
            async with asyncio.TaskGroup() as group:
                group.create_task(copy(reader, upstream_writer))
                group.create_task(copy(upstream_reader, writer))
        finally:
            upstream_writer.close()
    except ExceptionGroup as error:
        if error.subgroup(lambda exc: not isinstance(exc, (BaseExceptionGroup, OSError))):
            raise
    except OSError:
        pass
    finally:
        writer.close()


def send_codex_report():
    try:
        fd = os.open(CODEX_REPORT, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as result:
            info = os.fstat(result.fileno())
            if stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= MAX_REPORT:
                sys.stdout.buffer.write(result.read(MAX_REPORT + 1))
    except OSError:
        pass


async def bootstrap(client, provider, prompt, capture, command):
    # Confirm the one authorized socket and private loopback are available before launching.
    _, probe = await asyncio.open_unix_connection(MODEL_SOCKET)
    probe.close()
    await probe.wait_closed()
    async with await asyncio.start_server(bridge, "127.0.0.1", 0) as listener:
        base = f"http://127.0.0.1:{listener.sockets[0].getsockname()[1]}"
        env = dict(os.environ)
        if provider == "codex":
            env["PATH"] = "/client/codex-path:/client/path:" + env["PATH"]
            config = Path.home() / ".codex" / "config.toml"
            config.parent.mkdir()
            config.write_text(f'''model_provider = "her_relay"
web_search = "disabled"
[model_providers.her_relay]
name = "HER model relay"
base_url = "{base}/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
''')
        else:
            env.update(ANTHROPIC_BASE_URL=base, ANTHROPIC_API_KEY="local-relay",
                       CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")
        prompt_path = Path("/inputs") / prompt if prompt else os.devnull
        codex_report = capture and provider == "codex"
        with open(prompt_path, "rb") as source:
            child = await asyncio.create_subprocess_exec(
                client, *command, env=env, stdin=source,
                stdout=asyncio.subprocess.DEVNULL if codex_report else None)
        status = await child.wait()
        if codex_report:
            send_codex_report()
        return status if status >= 0 else 128 - status


def run_worker(command, filter_fd, timeout, capture):
    deadline = time.monotonic() + timeout
    # Give the worker pipes and a null input. It never inherits coordinator report file descriptors.
    with subprocess.Popen(command, env={}, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE if capture else subprocess.STDOUT,
                          pass_fds=(filter_fd,)) as child:
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(child.stdout, selectors.EVENT_READ, sys.stdout.buffer)
                if capture:
                    ready.register(child.stderr, selectors.EVENT_READ, sys.stderr.buffer)
                while ready.get_map():
                    events = ready.select(max(0, deadline - time.monotonic()))
                    if not events or time.monotonic() >= deadline:
                        break
                    for event, _ in events:
                        data = os.read(event.fd, 65536)
                        if data:
                            event.data.write(data)
                            event.data.flush()
                        else:
                            ready.unregister(event.fd)
                else:
                    return child.wait(timeout=max(0, deadline - time.monotonic()))
            raise subprocess.TimeoutExpired(command, timeout)
        finally:
            if child.poll() is None:
                child.kill()


def run_relay(args, key, connect):
    # Bind through a short descriptor path even when the ticket's absolute path is long.
    stage_fd = os.open(args.stage, os.O_DIRECTORY)
    relay_path = args.stage / "model.sock"
    try:
        with ModelRelay(f"/proc/self/fd/{stage_fd}/model.sock", args.provider, key, connect) as relay:
            thread = threading.Thread(target=relay.serve_forever)
            thread.start()
            try:
                with tempfile.TemporaryFile(dir=args.stage) as syscall_filter:
                    export_seccomp(syscall_filter)
                    fd = syscall_filter.fileno()
                    returncode = run_worker(worker_command(args, relay_path, fd), fd, args.timeout,
                                            args.capture_result)
                    status = returncode if returncode >= 0 else 128 - returncode
                    if args.capture_result:
                        primary_status = relay.inference_status
                        if primary_status is not None and not 200 <= primary_status < 300:
                            if primary_status == 429:
                                return 75
                            if status == 0:
                                print(f"readonly-agent: final model inference returned HTTP {primary_status}",
                                      file=sys.stderr)
                                return 69
                        if status == 0 and not relay.inference_succeeded:
                            print("readonly-agent: no successful model inference", file=sys.stderr)
                            return 69
                    return status
            finally:
                relay.shutdown()
                thread.join()
    finally:
        os.close(stage_fd)
        relay_path.unlink(missing_ok=True)


def main(connect=upstream_connection):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--base", required=True)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--stage", type=Path, required=True)
    parser.add_argument("--mode", choices=("review", "plan"), required=True)
    parser.add_argument("--provider", choices=PROVIDERS, required=True)
    parser.add_argument("--exec", type=Path, required=True, dest="executable")
    parser.add_argument("--prompt-file", default="")
    parser.add_argument("--capture-result", action="store_true")
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not sys.platform.startswith("linux") or not os.access(BWRAP, os.X_OK):
        parser.error("Linux and /usr/bin/bwrap are required")
    if not all(re.fullmatch(r"[a-f0-9]{40}", value) for value in (args.head, args.base)):
        parser.error("head and base must be full commit IDs")
    if args.command[:1] != ["--"] or args.timeout <= 0:
        parser.error("supply -- followed by worker arguments and a positive timeout")
    if args.prompt_file and Path(args.prompt_file).name != args.prompt_file:
        parser.error("prompt file must be a filename in the approved input directory")
    args.command = args.command[1:]
    _, env_key, _ = PROVIDERS[args.provider]
    key = os.environ.get(env_key, "")
    if not key or not re.fullmatch(r"[!-~]+", key):
        parser.error(f"{env_key} is required for the host model relay")
    try:
        for name in ("repo", "inputs", "executable"):
            setattr(args, name, getattr(args, name).resolve(strict=True))
        args.stage = args.stage.absolute()
        if not args.executable.is_file() or not os.access(args.executable, os.X_OK):
            raise ValueError("worker executable is unavailable")
        stage_run(args)
        return run_relay(args, key, connect)
    except subprocess.TimeoutExpired:
        print("readonly-agent: worker timed out", file=sys.stderr)
        return 124
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"readonly-agent: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    if sys.argv[1:2] == ["__worker"]:
        raise SystemExit(asyncio.run(bootstrap(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6:])))
    raise SystemExit(main())
