#!/usr/bin/env bash
# Prose word-count gate for the book's ui/ pages: words outside mockup
# blocks, measured per page. Fails the run when any page exceeds 600.
set -u
cd "$(dirname "$0")"

limit=600
fail=0
for f in src/ui/*.md; do
  words=$(awk '
    /<div/ { depth++; next }
    /<\/div>/ { depth--; next }
    depth == 0 { print }
  ' "$f" | wc -w)
  if [ "$words" -gt "$limit" ]; then
    printf 'OVER %s: %s (%s words)\n' "$limit" "$f" "$words"
    fail=1
  else
    printf 'ok: %s (%s words)\n' "$f" "$words"
  fi
done
exit $fail
