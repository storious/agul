#!/usr/bin/env python3
"""Tests for the demo fixture's cache-reporting contract."""

from copy import deepcopy
import os
from pathlib import Path
import tempfile
import unittest

import render_demo


class DemoCacheContractTests(unittest.TestCase):
    def test_launcher_declares_the_visible_context_window(self) -> None:
        with tempfile.TemporaryDirectory(prefix="agul-demo-launcher-") as temporary:
            root = Path(temporary)
            launcher = render_demo.write_launcher(
                root,
                root / "agul",
                root / "workspace",
                root / "home",
                root / "state",
                "http://127.0.0.1:12345/v1",
            )

            self.assertIn("--context-window 32768", launcher.read_text(encoding="utf-8"))

    def test_exact_replay_reports_only_the_documented_five_token_miss(self) -> None:
        values = [
            render_demo.usage(round_number, exact_replay=True)
            for round_number in range(1, 5)
        ]

        self.assertEqual(sum(value["prompt_tokens"] for value in values), 7_200)
        self.assertEqual(sum(value["prompt_cache_hit_tokens"] for value in values), 7_180)
        self.assertEqual(sum(value["prompt_cache_miss_tokens"] for value in values), 20)
        self.assertAlmostEqual(7_180 / 7_200 * 100, 99.7222222222)

    def test_unwarmed_request_reports_no_cache_hit(self) -> None:
        value = render_demo.usage(1, exact_replay=False)

        self.assertEqual(value["prompt_cache_hit_tokens"], 0)
        self.assertEqual(value["prompt_cache_miss_tokens"], value["prompt_tokens"])

    def test_request_change_breaks_exact_replay(self) -> None:
        warmed = {"model": "fixture", "messages": [{"role": "user", "content": "one"}]}
        visible = deepcopy(warmed)
        visible["messages"][0]["content"] = "two"

        self.assertFalse(render_demo.is_exact_replay(warmed, visible))

    @unittest.skipUnless(
        os.environ.get("AGUL_DEMO_TEST_BINARY"),
        "set AGUL_DEMO_TEST_BINARY to exercise a real Agul replay",
    )
    def test_real_agul_turn_exactly_replays_all_four_warmed_requests(self) -> None:
        binary = Path(os.environ["AGUL_DEMO_TEST_BINARY"]).resolve()
        self.assertTrue(binary.is_file())
        with tempfile.TemporaryDirectory(prefix="agul-demo-test-") as temporary:
            workspace, home, state = render_demo.prepare_workspace(Path(temporary))
            with render_demo.DemoProvider() as provider:
                environment = os.environ.copy()
                environment.pop("AGUL_PRICE_CATALOG_URL", None)
                render_demo.warm_provider(
                    binary,
                    workspace,
                    home,
                    state,
                    provider.base_url,
                    environment,
                )
                provider.begin_recording()
                render_demo.warm_provider(
                    binary,
                    workspace,
                    home,
                    state,
                    provider.base_url,
                    environment,
                )
                render_demo.verify(
                    workspace,
                    provider.warmup_requests,
                    provider.requests,
                    provider.visible_usage,
                )


if __name__ == "__main__":
    unittest.main()
