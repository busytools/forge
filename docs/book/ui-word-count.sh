#!/usr/bin/env bash
# Prose word-count gate for the book's client pages: words outside mockup
# blocks (term / term-bar / flex divs only, so reader-facing div prose
# cannot escape the gate) and <details> blocks, measured per page against
# `ui-word-count.baseline`, which records each page's count when the page
# is added. A page may shrink freely; growing, adding or removing one is a
# deliberate baseline edit in the same pull request. `--record` rewrites
# the baseline from the current pages.
#
# Div tracking keeps a depth for EVERY div; each frame records whether its
# open began a mockup, and a close decrements only that counter - unknown
# and classless divs are no-op frames whose text counts, so a close can
# never skew the counter negative. Residual hole, accepted: one stray close
# can pop one level early and un-strip until the next mockup open - fixing
# that needs real HTML parsing, which a word-count gate does not owe.
set -u
cd "$(dirname "$0")"

baseline=ui-word-count.baseline

count() {
  awk '
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
          else kind[d] = 0
          line = line substr(rest, 1, o - 1)
          rest = substr(rest, o + gt)
        } else {
          if (d > 0) {
            if (kind[d] == 1) mock--
            delete kind[d]
            d--
          }
          line = line substr(rest, 1, c - 1)
          rest = substr(rest, c + 6)
        }
      }
      if (mock == 0) $0 = line
      if (/<details>/) { det++; next }
      if (/<\/details>/) { det--; next }
      if (mock == 0 && det == 0) {
        gsub(/\|/, " ")
        if ($0 !~ /^[ \t-]+$/) print
      }
    }
  ' "$1" | wc -w | tr -d ' '
}

if [ "${1:-}" = "--record" ]; then
  for f in src/ui/client/*.md; do
    printf '%s %s\n' "$f" "$(count "$f")"
  done | sort > "$baseline"
  exit 0
fi

if [ ! -f "$baseline" ]; then
  echo "NO BASELINE FILE: $baseline" >&2
  exit 2
fi

fail=0
seen="$(mktemp)"
trap 'rm -f "$seen"' EXIT

for f in src/ui/client/*.md; do
  words=$(count "$f")
  printf '%s\n' "$f" >> "$seen"
  recorded=$(awk -v p="$f" '$1 == p { print $2; found = 1 } END { if (!found) exit 1 }' "$baseline")
  if [ -z "$recorded" ]; then
    printf 'NO BASELINE: %s (record it in the same PR)\n' "$f"
    fail=1
    continue
  fi
  if [ "$words" -gt "$recorded" ]; then
    printf 'OVER: %s (%s words, baseline %s)\n' "$f" "$words" "$recorded"
    fail=1
  else
    printf 'ok: %s (%s words)\n' "$f" "$words"
  fi
done

while read -r page _; do
  [ -n "$page" ] || continue
  if ! grep -Fxq "$page" "$seen"; then
    printf 'STALE BASELINE: %s (page is gone)\n' "$page"
    fail=1
  fi
done < "$baseline"

exit $fail
