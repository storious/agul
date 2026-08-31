#!/usr/bin/env python3
from __future__ import annotations

import argparse
import ctypes
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import queue
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Callable, TextIO


MODEL = "agul-benchmark-model"
WARMUP_INPUT = "Read benchmark.txt once, then confirm the warmup is complete."
MEASURED_INPUT = "Return a short benchmark response."
WARMUP_RESPONSE = "warmup complete"
MEASURED_RESPONSE = "benchmark response"

DEFAULT_SAMPLE_TIMEOUT_SECONDS = 30.0
MEASUREMENT_FAILURE_EXIT = 3
MAX_ARI_LINE_CHARS = 1_048_576
MAX_ARI_QUEUED_LINES = 64
_STREAM_EOF = object()


class BenchmarkMeasurementError(RuntimeError):
    """The benchmark could not produce a trustworthy measurement."""


class SampleTimeout(BenchmarkMeasurementError):
    """One sample exceeded its absolute deadline."""


def render_prompt(request: dict[str, Any]) -> str:
    """Render only fields that contribute to the reusable model prefix."""
    prompt = {
        "model": request.get("model"),
        "reasoning_effort": request.get("reasoning_effort"),
        "tools": request.get("tools", []),
        "messages": request.get("messages", []),
    }
    return json.dumps(prompt, ensure_ascii=False, separators=(",", ":"))


def estimated_tokens(text: str) -> int:
    if not text:
        return 0
    ascii_chars = sum(character.isascii() for character in text)
    return math.ceil(ascii_chars / 4) + len(text) - ascii_chars


def common_prefix(left: str | None, right: str) -> str:
    if left is None:
        return ""
    limit = min(len(left), len(right))
    index = 0
    while index < limit and left[index] == right[index]:
        index += 1
    return right[:index]


def prefix_usage(previous: str | None, current: str) -> dict[str, Any]:
    input_tokens = estimated_tokens(current)
    cache_hit_tokens = min(input_tokens, estimated_tokens(common_prefix(previous, current)))
    cache_miss_tokens = input_tokens - cache_hit_tokens
    return {
        "prompt_tokens": input_tokens,
        "completion_tokens": 20,
        "prompt_cache_hit_tokens": cache_hit_tokens,
        "prompt_cache_miss_tokens": cache_miss_tokens,
        "completion_tokens_details": {"reasoning_tokens": 5},
    }


class FakeProvider:
    def __init__(self) -> None:
        self.previous_prompt: str | None = None
        self.lock = threading.Lock()
        provider = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                request = json.loads(self.rfile.read(length))
                if self.path != "/v1/chat/completions" or request.get("model") != MODEL:
                    self.send_error(400)
                    return
                try:
                    response_kind, usage = provider.observe(request)
                except BenchmarkMeasurementError as error:
                    self.send_error(400, str(error))
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                time.sleep(0.01)
                self.event({
                    "id": "benchmark-response",
                    "model": MODEL,
                    "choices": [{"index": 0, "delta": {"reasoning_content": "inspect"}}],
                })
                if response_kind == "tool":
                    self.event({
                        "id": "benchmark-response",
                        "model": MODEL,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "benchmark-read",
                                    "type": "function",
                                    "function": {
                                        "name": "read",
                                        "arguments": '{"path":"benchmark.txt"}',
                                    },
                                }],
                            },
                        }],
                    })
                    finish_reason = "tool_calls"
                else:
                    content = WARMUP_RESPONSE if response_kind == "warmup" else MEASURED_RESPONSE
                    self.event({
                        "id": "benchmark-response",
                        "model": MODEL,
                        "choices": [{"index": 0, "delta": {"content": content}}],
                    })
                    finish_reason = "stop"
                self.event({
                    "id": "benchmark-response",
                    "model": MODEL,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
                    "usage": usage,
                })
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                self.close_connection = True

            def event(self, value: dict[str, Any]) -> None:
                payload = json.dumps(value, separators=(",", ":")).encode()
                self.wfile.write(b"data: " + payload + b"\n\n")
                self.wfile.flush()

            def log_message(self, *_: object) -> None:
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        self.base_url = f"http://{host}:{port}/v1"

    def observe(self, request: dict[str, Any]) -> tuple[str, dict[str, Any]]:
        messages = request.get("messages")
        tools = request.get("tools")
        if not isinstance(messages, list) or not isinstance(tools, list):
            raise BenchmarkMeasurementError("benchmark request must include messages and tools")
        users = [
            message.get("content")
            for message in messages
            if isinstance(message, dict) and message.get("role") == "user"
        ]
        if not users:
            raise BenchmarkMeasurementError("benchmark request has no user message")
        last_user = users[-1]
        has_tool_result = any(
            isinstance(message, dict) and message.get("role") == "tool"
            for message in messages
        )
        if last_user == WARMUP_INPUT and not has_tool_result:
            response_kind = "tool"
            reset = True
        elif last_user == WARMUP_INPUT and has_tool_result:
            response_kind = "warmup"
            reset = False
        elif last_user == MEASURED_INPUT and has_tool_result:
            response_kind = "measured"
            reset = False
        else:
            raise BenchmarkMeasurementError("unexpected benchmark conversation shape")

        rendered = render_prompt(request)
        with self.lock:
            previous = None if reset else self.previous_prompt
            usage = prefix_usage(previous, rendered)
            self.previous_prompt = rendered
        return response_kind, usage

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def __enter__(self) -> FakeProvider:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class AriProcess:
    def __init__(
        self,
        binary: Path,
        workspace: Path,
        base_url: str,
        sample_timeout_seconds: float,
    ) -> None:
        self.sample_timeout_seconds = sample_timeout_seconds
        self.sample_started = time.perf_counter()
        self.deadline = self.sample_started + sample_timeout_seconds
        self.closed = False
        self.stderr_tail = ""
        self.stdout_queue: queue.Queue[object] = queue.Queue(
            maxsize=MAX_ARI_QUEUED_LINES
        )
        self.stop_memory = threading.Event()
        environment = os.environ.copy()
        isolated_home = workspace.resolve()
        state_root = isolated_home / ".state"
        environment["HOME"] = str(isolated_home)
        environment["USERPROFILE"] = str(isolated_home)
        environment["LOCALAPPDATA"] = str(state_root)
        environment["XDG_STATE_HOME"] = str(state_root)
        environment.pop("AGUL_PRICE_CATALOG_URL", None)
        self.process = subprocess.Popen(
            [str(binary), "ari", "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            env=environment,
        )
        if (
            self.process.stdin is None
            or self.process.stdout is None
            or self.process.stderr is None
        ):
            self._cleanup(kill=True)
            raise BenchmarkMeasurementError("could not open Agul ARI streams")
        self.stdin: TextIO = self.process.stdin
        self.stdout: TextIO = self.process.stdout
        self.stderr: TextIO = self.process.stderr
        self.next_id = 1
        self.peak_kib = [0]
        self.memory_samples = [0]
        self.memory_error: list[str] = []
        self.stdout_thread = threading.Thread(target=self.read_stdout, daemon=True)
        self.stderr_thread = threading.Thread(target=self.read_stderr, daemon=True)
        self.memory_thread = threading.Thread(target=self.sample_memory, daemon=True)
        self.stdout_thread.start()
        self.stderr_thread.start()
        self.memory_thread.start()
        try:
            self.call("ari.initialize", {"client": {"name": "benchmark", "version": "1"}})
            started, _events = self.call(
                "ari.start_session",
                {
                    "workspace": str(workspace),
                    "base_url": base_url,
                    "model": MODEL,
                    "api_key_env": "",
                    "context_window": 32_768,
                },
            )
            self.session_id = started["session_id"]
        except BaseException:
            self._cleanup(kill=True)
            raise

    def read_stdout(self) -> None:
        try:
            while line := self.stdout.readline(MAX_ARI_LINE_CHARS + 1):
                if len(line) > MAX_ARI_LINE_CHARS:
                    self.stdout_queue.put(
                        BenchmarkMeasurementError(
                            f"Agul ARI line exceeds {MAX_ARI_LINE_CHARS} characters"
                        )
                    )
                    return
                self.stdout_queue.put(line)
        except BaseException as error:
            self.stdout_queue.put(error)
        finally:
            self.stdout_queue.put(_STREAM_EOF)

    def read_stderr(self) -> None:
        try:
            while chunk := self.stderr.read(4_096):
                self.stderr_tail = (self.stderr_tail + chunk)[-65_536:]
        except (OSError, ValueError):
            pass

    def _stderr(self) -> str:
        return self.stderr_tail.strip()

    def _timeout(self, method: str) -> SampleTimeout:
        elapsed = time.perf_counter() - self.sample_started
        return SampleTimeout(
            f"sample timed out after {elapsed:.3f}s while waiting for {method} "
            f"(deadline {self.sample_timeout_seconds:g}s)"
        )

    def _next_line(self, method: str) -> str:
        remaining = self.deadline - time.perf_counter()
        if remaining <= 0:
            error = self._timeout(method)
            self._cleanup(kill=True)
            raise error
        try:
            item = self.stdout_queue.get(timeout=remaining)
        except queue.Empty:
            error = self._timeout(method)
            self._cleanup(kill=True)
            raise error from None
        if item is _STREAM_EOF:
            stderr = self._stderr()
            detail = f": {stderr}" if stderr else ""
            raise BenchmarkMeasurementError(f"Agul closed ARI before replying{detail}")
        if isinstance(item, BenchmarkMeasurementError):
            raise item
        if isinstance(item, BaseException):
            raise BenchmarkMeasurementError(f"could not read Agul ARI output: {item}")
        if not isinstance(item, str):
            raise BenchmarkMeasurementError("Agul ARI reader returned an invalid item")
        return item

    def call(
        self, method: str, params: dict[str, Any]
    ) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        request_id = str(self.next_id)
        self.next_id += 1
        request = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        self.stdin.write(json.dumps(request) + "\n")
        self.stdin.flush()
        events = []
        while True:
            line = self._next_line(method)
            message = json.loads(line)
            if message.get("method") == "ari.event":
                event = message["params"]
                event["_received_at"] = time.perf_counter()
                events.append(event)
                continue
            if message.get("id") != request_id:
                raise RuntimeError(f"unexpected ARI response: {message}")
            if "error" in message:
                raise BenchmarkMeasurementError(
                    message["error"].get("message", str(message["error"]))
                )
            return message["result"], events

    def send(self, input_text: str, expected_text: str) -> dict[str, Any]:
        started = time.perf_counter()
        result, events = self.call(
            "ari.send",
            {"session_id": self.session_id, "input": input_text},
        )
        completed = time.perf_counter()
        if not events:
            raise RuntimeError("Agul produced no ARI events")
        first_event = events[0].pop("_received_at")
        usage_events = [event for event in events if event.get("kind") == "usage"]
        if not usage_events or not usage_events[-1].get("usage"):
            raise BenchmarkMeasurementError("Agul produced no usage event")
        if result.get("text") != expected_text:
            raise BenchmarkMeasurementError(f"unexpected response: {result!r}")
        if self.memory_samples[0] == 0:
            detail = f": {self.memory_error[-1]}" if self.memory_error else ""
            raise BenchmarkMeasurementError(f"peak memory measurement unavailable{detail}")
        return {
            "first_event_ms": (first_event - started) * 1_000,
            "total_response_ms": (completed - started) * 1_000,
            "peak_memory_mib": self.peak_kib[0] / 1_024,
            "usage": usage_events[-1]["usage"],
        }

    def sample_memory(self) -> None:
        while not self.stop_memory.is_set():
            try:
                value = memory_kib(self.process.pid)
            except BaseException as error:
                self.memory_error.append(str(error))
                return
            if value is not None:
                self.peak_kib[0] = max(self.peak_kib[0], value)
                self.memory_samples[0] += 1
            self.stop_memory.wait(0.01)

    def _cleanup(self, *, kill: bool) -> None:
        if self.closed:
            return
        self.closed = True
        self.stop_memory.set()
        try:
            if kill and self.process.poll() is None:
                self.process.kill()
        except OSError:
            pass
        self._close_stream("stdin")
        if self.process.poll() is None:
            try:
                self.process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self.process.kill()
                try:
                    self.process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    pass
        cleanup_deadline = time.perf_counter() + 1
        for name in ("stdout_thread", "stderr_thread", "memory_thread"):
            thread = getattr(self, name, None)
            if thread is not None:
                thread.join(timeout=max(0, cleanup_deadline - time.perf_counter()))
        self._close_stream("stdout")
        self._close_stream("stderr")

    def _close_stream(self, name: str) -> None:
        stream = getattr(self, name, None)
        if stream is None:
            stream = getattr(self.process, name, None)
        if stream is not None:
            try:
                stream.close()
            except (OSError, ValueError):
                pass

    def close(self) -> None:
        if self.closed:
            return
        try:
            self.call("ari.close_session", {"session_id": self.session_id})
            remaining = self.deadline - time.perf_counter()
            if remaining <= 0:
                raise self._timeout("Agul shutdown")
            self.stdin.close()
            try:
                self.process.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                raise self._timeout("Agul shutdown") from None
            if self.process.returncode != 0:
                stderr = self._stderr()
                detail = f": {stderr}" if stderr else ""
                raise BenchmarkMeasurementError(
                    f"Agul exited {self.process.returncode}{detail}"
                )
        except BaseException:
            self._cleanup(kill=True)
            raise
        self._cleanup(kill=False)

    def abort(self) -> None:
        self._cleanup(kill=True)


def memory_kib(pid: int) -> int | None:
    if sys.platform.startswith("linux"):
        try:
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
        except (FileNotFoundError, ProcessLookupError):
            return None
    if sys.platform == "win32":
        return windows_memory_kib(pid)
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
    )
    return int(result.stdout.strip()) if result.returncode == 0 and result.stdout.strip() else None


def windows_memory_kib(pid: int) -> int | None:
    class Counters(ctypes.Structure):
        _fields_ = [("cb", ctypes.c_ulong), ("faults", ctypes.c_ulong)] + [
            (name, ctypes.c_size_t)
            for name in (
                "peak_working_set", "working_set", "quota_peak_paged", "quota_paged",
                "quota_peak_nonpaged", "quota_nonpaged", "pagefile", "peak_pagefile",
            )
        ]

    kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
    kernel32.OpenProcess.restype = ctypes.c_void_p
    handle = kernel32.OpenProcess(0x1010, False, pid)
    if not handle:
        return None
    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    try:
        ok = ctypes.windll.psapi.GetProcessMemoryInfo(  # type: ignore[attr-defined]
            handle, ctypes.byref(counters), counters.cb
        )
        return int(counters.peak_working_set // 1_024) if ok else None
    finally:
        kernel32.CloseHandle(ctypes.c_void_p(handle))


def run_sample(
    binary: Path,
    base_url: str,
    sample_timeout_seconds: float,
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix="agul-benchmark-") as temporary:
        workspace = Path(temporary)
        agents = workspace / ".agents"
        agents.mkdir()
        (agents / "AGENTS.md").write_text("Answer directly and briefly.\n", encoding="utf-8")
        (workspace / "benchmark.txt").write_text(
            "BEGIN" + ("x" * 180_000) + "END\n",
            encoding="utf-8",
        )
        process = AriProcess(binary, workspace, base_url, sample_timeout_seconds)
        try:
            process.send(WARMUP_INPUT, WARMUP_RESPONSE)
            sample = process.send(MEASURED_INPUT, MEASURED_RESPONSE)
        except BaseException:
            process.abort()
            raise
        else:
            process.close()
            return sample


def metric(values: list[float]) -> dict[str, float]:
    ordered = sorted(values)
    p95 = ordered[math.ceil(len(ordered) * 0.95) - 1]
    return {
        "median": statistics.median(ordered),
        "p95": p95,
        "max": ordered[-1],
    }


def displayed_metric(values: list[float]) -> dict[str, float]:
    return {name: round(value, 3) for name, value in metric(values).items()}


# The gate compares the median across samples against each budget. With the
# small sample sets this benchmark uses (CI runs five), the median is the
# least noisy summary: p95 and max on five samples are just the single worst
# sample and would make the gate flaky on shared runners.
GATE_STAT = "median"

MAX_BUDGET_METRICS = ("first_event_ms", "total_response_ms", "peak_memory_mib")
MIN_BUDGET_METRICS = ("kv_hit_percent",)
METRIC_UNITS = {
    "first_event_ms": "ms",
    "total_response_ms": "ms",
    "peak_memory_mib": "MiB",
    "kv_hit_percent": "%",
}
BUDGET_FLAGS = {
    "first_event_budget_ms": "first_event_ms",
    "total_response_budget_ms": "total_response_ms",
    "peak_memory_budget_mib": "peak_memory_mib",
    "kv_hit_budget_percent": "kv_hit_percent",
}


def budget_type(flag: str) -> Callable[[str], float]:
    """Return an argparse type that accepts only finite, non-negative budgets."""

    def parse(text: str) -> float:
        try:
            value = float(text)
        except ValueError:
            value = math.nan
        if not math.isfinite(value) or value < 0:
            raise argparse.ArgumentTypeError(
                f"{flag} must be a finite number >= 0 (got {text!r})"
            )
        return value

    return parse


def positive_finite_type(flag: str) -> Callable[[str], float]:
    """Return an argparse type that accepts only finite values above zero."""

    def parse(text: str) -> float:
        try:
            value = float(text)
        except ValueError:
            value = math.nan
        if not math.isfinite(value) or value <= 0:
            raise argparse.ArgumentTypeError(
                f"{flag} must be a finite number > 0 (got {text!r})"
            )
        return value

    return parse


def percent_budget_type(flag: str) -> Callable[[str], float]:
    """Return an argparse type that accepts budgets between 0 and 100 percent."""

    def parse(text: str) -> float:
        value = budget_type(flag)(text)
        if value > 100:
            raise argparse.ArgumentTypeError(
                f"{flag} must be between 0 and 100 (got {text!r})"
            )
        return value

    return parse


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Benchmark Agul with a fake local model.")
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--binary", type=Path, default=Path(f"target/release/agul{suffix}"))
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--output", type=Path)
    parser.add_argument(
        "--sample-timeout-seconds",
        type=positive_finite_type("--sample-timeout-seconds"),
        default=DEFAULT_SAMPLE_TIMEOUT_SECONDS,
        metavar="SECONDS",
        help="hard deadline for one complete sample, including ARI shutdown",
    )
    parser.add_argument(
        "--first-event-budget-ms",
        type=budget_type("--first-event-budget-ms"),
        default=None,
        metavar="MS",
        help="maximum median milliseconds from ari.send to the first event",
    )
    parser.add_argument(
        "--total-response-budget-ms",
        type=budget_type("--total-response-budget-ms"),
        default=None,
        metavar="MS",
        help="maximum median milliseconds from ari.send to the final result",
    )
    parser.add_argument(
        "--peak-memory-budget-mib",
        type=budget_type("--peak-memory-budget-mib"),
        default=None,
        metavar="MIB",
        help="maximum median peak resident memory in MiB",
    )
    parser.add_argument(
        "--kv-hit-budget-percent",
        type=percent_budget_type("--kv-hit-budget-percent"),
        default=None,
        metavar="PERCENT",
        help="minimum warm-turn stable-prefix reuse percentage",
    )
    return parser


def collect_budgets(args: argparse.Namespace) -> dict[str, float]:
    """Map the budget flags that were set on the command line to metric names."""
    return {
        metric_name: float(getattr(args, flag))
        for flag, metric_name in BUDGET_FLAGS.items()
        if getattr(args, flag) is not None
    }


def gate_observed_value(report: dict[str, Any], metric_name: str) -> float:
    """Use raw samples for decisions and rounded aggregates only as fallback."""
    sample_metrics = report.get("sample_metrics") or []
    if sample_metrics:
        return statistics.median(sample[metric_name] for sample in sample_metrics)
    if metric_name in MAX_BUDGET_METRICS:
        return report[metric_name][GATE_STAT]
    return report[metric_name]


def evaluate_gate(report: dict[str, Any], budgets: dict[str, float]) -> dict[str, Any]:
    """Evaluate a measurement report against budgets.

    Pure function: takes the already generated report and the parsed budgets,
    returns the gate result. Latency and memory budgets are maximums for the
    median; the KV hit budget is a minimum for the reported percentage.
    """
    violations: list[dict[str, Any]] = []
    for metric_name, budget in sorted(budgets.items()):
        if metric_name in MIN_BUDGET_METRICS:
            observed = gate_observed_value(report, metric_name)
            failed = observed < budget
            message = (
                f"{metric_name} is {observed} {METRIC_UNITS[metric_name]}, "
                f"minimum budget is {budget} {METRIC_UNITS[metric_name]}"
            )
            statistic = None
        elif metric_name in MAX_BUDGET_METRICS:
            observed = gate_observed_value(report, metric_name)
            failed = observed > budget
            message = (
                f"{GATE_STAT} {metric_name} is {observed} {METRIC_UNITS[metric_name]}, "
                f"budget is {budget} {METRIC_UNITS[metric_name]}"
            )
            statistic = GATE_STAT
        else:
            raise KeyError(f"unknown budget metric {metric_name!r}")
        if failed:
            violation: dict[str, Any] = {
                "metric": metric_name,
                "observed": observed,
                "budget": budget,
                "message": message,
            }
            if statistic is not None:
                violation["statistic"] = statistic
            violations.append(violation)
    return {
        "statistic": GATE_STAT,
        "budgets": dict(sorted(budgets.items())),
        "passed": not violations,
        "violations": violations,
    }


def cache_usage_percent(usage: dict[str, Any], sample_number: int | None = None) -> float:
    """Validate provider-reported cache tokens for the measured warm turn."""
    label = f"sample {sample_number}: " if sample_number is not None else ""
    missing = [
        name
        for name in ("cache_hit_tokens", "cache_miss_tokens")
        if name not in usage
    ]
    if missing:
        raise BenchmarkMeasurementError(
            f"{label}cache usage contract is missing {', '.join(missing)}"
        )
    hit = usage["cache_hit_tokens"]
    miss = usage["cache_miss_tokens"]
    input_tokens = usage.get("input_tokens")
    if (
        not isinstance(hit, int)
        or isinstance(hit, bool)
        or hit < 0
        or not isinstance(miss, int)
        or isinstance(miss, bool)
        or miss < 0
        or not isinstance(input_tokens, int)
        or isinstance(input_tokens, bool)
        or input_tokens <= 0
    ):
        raise BenchmarkMeasurementError(
            f"{label}cache usage contract requires non-negative integer hit/miss "
            f"tokens and positive integer input tokens"
        )
    if hit + miss != input_tokens:
        raise BenchmarkMeasurementError(
            f"{label}cache usage contract expected hit + miss to equal input tokens, "
            f"got {hit} + {miss} != {input_tokens}"
        )
    return hit * 100 / input_tokens


def build_report(binary: Path, samples: list[dict[str, Any]]) -> dict[str, Any]:
    """Assemble the measurement report without any gate evaluation."""
    if not samples:
        raise BenchmarkMeasurementError("cannot build a report without samples")
    percentages = [
        cache_usage_percent(sample["usage"], sample_number)
        for sample_number, sample in enumerate(samples, start=1)
    ]
    sample_metrics = [
        {
            "first_event_ms": sample["first_event_ms"],
            "total_response_ms": sample["total_response_ms"],
            "peak_memory_mib": sample["peak_memory_mib"],
            "kv_hit_percent": percentage,
        }
        for sample, percentage in zip(samples, percentages, strict=True)
    ]
    usage = samples[-1]["usage"]
    return {
        "binary": str(binary),
        "samples": len(samples),
        "first_event_ms": displayed_metric(
            [sample["first_event_ms"] for sample in samples]
        ),
        "total_response_ms": displayed_metric(
            [sample["total_response_ms"] for sample in samples]
        ),
        "peak_memory_mib": displayed_metric(
            [sample["peak_memory_mib"] for sample in samples]
        ),
        "token_usage": usage,
        "kv_hit_percent": round(statistics.median(percentages), 3),
        "sample_metrics": sample_metrics,
        "cache_usage_contract": {
            "source": "fake-provider exact common prompt prefix",
            "workload": "warm tool turn followed by a measured turn in one session",
            "samples_validated": len(samples),
            "passed": True,
        },
    }


def apply_gate(report: dict[str, Any], budgets: dict[str, float]) -> dict[str, Any]:
    """Attach the gate result to a copy of the report when budgets are set."""
    if not budgets:
        return report
    gated = dict(report)
    gated["gate"] = evaluate_gate(report, budgets)
    return gated


def failure_report(
    binary: Path,
    requested_samples: int,
    completed_samples: list[dict[str, Any]],
    error: Exception,
    configuration: dict[str, Any],
) -> dict[str, Any]:
    report: dict[str, Any] = {
        "binary": str(binary),
        "status": "failed",
        "samples_requested": requested_samples,
        "samples_completed": len(completed_samples),
        "configuration": configuration,
        "sample_metrics": [],
        "error": {
            "type": type(error).__name__,
            "message": str(error),
        },
    }
    if completed_samples:
        try:
            partial = build_report(binary, completed_samples)
            report["sample_metrics"] = partial["sample_metrics"]
            report["partial_measurement"] = partial
        except Exception as partial_error:
            report["partial_measurement_error"] = str(partial_error)
    return report


def emit_report(report: dict[str, Any], output: Path | None) -> None:
    rendered = json.dumps(report, indent=2) + "\n"
    if output:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if not 1 <= args.samples <= 100:
        parser.error("--samples must be between 1 and 100")
    budgets = collect_budgets(args)
    configuration = {
        "samples": args.samples,
        "sample_timeout_seconds": args.sample_timeout_seconds,
        "budgets": dict(sorted(budgets.items())),
    }
    try:
        binary = args.binary.resolve(strict=True)
    except (OSError, RuntimeError):
        parser.error(f"--binary does not exist: {args.binary}")
    if not binary.is_file():
        parser.error(f"--binary is not a file: {args.binary}")

    samples: list[dict[str, Any]] = []
    try:
        with FakeProvider() as provider:
            for sample_number in range(1, args.samples + 1):
                sample = run_sample(
                    binary,
                    provider.base_url,
                    args.sample_timeout_seconds,
                )
                cache_usage_percent(sample["usage"], sample_number)
                samples.append(sample)
        report = build_report(binary, samples)
        report["configuration"] = configuration
        report = apply_gate(report, budgets)
    except Exception as error:
        report = failure_report(
            binary,
            args.samples,
            samples,
            error,
            configuration,
        )
        emit_report(report, args.output)
        print(f"measurement failed: {error}", file=sys.stderr)
        return MEASUREMENT_FAILURE_EXIT

    # Normal gate failures retain the full completed measurement before the
    # nonzero process result is returned.
    emit_report(report, args.output)

    gate = report.get("gate")
    if gate and not gate["passed"]:
        for violation in gate["violations"]:
            print(f"gate failed: {violation['message']}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
