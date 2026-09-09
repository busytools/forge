#!/usr/bin/env bash
# Prose word-count gate for the book's ui/ pages: words outside mockup
# blocks (term / term-bar / surface-grid / flex divs only, so reader-
# facing div prose cannot escape the gate) and <details> blocks, measured
# per page. Fails the run when any page exceeds 600.
#
# --strict also counts surface-grid blurbs, so the landing page's card
# text is held to the same ceiling as prose.
set -u
cd "$(dirname "$0")"

limit=600
strict=0
[ "${1:-}" = "--strict" ] && strict=1
fail=0
for f in src/ui/*.md; do
  words=$(awk -v strict="$strict" '
    {
      line = $0
      stripped = 0
      while (match(line, /<div[^>]*>/)) {
        tag = substr(line, RSTART, RLENGTH)
        if (tag ~ /class="(term|term-bar|surface-grid)"/ || tag ~ /display: *flex/) {
          divs++
          line = substr(line, RSTART + RLENGTH)
          stripped = 1
        } else break
      }
      line = (stripped ? line : $0)
      while (match(line, /<\/div>/)) { divs--; line = substr(line, RSTART + RLENGTH) }
      if (divs == 0) $0 = line
      if (/<details>/) { det++; next }
      if (/<\/details>/) { det--; next }
      if (divs == 0 && det == 0) {
        if (strict == 0 && $0 ~ /class="blurb"/) next
        gsub(/\|/, " ")
        if ($0 !~ /^[ \t-]+$/) print
      }
    }
  ' "$f" | wc -w)
  if [ "$words" -gt "$limit" ]; then
    printf 'OVER %s: %s (%s words)\n' "$limit" "$f" "$words"
    fail=1
  else
    printf 'ok: %s (%s words)\n' "$f" "$words"
  fi
done
exit $fail
