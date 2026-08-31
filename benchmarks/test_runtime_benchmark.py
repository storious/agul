#!/usr/bin/env python3
"""Focused tests for the runtime benchmark gate.

The gate evaluation, report assembly, and budget argument parsing are pure
Python and are tested here without starting Agul. The end-to-end tests that
start a real release binary only run when AGUL_BENCHMARK_BINARY is set, so the
fast unit tests can run anywhere.
"""
from __future__ import annotations

import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
from typing import Any
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import runtime_benchmark as benchmark


def sample_report(**overrides: object) -> dict[str, Any]:
    report: dict[str, Any] = {
        "binary": "fake/agul",
        "samples": 3,
        "first_event_ms": {"median": 100.0, "p95": 120.0, "max": 150.0},
        "total_response_ms": {"median": 200.0, "p95": 250.0, "max": 300.0},
        "peak_memory_mib": {"median": 60.0, "p95": 65.0, "max": 70.0},
        "token_usage": {
            "input_tokens": 1_000,
            "output_tokens": 20,
            "cache_hit_tokens": 750,
            "cache_miss_tokens": 250,
        },
        "kv_hit_percent": 75.0,
    }
    report.update(overrides)
    return report


class PromptPrefixTest(unittest.TestCase):
    def request(
        self,
        *,
        tools: list[dict[str, object]] | None = None,
        messages: list[dict[str, str]] | None = None,
    ) -> dict[str, Any]:
        return {
            "model": benchmark.MODEL,
            "reasoning_effort": "medium",
            "tools": tools or [
                {"type": "function", "function": {"name": "read"}},
                {"type": "function", "function": {"name": "shell"}},
            ],
            "messages": messages or [
                {"role": "system", "content": "stable " * 1_000},
                {"role": "user", "content": "inspect"},
            ],
        }

    def test_append_only_history_reports_the_exact_reusable_prefix(self) -> None:
        first = self.request()
        second = self.request(
            messages=first["messages"]
            + [
                {"role": "assistant", "content": "done"},
                {"role": "user", "content": "continue"},
            ]
        )
        usage = benchmark.prefix_usage(
            benchmark.render_prompt(first), benchmark.render_prompt(second)
        )
        self.assertEqual(
            usage["prompt_tokens"],
            usage["prompt_cache_hit_tokens"] + usage["prompt_cache_miss_tokens"],
        )
        self.assertGreater(
            usage["prompt_cache_hit_tokens"] * 100 / usage["prompt_tokens"], 95
        )

    def test_reordered_tools_or_rewritten_history_reduce_reuse(self) -> None:
        first = self.request()
        appended = self.request(
            messages=first["messages"]
            + [{"role": "assistant", "content": "done"}]
        )
        reordered = self.request(
            tools=list(reversed(first["tools"])), messages=appended["messages"]
        )
        rewritten_messages = list(appended["messages"])
        rewritten_messages[0] = {"role": "system", "content": "changed " * 1_000}
        rewritten = self.request(messages=rewritten_messages)
        previous = benchmark.render_prompt(first)
        exact_hit = benchmark.prefix_usage(previous, benchmark.render_prompt(appended))[
            "prompt_cache_hit_tokens"
        ]
        reordered_hit = benchmark.prefix_usage(previous, benchmark.render_prompt(reordered))[
            "prompt_cache_hit_tokens"
        ]
        rewritten_hit = benchmark.prefix_usage(previous, benchmark.render_prompt(rewritten))[
            "prompt_cache_hit_tokens"
        ]
        self.assertLess(reordered_hit, exact_hit)
        self.assertLess(rewritten_hit, exact_hit)

    def test_cold_request_reports_no_cached_tokens(self) -> None:
        usage = benchmark.prefix_usage(None, benchmark.render_prompt(self.request()))
        self.assertEqual(0, usage["prompt_cache_hit_tokens"])
        self.assertEqual(usage["prompt_tokens"], usage["prompt_cache_miss_tokens"])


class GateEvaluationTest(unittest.TestCase):
    def test_passes_when_all_metrics_within_budgets(self) -> None:
        gate = benchmark.evaluate_gate(
            sample_report(),
            {
                "first_event_ms": 200.0,
                "total_response_ms": 300.0,
                "peak_memory_mib": 100.0,
                "kv_hit_percent": 50.0,
            },
        )
        self.assertTrue(gate["passed"])
        self.assertEqual([], gate["violations"])
        self.assertEqual("median", gate["statistic"])

    def test_fails_with_one_violation_per_metric(self) -> None:
        gate = benchmark.evaluate_gate(
            sample_report(),
            {
                "first_event_ms": 50.0,
                "total_response_ms": 100.0,
                "peak_memory_mib": 30.0,
                "kv_hit_percent": 90.0,
            },
        )
        self.assertFalse(gate["passed"])
        self.assertEqual(
            ["first_event_ms", "kv_hit_percent", "peak_memory_mib", "total_response_ms"],
            [violation["metric"] for violation in gate["violations"]],
        )
        violation = gate["violations"][0]
        self.assertEqual("median", violation["statistic"])
        self.assertEqual(100.0, violation["observed"])
        self.assertEqual(50.0, violation["budget"])
        self.assertIn("budget is 50.0 ms", violation["message"])

    def test_gates_on_median_not_p95_or_max(self) -> None:
        report = sample_report(
            first_event_ms={"median": 100.0, "p95": 900.0, "max": 950.0},
        )
        gate = benchmark.evaluate_gate(report, {"first_event_ms": 150.0})
        self.assertTrue(gate["passed"])

    def test_unrounded_value_over_budget_fails(self) -> None:
        report = sample_report(
            first_event_ms={"median": 1.0, "p95": 1.0, "max": 1.0},
        )
        report["sample_metrics"] = [{"first_event_ms": 1.0004}]
        gate = benchmark.evaluate_gate(report, {"first_event_ms": 1.0})
        self.assertFalse(gate["passed"])
        self.assertEqual(1.0004, gate["violations"][0]["observed"])

    def test_unrounded_kv_value_below_budget_fails(self) -> None:
        gate = benchmark.evaluate_gate(
            sample_report(kv_hit_percent=74.9999),
            {"kv_hit_percent": 75.0},
        )
        self.assertFalse(gate["passed"])

    def test_metrics_exactly_at_budget_pass(self) -> None:
        gate = benchmark.evaluate_gate(
            sample_report(),
            {"first_event_ms": 100.0, "kv_hit_percent": 75.0},
        )
        self.assertTrue(gate["passed"])

    def test_kv_hit_budget_is_a_minimum(self) -> None:
        report = sample_report(kv_hit_percent=0.0)
        gate = benchmark.evaluate_gate(report, {"kv_hit_percent": 50.0})
        self.assertFalse(gate["passed"])
        violation = gate["violations"][0]
        self.assertNotIn("statistic", violation)
        self.assertIn("minimum budget is 50.0 %", violation["message"])

    def test_unknown_budget_metric_rejected(self) -> None:
        with self.assertRaises(KeyError):
            benchmark.evaluate_gate(sample_report(), {"made_up_metric": 1.0})

    def test_apply_gate_attaches_result_without_mutating_report(self) -> None:
        report = sample_report()
        budgets = {"first_event_ms": 50.0}
        gated = benchmark.apply_gate(report, budgets)
        self.assertNotIn("gate", report)
        self.assertIn("gate", gated)
        self.assertEqual(benchmark.evaluate_gate(report, budgets), gated["gate"])

    def test_apply_gate_omits_gate_without_budgets(self) -> None:
        report = sample_report()
        self.assertIs(report, benchmark.apply_gate(report, {}))
        self.assertNotIn("gate", report)


class ReportGenerationTest(unittest.TestCase):
    def test_build_report_is_independent_of_budgets(self) -> None:
        usage = sample_report()["token_usage"]
        samples = [
            {
                "first_event_ms": 100.0,
                "total_response_ms": 200.0,
                "peak_memory_mib": 60.0,
                "usage": usage,
            },
            {
                "first_event_ms": 150.0,
                "total_response_ms": 300.0,
                "peak_memory_mib": 70.0,
                "usage": usage,
            },
        ]
        report = benchmark.build_report(Path("fake/agul"), samples)
        self.assertEqual(2, report["samples"])
        self.assertEqual(125.0, report["first_event_ms"]["median"])
        self.assertEqual(75.0, report["kv_hit_percent"])
        self.assertEqual(2, report["cache_usage_contract"]["samples_validated"])
        self.assertNotIn("gate", report)

    def test_cache_usage_contract_rejects_missing_tokens(self) -> None:
        usage = {"input_tokens": 100, "output_tokens": 20}
        samples = [
            {
                "first_event_ms": 1.0,
                "total_response_ms": 2.0,
                "peak_memory_mib": 1.0,
                "usage": usage,
            }
        ]
        with self.assertRaisesRegex(
            benchmark.BenchmarkMeasurementError,
            "cache usage contract is missing",
        ):
            benchmark.build_report(Path("fake/agul"), samples)

    def test_cache_usage_contract_validates_every_sample(self) -> None:
        usage = sample_report()["token_usage"]
        bad_usage = dict(usage)
        bad_usage["cache_miss_tokens"] = 249
        samples = [
            {
                "first_event_ms": 1.0,
                "total_response_ms": 2.0,
                "peak_memory_mib": 3.0,
                "usage": usage,
            },
            {
                "first_event_ms": 1.0,
                "total_response_ms": 2.0,
                "peak_memory_mib": 3.0,
                "usage": bad_usage,
            },
        ]
        with self.assertRaisesRegex(
            benchmark.BenchmarkMeasurementError,
            "sample 2: cache usage contract expected",
        ):
            benchmark.build_report(Path("fake/agul"), samples)

    def test_metric_keeps_raw_precision(self) -> None:
        self.assertEqual(1.0004, benchmark.metric([1.0004])["median"])


class BudgetParsingTest(unittest.TestCase):
    def test_valid_budgets_are_collected(self) -> None:
        args = benchmark.build_parser().parse_args(
            [
                "--first-event-budget-ms", "100",
                "--total-response-budget-ms", "0",
                "--peak-memory-budget-mib", "64",
                "--kv-hit-budget-percent", "100",
            ]
        )
        self.assertEqual(
            {
                "first_event_ms": 100.0,
                "total_response_ms": 0.0,
                "peak_memory_mib": 64.0,
                "kv_hit_percent": 100.0,
            },
            benchmark.collect_budgets(args),
        )

    def test_no_budgets_by_default(self) -> None:
        args = benchmark.build_parser().parse_args([])
        self.assertEqual({}, benchmark.collect_budgets(args))

    def test_partial_budgets_only_collect_set_flags(self) -> None:
        args = benchmark.build_parser().parse_args(["--kv-hit-budget-percent", "50"])
        self.assertEqual({"kv_hit_percent": 50.0}, benchmark.collect_budgets(args))

    def test_invalid_budgets_rejected_with_clear_error(self) -> None:
        cases = [
            (["--first-event-budget-ms", "-1"], "--first-event-budget-ms", "finite number"),
            (["--first-event-budget-ms", "nan"], "--first-event-budget-ms", "finite number"),
            (["--first-event-budget-ms", "inf"], "--first-event-budget-ms", "finite number"),
            (["--first-event-budget-ms", "fast"], "--first-event-budget-ms", "finite number"),
            (["--total-response-budget-ms", "-0.5"], "--total-response-budget-ms", "finite number"),
            (["--peak-memory-budget-mib", "-1"], "--peak-memory-budget-mib", "finite number"),
            (["--kv-hit-budget-percent", "-1"], "--kv-hit-budget-percent", "finite number"),
            (["--kv-hit-budget-percent", "101"], "--kv-hit-budget-percent", "between 0 and 100"),
            (["--kv-hit-budget-percent", "nan"], "--kv-hit-budget-percent", "finite number"),
        ]
        parser = benchmark.build_parser()
        for argv, flag, reason in cases:
            with self.subTest(argv=argv):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    with self.assertRaises(SystemExit) as raised:
                        parser.parse_args(argv)
                self.assertEqual(2, raised.exception.code)
                self.assertIn(flag, stderr.getvalue())
                self.assertIn(reason, stderr.getvalue())

    def test_invalid_samples_rejected_before_measurement(self) -> None:
        for samples in ("0", "101"):
            with self.subTest(samples=samples):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    with self.assertRaises(SystemExit) as raised:
                        benchmark.main(["--samples", samples])
                self.assertEqual(2, raised.exception.code)
                self.assertIn("--samples", stderr.getvalue())

    def test_invalid_sample_timeout_rejected(self) -> None:
        parser = benchmark.build_parser()
        for value in ("0", "-1", "nan", "inf", "slow"):
            with self.subTest(value=value):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    with self.assertRaises(SystemExit) as raised:
                        parser.parse_args(["--sample-timeout-seconds", value])
                self.assertEqual(2, raised.exception.code)
                self.assertIn("--sample-timeout-seconds", stderr.getvalue())
                self.assertIn("finite number > 0", stderr.getvalue())

    def test_missing_binary_is_argparse_error_without_traceback(self) -> None:
        with tempfile.TemporaryDirectory(prefix="agul-missing-binary-") as temporary:
            missing = Path(temporary) / "missing-agul"
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as raised:
                    benchmark.main(["--binary", str(missing)])
        self.assertEqual(2, raised.exception.code)
        self.assertIn("--binary does not exist", stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())


class DeadlineTest(unittest.TestCase):
    class BlockingStdout:
        def __init__(self, released: threading.Event) -> None:
            self.released = released
            self.closed = False

        def readline(self, *_: object) -> str:
            self.released.wait()
            return ""

        def close(self) -> None:
            self.closed = True
            self.released.set()

    class FakeProcess:
        def __init__(self) -> None:
            self.released = threading.Event()
            self.stdin = io.StringIO()
            self.stdout = DeadlineTest.BlockingStdout(self.released)
            self.stderr = io.StringIO()
            self.pid = 123
            self.returncode: int | None = None
            self.killed = False

        def poll(self) -> int | None:
            return self.returncode

        def kill(self) -> None:
            self.killed = True
            self.returncode = -9
            self.released.set()

        def wait(self, timeout: float | None = None) -> int:
            if self.returncode is None:
                raise benchmark.subprocess.TimeoutExpired("fake-agul", timeout)
            return self.returncode

    def test_blocked_ari_read_hits_deadline_and_kills_child(self) -> None:
        process = self.FakeProcess()
        started = time.perf_counter()
        with (
            mock.patch.object(
                benchmark.subprocess, "Popen", return_value=process
            ) as popen,
            mock.patch.object(benchmark, "memory_kib", return_value=1),
        ):
            with self.assertRaises(benchmark.SampleTimeout):
                benchmark.AriProcess(
                    Path("fake-agul"),
                    Path("."),
                    "http://127.0.0.1:1/v1",
                    0.05,
                )
        self.assertLess(time.perf_counter() - started, 1.0)
        self.assertTrue(process.killed)
        self.assertTrue(process.stdin.closed)
        self.assertTrue(process.stdout.closed)
        self.assertTrue(process.stderr.closed)
        environment = popen.call_args.kwargs["env"]
        self.assertEqual(environment["HOME"], str(Path(".").resolve()))
        self.assertEqual(
            environment["LOCALAPPDATA"], str(Path(".").resolve() / ".state")
        )
        self.assertNotIn("AGUL_PRICE_CATALOG_URL", environment)

    def test_oversized_ari_line_is_bounded_and_kills_child(self) -> None:
        process = self.FakeProcess()
        process.stdout = io.StringIO("x" * (benchmark.MAX_ARI_LINE_CHARS + 1))
        with (
            mock.patch.object(benchmark.subprocess, "Popen", return_value=process),
            mock.patch.object(benchmark, "memory_kib", return_value=1),
        ):
            with self.assertRaisesRegex(
                benchmark.BenchmarkMeasurementError,
                "ARI line exceeds",
            ):
                benchmark.AriProcess(
                    Path("fake-agul"),
                    Path("."),
                    "http://127.0.0.1:1/v1",
                    1.0,
                )
        self.assertTrue(process.killed)

    def test_cleanup_closes_every_parent_pipe(self) -> None:
        process = self.FakeProcess()
        process.returncode = 0
        process.stdout = io.StringIO()
        runner = benchmark.AriProcess.__new__(benchmark.AriProcess)
        runner.closed = False
        runner.stop_memory = threading.Event()
        runner.process = process
        runner.stdin = process.stdin
        runner.stdout = process.stdout
        runner.stderr = process.stderr

        runner._cleanup(kill=False)

        self.assertTrue(process.stdin.closed)
        self.assertTrue(process.stdout.closed)
        self.assertTrue(process.stderr.closed)

    def test_timeout_writes_partial_failure_report(self) -> None:
        usage = sample_report()["token_usage"]
        completed = {
            "first_event_ms": 1.0,
            "total_response_ms": 2.0,
            "peak_memory_mib": 3.0,
            "usage": usage,
        }

        class Provider:
            base_url = "http://127.0.0.1:1/v1"

            def __enter__(self) -> object:
                return self

            def __exit__(self, *_: object) -> None:
                pass

        with tempfile.TemporaryDirectory(prefix="agul-timeout-report-") as temporary:
            report_path = Path(temporary) / "report.json"
            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                mock.patch.object(benchmark, "FakeProvider", Provider),
                mock.patch.object(
                    benchmark,
                    "run_sample",
                    side_effect=[completed, benchmark.SampleTimeout("deliberate stall")],
                ),
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                code = benchmark.main(
                    [
                        "--binary",
                        sys.executable,
                        "--samples",
                        "2",
                        "--output",
                        str(report_path),
                    ]
                )
            report = json.loads(report_path.read_text(encoding="utf-8"))
        self.assertEqual(benchmark.MEASUREMENT_FAILURE_EXIT, code)
        self.assertEqual("failed", report["status"])
        self.assertEqual(1, report["samples_completed"])
        self.assertEqual("SampleTimeout", report["error"]["type"])
        self.assertEqual(1, report["partial_measurement"]["samples"])
        self.assertEqual(30.0, report["configuration"]["sample_timeout_seconds"])
        self.assertEqual(1, len(report["sample_metrics"]))
        self.assertIn("measurement failed", stderr.getvalue())


class EndToEndGateTest(unittest.TestCase):
    """Runs the real benchmark binary; skipped unless AGUL_BENCHMARK_BINARY is set."""

    @classmethod
    def setUpClass(cls) -> None:
        binary = os.environ.get("AGUL_BENCHMARK_BINARY")
        if not binary:
            raise unittest.SkipTest("set AGUL_BENCHMARK_BINARY to run the end-to-end gate test")
        cls.binary = str(Path(binary).resolve())

    def run_main(self, report_path: Path, budgets: dict[str, str]) -> tuple[int, str]:
        argv = ["--binary", self.binary, "--samples", "2", "--output", str(report_path)]
        for flag, value in budgets.items():
            argv += [flag, value]
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = benchmark.main(argv)
        return code, stderr.getvalue()

    def test_gate_failure_preserves_json_report(self) -> None:
        with tempfile.TemporaryDirectory(prefix="agul-benchmark-test-") as temporary:
            report_path = Path(temporary) / "report.json"
            # A zero budget always fails: the fake provider sleeps 10 ms before
            # the first event, so the median first-event time is never 0.
            code, stderr = self.run_main(report_path, {"--first-event-budget-ms": "0"})
            self.assertEqual(1, code)
            self.assertIn("gate failed", stderr)
            report = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(2, report["samples"])
            self.assertFalse(report["gate"]["passed"])
            self.assertEqual(["first_event_ms"], [v["metric"] for v in report["gate"]["violations"]])

    def test_gate_passes_with_generous_budgets(self) -> None:
        with tempfile.TemporaryDirectory(prefix="agul-benchmark-test-") as temporary:
            report_path = Path(temporary) / "report.json"
            code, stderr = self.run_main(
                report_path,
                {
                    "--first-event-budget-ms": "60000",
                    "--total-response-budget-ms": "60000",
                    "--peak-memory-budget-mib": "8192",
                    "--kv-hit-budget-percent": "99",
                },
            )
            self.assertEqual(0, code)
            self.assertEqual("", stderr)
            report = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(2, report["samples"])
            self.assertTrue(report["gate"]["passed"])
            self.assertEqual(4, len(report["gate"]["budgets"]))
            self.assertEqual(2, len(report["sample_metrics"]))
            self.assertEqual(30.0, report["configuration"]["sample_timeout_seconds"])

    def test_unavailable_memory_is_measurement_failure(self) -> None:
        with tempfile.TemporaryDirectory(prefix="agul-benchmark-test-") as temporary:
            report_path = Path(temporary) / "report.json"
            with mock.patch.object(benchmark, "memory_kib", return_value=None):
                code, stderr = self.run_main(report_path, {})
            self.assertEqual(benchmark.MEASUREMENT_FAILURE_EXIT, code)
            self.assertIn("peak memory measurement unavailable", stderr)
            report = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual("failed", report["status"])
            self.assertEqual("BenchmarkMeasurementError", report["error"]["type"])
            self.assertIn("peak memory measurement unavailable", report["error"]["message"])


if __name__ == "__main__":
    unittest.main()
