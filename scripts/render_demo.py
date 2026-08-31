#!/usr/bin/env python3
"""Record the current Agul workbench against a deterministic local provider.

The provider is warmed by one hidden, real Agul turn before VHS starts.  The
visible turn is allowed to report cache hits only when each request exactly
matches the corresponding request from that warm-up turn.
"""

from __future__ import annotations

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
MODEL = "local-fixture"
API_KEY = "local-demo-key"
PROMPT = "@skill:project-summary Update STATUS.md from PROJECT.md, then verify it."
UNCACHEABLE_REQUEST_TOKENS = 5
PROJECT = """# Project

Agul is a small agent runtime with four core tools.
Agulater prepares context and installs extensions.
AgentKube publishes optional skills and plugins.
"""
STATUS = """# Status

Agul runs the loop; Agulater prepares it; AgentKube extends it.
"""


def completion(
    response_id: str,
    delta: dict[str, Any],
    finish_reason: str | None = None,
    usage: dict[str, Any] | None = None,
) -> dict[str, Any]:
    value: dict[str, Any] = {
        "id": response_id,
        "object": "chat.completion.chunk",
        "model": MODEL,
        "choices": [
            {"index": 0, "delta": delta, "finish_reason": finish_reason}
        ],
    }
    if usage is not None:
        value["usage"] = usage
    return value


def usage(round_number: int, *, exact_replay: bool) -> dict[str, Any]:
    prompt_tokens = 1_500 + round_number * 120
    cache_miss = UNCACHEABLE_REQUEST_TOKENS if exact_replay else prompt_tokens
    cache_hit = prompt_tokens - cache_miss
    return {
        "prompt_tokens": prompt_tokens,
        "completion_tokens": 120,
        "total_tokens": prompt_tokens + 120,
        "prompt_cache_hit_tokens": cache_hit,
        "prompt_cache_miss_tokens": cache_miss,
        "completion_tokens_details": {"reasoning_tokens": 35},
    }


def is_exact_replay(warmed: dict[str, Any], visible: dict[str, Any]) -> bool:
    return warmed == visible


def tool_delta(response_id: str, call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return completion(
        response_id,
        {
            "tool_calls": [
                {
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": json.dumps(arguments, separators=(",", ":")),
                    },
                }
            ]
        },
    )


ROUNDS = [
    (
        "Read the source before changing the status.",
        tool_delta("demo-read-project", "read-project", "read", {"path": "PROJECT.md"}),
        "tool_calls",
    ),
    (
        "The source is clear; write the concise status.",
        tool_delta(
            "demo-write-status",
            "write-status",
            "write",
            {"path": "STATUS.md", "content": STATUS},
        ),
        "tool_calls",
    ),
    (
        "Re-read the file to verify the change.",
        tool_delta("demo-read-status", "read-status", "read", {"path": "STATUS.md"}),
        "tool_calls",
    ),
    (
        "The saved text matches the project source.",
        completion(
            "demo-final",
            {"content": "Done: STATUS.md is updated and verified."},
        ),
        "stop",
    ),
]


class DemoProvider:
    def __init__(self) -> None:
        self.warmup_requests: list[dict[str, Any]] = []
        self.requests: list[dict[str, Any]] = []
        self.visible_usage: list[dict[str, Any]] = []
        self.recording = False
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self) -> None:  # noqa: N802
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    request = json.loads(self.rfile.read(length))
                    if self.path != "/v1/chat/completions":
                        self.send_error(400)
                        return
                    with owner.lock:
                        requests = owner.requests if owner.recording else owner.warmup_requests
                        index = len(requests)
                        if index >= len(ROUNDS):
                            self.send_error(400)
                            return
                        exact_replay = owner.recording and is_exact_replay(
                            owner.warmup_requests[index], request
                        )
                        if owner.recording and not exact_replay:
                            self.send_error(
                                409,
                                f"visible request {index + 1} does not match its warm-up request",
                            )
                            return
                        requests.append(request)
                        reported_usage = usage(index + 1, exact_replay=exact_replay)
                        if owner.recording:
                            owner.visible_usage.append(reported_usage)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Cache-Control", "no-cache")
                    self.send_header("Connection", "close")
                    self.end_headers()

                    reasoning, action, finish_reason = ROUNDS[index]
                    response_id = f"demo-{index + 1}"
                    for offset in range(0, len(reasoning), 12):
                        delta = {"reasoning_content": reasoning[offset : offset + 12]}
                        if offset == 0:
                            delta["role"] = "assistant"
                        self.event(completion(response_id, delta))
                        time.sleep(0.10)
                    self.event(action)
                    time.sleep(0.12)
                    self.event(
                        completion(
                            response_id,
                            {},
                            finish_reason=finish_reason,
                            usage=reported_usage,
                        )
                    )
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                    self.close_connection = True
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def event(self, value: dict[str, Any]) -> None:
                body = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
                self.wfile.write(f"data: {body}\n\n".encode())
                self.wfile.flush()

            def log_message(self, *_: object) -> None:
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        self.base_url = f"http://{host}:{port}/v1"

    def begin_recording(self) -> None:
        with self.lock:
            if len(self.warmup_requests) != len(ROUNDS):
                raise SystemExit(
                    f"expected {len(ROUNDS)} warm-up requests, received "
                    f"{len(self.warmup_requests)}"
                )
            self.recording = True

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def __enter__(self) -> DemoProvider:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def prepare_workspace(root: Path) -> tuple[Path, Path, Path]:
    workspace = root / "workspace"
    home = root / "home"
    state = root / "state"
    skill = workspace / ".agents" / "skills" / "project-summary"
    skill.mkdir(parents=True)
    home.mkdir()
    state.mkdir()
    (workspace / "PROJECT.md").write_text(PROJECT, encoding="utf-8", newline="\n")
    (skill / "SKILL.md").write_text(
        """---
name: project-summary
description: Keep STATUS.md aligned with PROJECT.md.
---

Read PROJECT.md, update STATUS.md with one verified sentence, then read it back.
""",
        encoding="utf-8",
        newline="\n",
    )
    return workspace, home, state


def write_launcher(
    directory: Path,
    binary: Path,
    workspace: Path,
    home: Path,
    state: Path,
    base_url: str,
) -> Path:
    if os.name == "nt":
        launcher = directory / "agul-demo.cmd"
        launcher.write_text(
            "\r\n".join(
                [
                    "@echo off",
                    'set "NO_COLOR="',
                    'set "TERM=xterm-256color"',
                    f'set "HOME={home}"',
                    f'set "USERPROFILE={home}"',
                    f'set "AGUL_DEMO_API_KEY={API_KEY}"',
                    f'"{binary}" chat --workspace "{workspace}" --state-dir "{state}" '
                    f'--base-url "{base_url}" --model "{MODEL}" '
                    '--api-key-env AGUL_DEMO_API_KEY --reasoning-effort high --no-session '
                    '--context-window 32768 --timeout-seconds 20',
                    "",
                ]
            ),
            encoding="utf-8",
        )
    else:
        launcher = directory / "agul-demo"
        launcher.write_text(
            "#!/bin/sh\n"
            "unset NO_COLOR\n"
            "export TERM=xterm-256color\n"
            f"export HOME='{home}' USERPROFILE='{home}' AGUL_DEMO_API_KEY='{API_KEY}'\n"
            f"exec '{binary}' chat --workspace '{workspace}' --state-dir '{state}' "
            f"--base-url '{base_url}' --model '{MODEL}' "
            "--api-key-env AGUL_DEMO_API_KEY --reasoning-effort high --no-session "
            "--context-window 32768 --timeout-seconds 20\n",
            encoding="utf-8",
            newline="\n",
        )
        launcher.chmod(0o755)
    return launcher


def warm_provider(
    binary: Path,
    workspace: Path,
    home: Path,
    state: Path,
    base_url: str,
    environment: dict[str, str],
) -> None:
    warm_environment = environment.copy()
    warm_environment.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "AGUL_DEMO_API_KEY": API_KEY,
            "TERM": "xterm-256color",
        }
    )
    warm_environment.pop("NO_COLOR", None)
    subprocess.run(
        [
            str(binary),
            "chat",
            "--workspace",
            str(workspace),
            "--state-dir",
            str(state),
            "--base-url",
            base_url,
            "--model",
            MODEL,
            "--api-key-env",
            "AGUL_DEMO_API_KEY",
            "--reasoning-effort",
            "high",
            "--no-session",
            "--context-window",
            "32768",
            "--timeout-seconds",
            "20",
            "--prompt",
            PROMPT,
            "--json",
        ],
        cwd=ROOT,
        env=warm_environment,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def verify(
    workspace: Path,
    warmup_requests: list[dict[str, Any]],
    requests: list[dict[str, Any]],
    visible_usage: list[dict[str, Any]],
) -> None:
    if (workspace / "STATUS.md").read_text(encoding="utf-8") != STATUS:
        raise SystemExit("the real write/read loop did not produce the expected STATUS.md")
    if len(requests) != 4:
        raise SystemExit(f"expected four model rounds, received {len(requests)}")
    if warmup_requests != requests:
        raise SystemExit("visible provider requests did not exactly replay the warm-up requests")
    expected_hits = sum(item["prompt_tokens"] - UNCACHEABLE_REQUEST_TOKENS for item in visible_usage)
    observed_hits = sum(item["prompt_cache_hit_tokens"] for item in visible_usage)
    if len(visible_usage) != 4 or observed_hits != expected_hits:
        raise SystemExit("visible cache telemetry was not derived from four exact replay hits")
    user_messages = [
        message.get("content", "")
        for message in requests[0].get("messages", [])
        if message.get("role") == "user"
    ]
    if not any(PROMPT in str(message) for message in user_messages):
        raise SystemExit("the recorded prompt or expanded Skill was not sent to the model")
    expected_calls = ["read-project", "write-status", "read-status"]
    for request, call_id in zip(requests[1:], expected_calls):
        observed = {
            message.get("tool_call_id")
            for message in request.get("messages", [])
            if message.get("role") == "tool"
        }
        if call_id not in observed:
            raise SystemExit(f"missing real tool result for {call_id}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()
    if os.name == "nt":
        raise SystemExit("record the release GIF from Linux or macOS; VHS 0.11 is not reliable on Windows")
    binary = ROOT / "target" / "release" / ("agul.exe" if os.name == "nt" else "agul")
    if not args.skip_build:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, check=True)
    if not binary.is_file():
        raise SystemExit(f"missing release binary: {binary}")
    if shutil.which("vhs") is None:
        raise SystemExit("VHS 0.11+ is required: https://github.com/charmbracelet/vhs")

    output = ROOT / "docs" / "assets" / "agul-demo.gif"
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="agul-demo-") as temporary, DemoProvider() as provider:
        temporary_root = Path(temporary)
        workspace, home, state = prepare_workspace(temporary_root)
        bin_directory = temporary_root / "bin"
        bin_directory.mkdir()
        write_launcher(bin_directory, binary.resolve(), workspace, home, state, provider.base_url)
        environment = os.environ.copy()
        environment["PATH"] = str(bin_directory) + os.pathsep + environment["PATH"]
        environment["TERM"] = "xterm-256color"
        environment.pop("NO_COLOR", None)
        environment.pop("AGUL_PRICE_CATALOG_URL", None)
        warm_provider(
            binary.resolve(),
            workspace,
            home,
            state,
            provider.base_url,
            environment,
        )
        provider.begin_recording()
        subprocess.run(
            ["vhs", str(ROOT / "docs" / "demo" / "agul.tape")],
            cwd=ROOT,
            env=environment,
            check=True,
        )
        verify(
            workspace,
            provider.warmup_requests,
            provider.requests,
            provider.visible_usage,
        )
    if not output.is_file() or output.stat().st_size == 0:
        raise SystemExit("VHS did not produce docs/assets/agul-demo.gif")
    print(output)


if __name__ == "__main__":
    main()
