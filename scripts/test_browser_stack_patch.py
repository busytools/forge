#!/usr/bin/env python3
"""Tests for the vendored Playwright patches. Run:
python3 scripts/test_browser_stack_patch.py

Playwright intercepts a page's file chooser for every tab, so CEF's own
dialog never fires while the driver is attached. The patch makes the driver
surface each chooser on the socket forge binds; the host then asks the
controlling person or answers with the agent's own paths, both through the
MCP tool that owns the captured chooser.

Its tab listing is also patched twice: the target id a call selects by is
cached per page so a listing never asks a renderer anything, and the select
arm takes that target instead of a position.

Every needle is exact for the pinned @playwright/mcp bundle: a version bump
must fail the patch at install time, not ship a driver that never surfaces a
chooser or holds every call behind one paused page. Stdlib only, like the
other script tests.
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
TABSELECT_FIXTURE = Path(__file__).resolve().parent / "playwright-mcp-0.0.83-tabselect.txt"
TABSCHEMA_FIXTURE = Path(__file__).resolve().parent / "playwright-mcp-0.0.83-tabschema.txt"
VENDOR = Path(__file__).resolve().parent / "vendor_browser_stack.sh"
MARKER = "FORGE_FILE_CHOOSER_SOCKET"
TABS_MARKER = "FORGE_TAB_IDS_PATCH 3"
TABLIST_MARKER = "FORGE_TAB_LIST_READS 1"
TABSELECT_MARKER = "FORGE_SELECT_TARGET 1"
TABSCHEMA_MARKER = "FORGE_SELECT_TARGET_SCHEMA 1"


def run_patcher(*args: str) -> "subprocess.CompletedProcess[str]":
    return subprocess.run(["node", str(PATCHER), *args], capture_output=True, text=True)


def printed(flag: str) -> str:
    result = run_patcher(flag)
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stdout.rstrip("\n")


def needle() -> str:
    return printed("--print-needle")


def tabs_needle() -> str:
    return printed("--print-tabs-needle")


def tablist_needle() -> str:
    return printed("--print-tablist-needle")


def tabselect_needle() -> str:
    return printed("--print-tabselect-needle")


def tabschema_needle() -> str:
    return printed("--print-tabschema-needle")


def pinned_bundle() -> str:
    """A bundle carrying every excerpt the patch expects, in one file."""
    return (
        f"// before\n{needle()}\n{tabs_needle()}\n{tablist_needle()}\n"
        f"{tabselect_needle()}\n{tabschema_needle()}\n// after\n"
    )


class BrowserStackPatchTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)

    def bundle(self, body: str) -> Path:
        path = self.root / "coreBundle.js"
        path.write_text(body)
        return path

    def test_the_needles_are_the_pinned_versions_excerpts(self) -> None:
        pin = re.search(r'^PLAYWRIGHT_MCP_VERSION="([^"]+)"', VENDOR.read_text(), re.MULTILINE)
        self.assertIsNotNone(pin, "the vendor script names its pin")
        self.assertEqual(pin.group(1), "0.0.83", "the fixtures below are that version's excerpts")
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
        tabselect_fixture = TABSELECT_FIXTURE.read_text().rstrip("\n")
        self.assertIn("selectTab", tabselect_fixture)
        self.assertEqual(
            tabselect_needle(),
            tabselect_fixture,
            "the patcher's select-arm needle is the checked-in 0.0.83 excerpt",
        )
        tabschema_fixture = TABSCHEMA_FIXTURE.read_text().rstrip("\n")
        self.assertIn("z25.number()", tabschema_fixture)
        self.assertEqual(
            tabschema_needle(),
            tabschema_fixture,
            "the patcher's schema needle is the checked-in 0.0.83 excerpt",
        )

    def test_the_pinned_listener_is_patched_once_and_carries_the_socket(self) -> None:
        bundle = self.bundle(pinned_bundle())

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
        bundle = self.bundle(f"// before\n{old}\n{pinned_bundle()}\n// after\n")

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

    def test_the_pinned_tab_listing_is_patched_once_and_reads_no_renderer(self) -> None:
        bundle = self.bundle(pinned_bundle())

        first = run_patcher("--bundle", str(bundle))
        self.assertEqual(first.returncode, 0, first.stderr)
        patched = bundle.read_text()
        self.assertEqual(patched.count(TABS_MARKER), 1, "the tab patch marker appears once")
        self.assertEqual(patched.count("FORGE-TABS "), 1, "the export line appears once")
        self.assertEqual(
            patched.count("Target.getTargetInfo"),
            1,
            "the target id is read once per page, not once per tab per listing",
        )
        self.assertIn("forgeTab.forgeTarget = forgeTarget", patched, "the id is cached on the tab")
        self.assertIn("forgeTargetAt", patched, "a failed read is retried after a window")
        self.assertNotIn("_guid", patched, "the driver no longer exports a page guid")
        self.assertNotIn("headerSnapshot", patched, "a listing reads no title from the page")

        second = run_patcher("--bundle", str(bundle))
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn("already applied", second.stdout)
        self.assertEqual(bundle.read_text(), patched, "a second run rewrites nothing")

    def test_a_version_2_tab_listing_is_updated_in_place(self) -> None:
        # The version-2 block, as an older stack carries it.
        version_2 = (
            "        const forgeOpenTabs = context.tabs();\n"
            "        const tabHeaders = await Promise.all(forgeOpenTabs.map((tab2) => tab2.headerSnapshot()));\n"
            "        const result2 = renderTabsMarkdown(tabHeaders);\n"
            "        // FORGE_TAB_IDS_PATCH 2: the guid names the tab, the target id is\n"
            "        // where CDP drives it right now.\n"
            "        const forgeRows = [];\n"
            "        for (let i = 0; i < forgeOpenTabs.length; i++) {\n"
            "          const forgeTab = forgeOpenTabs[i];\n"
            '          let forgeTarget = "";\n'
            "          try {\n"
            "            const forgeSession = await forgeTab.page.context().newCDPSession(forgeTab.page);\n"
            '            const forgeInfo = await forgeSession.send("Target.getTargetInfo");\n'
            '            forgeTarget = (forgeInfo && forgeInfo.targetInfo && forgeInfo.targetInfo.targetId) || "";\n'
            "            await forgeSession.detach().catch(() => {});\n"
            "          } catch (forgeWhy) {\n"
            '            forgeTarget = "";\n'
            "          }\n"
            "          forgeRows.push({\n"
            "            i,\n"
            '            id: forgeTab.page._guid || "",\n'
            "            target: forgeTarget,\n"
            "            title: tabHeaders[i].title,\n"
            "            url: tabHeaders[i].url,\n"
            "            current: tabHeaders[i].current\n"
            "          });\n"
            "        }\n"
            '        response2.addTextResult("FORGE-TABS " + JSON.stringify(forgeRows) + "\\n" + result2.join("\\n"));'
        )
        bundle = self.bundle(
            f"// before\n{needle()}\n{version_2}\n{tablist_needle()}\n"
            f"{tabselect_needle()}\n{tabschema_needle()}\n// after\n"
        )

        updated = run_patcher("--bundle", str(bundle))
        self.assertEqual(updated.returncode, 0, updated.stderr)
        self.assertIn("updated", updated.stdout)
        self.assertIn("FORGE_TAB_IDS_PATCH 3", bundle.read_text())

        again = run_patcher("--bundle", str(bundle))
        self.assertIn("already applied", again.stdout)

    def test_the_select_arm_takes_a_target(self) -> None:
        bundle = self.bundle(pinned_bundle())

        first = run_patcher("--bundle", str(bundle))
        self.assertEqual(first.returncode, 0, first.stderr)
        patched = bundle.read_text()
        self.assertIn(TABSELECT_MARKER, patched)
        self.assertIn("params2.target !== void 0", patched)
        self.assertIn("findIndex((tab2) => tab2.forgeTarget === params2.target)", patched)
        self.assertIn(TABSCHEMA_MARKER, patched)
        self.assertIn(
            'target: z25.string().optional().describe("CDP target id of the tab to select.")',
            patched,
        )

        second = run_patcher("--bundle", str(bundle))
        self.assertIn("already applied", second.stdout)
        self.assertEqual(bundle.read_text(), patched, "a second run rewrites nothing")

    def test_the_pinned_list_arm_lists_without_creating_a_page(self) -> None:
        bundle = self.bundle(pinned_bundle())

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

    def test_a_bundle_without_the_pinned_list_arm_fails_loudly(self) -> None:
        body = (
            f"// before\n{needle()}\n{tabs_needle()}\n{tabselect_needle()}\n"
            f"{tabschema_needle()}\n// after\n"
        )
        bundle = self.bundle(body)

        result = run_patcher("--bundle", str(bundle))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not carry the tab list arm", result.stderr)
        self.assertNotIn(
            "[OK]",
            result.stdout,
            "a patch the file never got is not reported as applied",
        )
        self.assertEqual(bundle.read_text(), body, "nothing is written when a needle is missing")

    def test_a_bundle_without_the_pinned_select_arm_fails_loudly(self) -> None:
        body = f"// before\n{needle()}\n{tabs_needle()}\n{tablist_needle()}\n// after\n"
        bundle = self.bundle(body)

        result = run_patcher("--bundle", str(bundle))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not carry the select arm", result.stderr)
        self.assertEqual(bundle.read_text(), body, "nothing is written when a needle is missing")

    def test_a_bundle_without_the_pinned_tab_listing_fails_loudly(self) -> None:
        body = f"// before\n{needle()}\n// after\n"
        bundle = self.bundle(body)

        result = run_patcher("--bundle", str(bundle))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not carry the tab listing", result.stderr)
        self.assertEqual(bundle.read_text(), body, "nothing is written when a needle is missing")


if __name__ == "__main__":
    unittest.main()
