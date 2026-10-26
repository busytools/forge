#!/usr/bin/env python3
"""Tests that the browser host can never take focus. Run:
python3 scripts/test_browser_host_focus.py

A host is an engine, not an app: the staged main bundle is an accessory
(`LSUIElement`), CEF creates the browser windowless, and the process claims
the accessory activation policy before CEF starts. The policy already has a
runtime guard - `crates/forge-browser/tests/app_ivars.rs` calls `setup()` and
asserts the policy on macOS through the `testing` feature - but the bundle
flag lives in a shell script and the windowless flag is one CEF setting, so
each gets a pin here: a bundling or startup change that drops one fails the
gate rather than shipping a host that takes the front.

**The assertions read code, never comments.** A commented-out line is
asserted to fail below, because a pin that prose satisfies is not a pin.

Stdlib only, like the other script tests.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGE = ROOT / "scripts" / "stage-browser-host.sh"
HOST = ROOT / "crates" / "forge-browser" / "src" / "host.rs"
APPLICATION = ROOT / "crates" / "forge-browser" / "src" / "client_application.rs"


def function_body(source: str, name: str) -> str:
    """The text from `name() {` to the closing brace at column 0."""
    start = source.index(f"{name}() {{")
    end = source.index("\n}", start)
    return source[start:end]


def code_lines(source: str, marker: str) -> list[str]:
    """The lines of `source` that are not comments for `marker`."""
    return [line for line in source.splitlines() if not line.lstrip().startswith(marker)]


def has_accessory_plist(script: str) -> bool:
    """The plist template carries `LSUIElement` 1.

    The template is shell-quoted pairs, so the key and its value are separate
    literals on one line: the quotes are part of the match.
    """
    plist = "\n".join(code_lines(function_body(script, "write_plist"), "#"))
    return re.search(r"<key>LSUIElement</key>['\s]*<string>1</string>", plist) is not None


def stages_the_main_bundle(script: str) -> bool:
    """The plist template is applied to the main bundle, not only to helpers."""
    return 'write_plist "$MAIN" "$APP"' in "\n".join(code_lines(script, "#"))


def has_windowless(host: str) -> bool:
    return any("windowless_rendering_enabled: 1" in line for line in code_lines(host, "//"))


def claims_accessory(application: str) -> bool:
    return any(
        "setActivationPolicy(NSApplicationActivationPolicy::Accessory)" in line
        for line in code_lines(application, "//")
    )


def commented_out(source: str, needle: str, marker: str) -> str:
    """`source` with the line carrying `needle` prefixed by `marker`."""
    for line in source.splitlines(keepends=True):
        if needle in line:
            return source.replace(line, f"{marker}{line}", 1)
    raise AssertionError(f"no line carrying {needle!r} to comment out")


class BrowserHostFocusTest(unittest.TestCase):
    def test_the_staged_main_bundle_is_an_accessory(self):
        script = STAGE.read_text()
        self.assertTrue(
            has_accessory_plist(script),
            "the staged bundle lost LSUIElement 1, so macOS registers the host "
            "as an app of its own with a Dock tile",
        )
        self.assertTrue(
            stages_the_main_bundle(script),
            "the staging script stopped writing the main bundle's plist",
        )

    def test_the_browser_is_created_windowless(self):
        self.assertTrue(
            has_windowless(HOST.read_text()),
            "the host no longer creates CEF's browser windowless, so it has a "
            "window to bring to the front",
        )

    def test_the_process_claims_the_accessory_policy(self):
        self.assertTrue(
            claims_accessory(APPLICATION.read_text()),
            "the host no longer claims the accessory activation policy, so "
            "macOS may activate it like a regular app",
        )

    def test_a_commented_out_line_is_not_a_pin(self):
        script = STAGE.read_text()
        self.assertFalse(
            has_accessory_plist(commented_out(script, "LSUIElement", "#")),
            "a commented-out LSUIElement line satisfied the plist pin",
        )
        self.assertFalse(
            stages_the_main_bundle(commented_out(script, 'write_plist "$MAIN" "$APP"', "#")),
            "a commented-out write_plist call satisfied the staging pin",
        )
        self.assertFalse(
            has_windowless(commented_out(HOST.read_text(), "windowless_rendering_enabled", "//")),
            "a commented-out windowless line satisfied the windowless pin",
        )
        self.assertFalse(
            claims_accessory(
                commented_out(
                    APPLICATION.read_text(),
                    "setActivationPolicy(NSApplicationActivationPolicy::Accessory)",
                    "//",
                )
            ),
            "a commented-out activation policy line satisfied the policy pin",
        )


if __name__ == "__main__":
    unittest.main()
