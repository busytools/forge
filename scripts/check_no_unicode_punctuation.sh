#!/usr/bin/env bash
# Thin wrapper that invokes the Python gate. Kept as a `.sh` so
# `just check` can call it as a script; the actual scan logic lives
# in the .py file for portability (Python is on every dev mac + the
# GitHub-hosted CI runners; ripgrep is not guaranteed).
set -euo pipefail
exec python3 "$(dirname "$0")/check_no_unicode_punctuation.py" "$@"
