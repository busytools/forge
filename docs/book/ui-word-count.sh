#!/usr/bin/env bash
# Prose word-count gate for the book's ui/ pages: words outside mockup
# blocks (term / term-bar / flex divs only, so reader-facing div prose
# cannot escape the gate) and <details> blocks, measured per page. Fails
# the run when any page exceeds 600.
#
# --strict also counts the landing page's card text (surface-grid /
# surface-card / name / blurb), so index.md is held to the same ceiling
# as prose.
#
# Div tracking keeps a depth for EVERY div and pops on close, so a
# non-mockup div's close cannot drive the counter negative and zero out
# the rest of the page. Residual hole, accepted: one stray close can pop
# one level early and un-strip until the next mockup open - fixing that
# needs real HTML parsing, which a word-count gate does not owe.
set -u
cd "$(dirname "$0")"

limit=600
strict=0
[ "${1:-}" = "--strict" ] && strict=1
fail=0
for f in src/ui/*.md; do
  words=$(awk -v strict="$strict" '
    {
      rest = $0
      line = ""
      while (1) {
        o = index(rest, "<div")
        c = index(rest, "</div>")
        if (o == 0 && c == 0) { line = line rest; break }
        if (o != 0 && (c == 0 || o < c)) {
          gt = index(substr(rest, o), ">")
          if (gt == 0) { line = line rest; break }
          tag = substr(rest, o, gt)
          d++
          if (tag ~ /class="(term|term-bar)"/ || tag ~ /display: *flex/) { mock++; kind[d] = 1 }
          else if (tag ~ /class="(surface-grid|surface-card|name|blurb)"/) { blurb++; bflag[d] = 1 }
          else bflag[d] = 0
          line = line substr(rest, 1, o - 1)
          rest = substr(rest, o + gt)
        } else {
          if (d > 0) {
            if (bflag[d]) blurb--
            else mock--
            delete bflag[d]
            d--
          }
          line = line substr(rest, 1, c - 1)
          rest = substr(rest, c + 6)
        }
      }
      if (mock == 0) $0 = line
      if (/<details>/) { det++; next }
      if (/<\/details>/) { det--; next }
      if (mock == 0 && det == 0 && (strict == 1 || blurb == 0)) {
        if ($0 ~ /class="blurb"/) next
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
