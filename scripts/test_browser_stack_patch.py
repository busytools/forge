#!/usr/bin/env python3
"""Tests for the vendored Playwright file chooser patch. Run:
python3 scripts/test_browser_stack_patch.py

Playwright intercepts a page's file chooser for every tab, so CEF's own
dialog never fires while the driver is attached. The patch makes the driver
surface each chooser on the socket forge binds; the host then asks the
controlling person or answers with the agent's own paths, both through the
MCP tool that owns the captured chooser.

The needle is exact for the pinned @playwright/mcp bundle: a version bump
must fail the patch at install time, not ship a driver that never surfaces a
chooser. Stdlib only, like the other script tests.
"""

import re
import subprocess
import tempfile
import unittest
from pathlib import Path

PATCHER = Path(__file__).resolve().parent / "patch_browser_stack.mjs"
FIXTURE = Path(__file__).resolve().parent / "playwright-mcp-0.0.83-filechooser.txt"
TABS_FIXTURE = Path(__file__).resolve().parent / "playwright-mcp-0.0.83-tabids.txt"
TABLIST_FIXTURE = Path(__file__).resolve().parent / "playwright-mcp-0.0.83-tablist.txt"
VENDOR = Path(__file__).resolve().parent / "vendor_browser_stack.sh"
MARKER = "FORGE_FILE_CHOOSER_SOCKET"
TABS_MARKER = "FORGE_TAB_IDS_PATCH 2"
TABLIST_MARKER = "FORGE_TAB_LIST_READS 1"


def run_patcher(*args: str) -> "subprocess.CompletedProcess[str]":
    return subprocess.run(["node", str(PATCHER), *args], capture_output=True, text=True)


def needle() -> str:
    result = run_patcher("--print-needle")
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stdout.rstrip("\n")


def tabs_needle() -> str:
    result = run_patcher("--print-tabs-needle")
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stdout.rstrip("\n")


def tablist_needle() -> str:
    result = run_patcher("--print-tablist-needle")
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stdout.rstrip("\n")


class BrowserStackPatchTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)

    def bundle(self, body: str) -> Path:
        path = self.root / "coreBundle.js"
        path.write_text(body)
        return path

    def test_the_needle_is_the_pinned_versions_excerpt(self) -> None:
        pin = re.search(r'^PLAYWRIGHT_MCP_VERSION="([^"]+)"', VENDOR.read_text(), re.MULTILINE)
        self.assertIsNotNone(pin, "the vendor script names its pin")
        self.assertEqual(pin.group(1), "0.0.83", "the fixture below is that version's excerpt")
        fixture = FIXTURE.read_text().rstrip("\n")
        self.assertIn('"filechooser"', fixture)
        self.assertEqual(
            needle(),
            fixture,
            "the patcher's needle is the checked-in 0.0.83 listener, not a freehand string",
        )
        tabs_fixture = TABS_FIXTURE.read_text().rstrip("\n")
        self.assertIn("headerSnapshot", tabs_fixture)
        self.assertIn('join("\\n")', tabs_fixture, "the bundle's own escape, not a line break")
        self.assertEqual(
            tabs_needle(),
            tabs_fixture,
            "the patcher's tab needle is the checked-in 0.0.83 listing, not a freehand string",
        )
        tablist_fixture = TABLIST_FIXTURE.read_text().rstrip("\n")
        self.assertIn("ensureTab", tablist_fixture)
        self.assertEqual(
            tablist_needle(),
            tablist_fixture,
            "the patcher's list-arm needle is the checked-in 0.0.83 excerpt",
        )

    def test_the_pinned_listener_is_patched_once_and_carries_the_socket(self) -> None:
        bundle = self.bundle(
            f"// before\n{needle()}\n{tabs_needle()}\n{tablist_needle()}\n// after\n"
        )

        first = run_patcher("--bundle", str(bundle))
        self.assertEqual(first.returncode, 0, first.stderr)
        patched = bundle.read_text()
        self.assertIn(MARKER, patched)
        self.assertIn("FORGE_FILE_CHOOSER_PATCH 2", patched)
        self.assertIn('this.setModalState({', patched)
        self.assertIn('fileChooser: chooser,', patched)
        self.assertIn('chooser.isMultiple()', patched)
        self.assertEqual(patched.count('forgeNet.createConnection'), 1, "one socket write")

        second = run_patcher("--bundle", str(bundle))
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn("already applied", second.stdout)
        self.assertEqual(bundle.read_text(), patched, "a second run rewrites nothing")

    def test_a_version_1_patch_is_updated_in_place(self) -> None:
        old = run_patcher("--print-old").stdout.rstrip("\n")
        self.assertIn("FORGE_FILE_CHOOSER_SOCKET", old)
        self.assertNotIn("FORGE_FILE_CHOOSER_PATCH 2", old)
        bundle = self.bundle(f"// before\n{old}\n{tabs_needle()}\n{tablist_needle()}\n// after\n")

        updated = run_patcher("--bundle", str(bundle))
        self.assertEqual(updated.returncode, 0, updated.stderr)
        self.assertIn("updated", updated.stdout)
        patched = bundle.read_text()
        self.assertIn("FORGE_FILE_CHOOSER_PATCH 2", patched)
        self.assertIn("chooser.isMultiple(), current:", patched)

        again = run_patcher("--bundle", str(bundle))
        self.assertIn("already applied", again.stdout)

    def test_a_bundle_without_the_pinned_listener_fails_loudly(self) -> None:
        bundle = self.bundle("// a Playwright bundle this patch does not know\n")

        result = run_patcher("--bundle", str(bundle))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not carry the file chooser listener", result.stderr)
        self.assertEqual(bundle.read_text(), "// a Playwright bundle this patch does not know\n")

    def test_the_pinned_tab_listing_is_patched_once_and_carries_target_ids(self) -> None:
        bundle = self.bundle(
            f"// before\n{needle()}\n{tabs_needle()}\n{tablist_needle()}\n// after\n"
        )

        first = run_patcher("--bundle", str(bundle))
        self.assertEqual(first.returncode, 0, first.stderr)
        patched = bundle.read_text()
        self.assertEqual(patched.count(TABS_MARKER), 1, "the tab patch marker appears once")
        self.assertEqual(patched.count("FORGE-TABS "), 1, "the export line appears once")
        self.assertEqual(patched.count("_guid"), 1, "one borrowed private field")
        self.assertEqual(patched.count("Target.getTargetInfo"), 1, "the page's live target id")
        self.assertEqual(
            patched.count("context.tabs()"), 1, "the tab list is captured once, before the awaits"
        )
        self.assertEqual(patched.count("FORGE_FILE_CHOOSER_PATCH 2"), 1)
        self.assertIn("JSON.stringify(forgeRows)", patched)
        self.assertIn('"FORGE-TABS " + JSON.stringify(forgeRows) + "\\n"', patched)

        second = run_patcher("--bundle", str(bundle))
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn("already applied", second.stdout)
        self.assertEqual(bundle.read_text(), patched, "a second run rewrites nothing")

    def test_the_pinned_list_arm_lists_without_creating_a_page(self) -> None:
        bundle = self.bundle(
            f"// before\n{tablist_needle()}\n{needle()}\n{tabs_needle()}\n// after\n"
        )

        first = run_patcher("--bundle", str(bundle))
        self.assertEqual(first.returncode, 0, first.stderr)
        patched = bundle.read_text()
        self.assertEqual(patched.count(TABLIST_MARKER), 1, "the list patch marker appears once")
        self.assertNotIn(
            "await context.ensureTab();",
            patched,
            "a listing creates no page; ensureTab makes one when the driver has none",
        )
        self.assertEqual(
            patched.count("await context.ensureBrowserContext();"),
            1,
            "the listing still builds the driver's context, which is what arms the chooser",
        )

        second = run_patcher("--bundle", str(bundle))
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn("already applied", second.stdout)
        self.assertEqual(bundle.read_text(), patched, "a second run rewrites nothing")

    def test_a_bundle_without_the_pinned_tab_listing_fails_loudly(self) -> None:
        body = f"// before\n{needle()}\n// after\n"
        bundle = self.bundle(body)

        result = run_patcher("--bundle", str(bundle))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not carry the tab listing", result.stderr)
        self.assertEqual(bundle.read_text(), body, "nothing is written when a needle is missing")


if __name__ == "__main__":
    unittest.main()
