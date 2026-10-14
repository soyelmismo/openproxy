#!/usr/bin/env python3
"""Capture the installed Claude CLI against a loopback mock, never real auth.

Run periodically by openproxy. Does not install/update/execute downloaded code.
An atomic profile is only emitted after a main interactive request is observed.
"""
import argparse
import http.server
import ipaddress
import json
import os
import pathlib
import pty
import re
import select
import shutil
import signal
import tempfile
import threading
import time

CONDITIONAL_BETAS = {
    "oauth-2025-04-20", "sec_" + format(0x5afb2fdf, "08x"), "sec_" + format(0x36bb7427, "08x"),
    "per-turn-control-2026-07-01", "effort-2025-11-24",
    "server-side-fallback-2026-06-01", "fallback-credit-2026-06-01",
    "extended-cache-ttl-2025-04-11", "redact-thinking-2026-02-12",
    "structured-outputs-2025-12-15",
}


def capture(cli, timeout, scratch):
    observed = []

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format, *args):
            pass

        def do_GET(self):
            self.reply({"data": []})

        def reply(self, value):
            data = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length < 4 * 1024 * 1024:
                self.send_error(413)
                return
            body = json.loads(self.rfile.read(length))
            if "/messages" in self.path and isinstance(body, dict):
                # Only retain the public contract, never auth or prompts.
                def cache_ttls(value):
                    if isinstance(value, dict):
                        cc = value.get("cache_control", {})
                        if isinstance(cc, dict) and "ttl" in cc:
                            yield cc["ttl"]
                        for child in value.values():
                            yield from cache_ttls(child)
                    elif isinstance(value, list):
                        for child in value:
                            yield from cache_ttls(child)
                ttl = next(cache_ttls(body), None)
                observed.append((
                    {key.lower(): self.headers.get(key, "") for key in (
                        "User-Agent", "X-Stainless-Package-Version",
                        "X-Stainless-Runtime-Version", "anthropic-beta")},
                    {key: body[key] for key in (
                        "thinking", "output_config", "context_management") if key in body},
                    ttl,
                ))
            self.reply({
                "id": "msg_profile_capture", "type": "message", "role": "assistant",
                "model": body.get("model", "claude-opus-5-5"),
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })

    server = http.server.ThreadingHTTPServer((str(ipaddress.IPv4Address(2130706433)), 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    os.makedirs(scratch, exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix="claude-profile-", dir=scratch) as directory:
            home = pathlib.Path(directory)
            work = home / "work"
            work.mkdir()
            mock_key = "sk-ant-api03-profile-capture-not-a-real-credential"
            (home / ".claude.json").write_text(json.dumps({
                "hasCompletedOnboarding": True, "theme": "dark",
                "customApiKeyResponses": {"approved": [mock_key[-20:]], "rejected": []},
                "projects": {str(work): {"hasTrustDialogAccepted": True}},
            }))
            settings = home / ".claude"
            settings.mkdir()
            (settings / "settings.json").write_text(json.dumps({
                "effortLevel": "high", "skipDangerousModePermissionPrompt": True,
            }))
            # Fresh HOME plus whitelist: no provider credentials or proxy env leak.
            env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ}
            env.update({
                "HOME": directory, "TERM": "xterm-256color",
                "ANTHROPIC_API_KEY": mock_key,
                "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "DISABLE_AUTOUPDATER": "1",
            })
            pid, fd = pty.fork()
            if pid == 0:
                # Do not orphan the CLI if the supervisor cancels this capture.
                import ctypes
                ctypes.CDLL(None).prctl(1, signal.SIGKILL, 0, 0, 0)
                os.chdir(work)
                os.execve(cli, [cli, "--model", "claude-opus-5-5", "--effort", "high"], env)
            start = time.monotonic()
            sent = False
            pending = b""
            try:
                while time.monotonic() - start < timeout:
                    ready, _, _ = select.select([fd], [], [], 0.2)
                    if ready:
                        try:
                            chunk = os.read(fd, 65536)
                        except OSError:
                            break
                        if not chunk:
                            break
                        pending = (pending + chunk)[-65536:]
                    if not sent and time.monotonic() - start > 5:
                        os.write(fd, b"Reply with exactly ok")
                        time.sleep(0.2)
                        os.write(fd, b"\r")
                        sent = True
                    if any(defaults.get("thinking", {}).get("display") == "updates"
                           for _, defaults, _ in observed):
                        break
            finally:
                for sig in (signal.SIGTERM, signal.SIGKILL):
                    try:
                        os.kill(pid, sig)
                    except ProcessLookupError:
                        pass
                os.close(fd)
                os.waitpid(pid, 0)
    finally:
        server.shutdown()
        server.server_close()
    for headers, defaults, ttl in reversed(observed):
        if defaults.get("thinking", {}).get("display") != "updates":
            continue
        match = re.fullmatch(r"claude-cli/(\d+\.\d+\.\d+) \(external, cli\)", headers["user-agent"])
        if not match or not re.fullmatch(r"\d+\.\d+\.\d+", headers["x-stainless-package-version"]):
            continue
        if not re.fullmatch(r"v\d+\.\d+\.\d+", headers["x-stainless-runtime-version"]):
            continue
        betas = [b.strip() for b in headers["anthropic-beta"].split(",") if b.strip()]
        if "claude-code-20250219" not in betas:
            continue
        return {
            "cli_version": match[1], "package_version": headers["x-stainless-package-version"],
            "runtime_version": headers["x-stainless-runtime-version"],
            "betas": [beta for beta in betas if beta not in CONDITIONAL_BETAS and not beta.startswith("mid-")],
            "request_defaults": defaults,
            "cache_ttl": ttl if ttl in ("1h", "5m") else "1h",
        }
    diagnostic = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", pending.decode("utf-8", "replace"))
    raise RuntimeError("no validated interactive Claude Code request captured: " + diagnostic[-4000:])


def installed_cli():
    """Resolve PATH first, then the native install under the service user's HOME."""
    cli = shutil.which("claude")
    if cli:
        return cli
    native = pathlib.Path.home() / ".local" / "bin" / "claude"
    if native.is_file() and os.access(native, os.X_OK):
        return str(native)
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", default=installed_cli())
    parser.add_argument("--output", required=True)
    parser.add_argument("--timeout", type=int, default=30)
    parser.add_argument("--scratch", default=os.environ.get("TMPDIR", str(pathlib.Path.home() / ".cache/openproxy")))
    args = parser.parse_args()
    if not args.cli:
        parser.error("installed claude executable not found")
    cli = str(pathlib.Path(args.cli).resolve())
    profile = capture(cli, args.timeout, args.scratch)
    defaults = profile["request_defaults"]
    if (defaults.get("thinking") != {"type": "adaptive", "display": "updates"}
            or defaults.get("output_config", {}).get("effort") not in ("low", "medium", "high", "xhigh", "max")
            or not isinstance(defaults.get("context_management"), dict)
            or not 0 < len(profile["betas"]) <= 64
            or any(not re.fullmatch(r"[a-zA-Z0-9_-]{1,128}", beta) for beta in profile["betas"])):
        raise RuntimeError("unsupported Claude profile, retaining last good cache")
    output = pathlib.Path(args.output)
    if output.exists():
        try:
            previous = json.loads(output.read_text())
            older = tuple(map(int, profile["cli_version"].split("."))) < tuple(map(int, previous["cli_version"].split(".")))
        except (ValueError, KeyError, TypeError):
            older = False
        if older:
            raise RuntimeError("Claude profile downgrade rejected, retaining last good cache")
    output.parent.mkdir(parents=True, exist_ok=True)
    # Atomically replace only on successful validated capture.
    fd, temporary = tempfile.mkstemp(prefix=".claude-profile-", dir=output.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(profile, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, output)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    print(json.dumps({"cli_version": profile["cli_version"], "package_version": profile["package_version"],
                      "beta_count": len(profile["betas"]), "output": str(output)}))


if __name__ == "__main__":
    main()
