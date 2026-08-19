#!/usr/bin/env bash
# Verify that the differential harness has teeth.
#
# tests/ctp_filter_differential.rs compares ctp_filter.txt against a reference
# reading of the CTP script. A comparison test is only worth what it catches, so
# this deliberately breaks the recipe in ways a bad conversion would and checks
# that each break is detected. Every mutation below must report disagreements;
# a mutation that passes means the harness is blind to that class of defect.
#
# The recipe is restored after each mutation, and verified identical at the end.
#
# Usage: tools/mutation_check.sh
set -uo pipefail
cd "$(dirname "$0")/.."

RECIPE=ctp_filter.txt
# Kept beside the recipe rather than in a temp directory: this script rewrites a
# checked-in file, so the backup must be somewhere guaranteed writable. If the
# backup were to fail silently, every mutation would land on an already-mutated
# recipe and the results would be meaningless while still looking plausible.
BACKUP=".ctp_filter.mutation-backup"
cp "$RECIPE" "$BACKUP"
if [[ ! -s "$BACKUP" ]]; then
  echo "could not back up $RECIPE; refusing to mutate it" >&2
  exit 1
fi
trap 'cp "$BACKUP" "$RECIPE" && rm -f "$BACKUP"' EXIT

restore() {
  if ! cp "$BACKUP" "$RECIPE"; then
    echo "FATAL: could not restore $RECIPE from $BACKUP" >&2
    exit 1
  fi
}

run_test() {
  cargo test --test ctp_filter_differential converted_recipe 2>&1 \
    | grep -oE "[0-9]+ of [0-9]+ vectors disagree|test result: ok" | head -1
}

failures=0

# Apply a regex substitution to the filter sections only, never the header
# comment, then confirm the harness notices.
mutate() {
  local desc="$1" pattern="$2" replacement="$3"
  python3 - "$pattern" "$replacement" <<'PY'
import pathlib, re, sys
path = pathlib.Path("ctp_filter.txt")
text = path.read_text()
head, body = text.split("%filter allowlist", 1)
body, count = re.subn(sys.argv[1], sys.argv[2], body)
assert count > 0, f"mutation matched nothing: {sys.argv[1]}"
path.write_text(head + "%filter allowlist" + body)
PY
  if [[ $? -ne 0 ]]; then
    printf '  BROKEN   %-44s (pattern matched nothing)\n' "$desc"
    failures=$((failures + 1))
    restore
    return
  fi
  local result
  result=$(run_test)
  restore
  if [[ "$result" == *disagree* ]]; then
    printf '  caught   %-44s %s\n' "$desc" "$result"
  else
    printf '  MISSED   %-44s %s\n' "$desc" "$result"
    failures=$((failures + 1))
  fi
}

echo "baseline (unmutated recipe must agree with the reference):"
baseline=$(run_test)
if [[ "$baseline" == "test result: ok" ]]; then
  printf '  ok       %s\n\n' "$baseline"
else
  printf '  FAILED   %s\n\n' "$baseline"
  failures=$((failures + 1))
fi

echo "mutations (each must be caught):"

# Structural: the allowlist is what admits the whitelisted devices back through
# the gauntlet. Losing it should be loud.
mutate "remove the whole allowlist section" \
  '(?s)^.*?(?=\n%filter blacklist)' ''

# Predicate semantics: each of these is a plausible mis-conversion of a CTP
# method, and each differs from the correct rule only on particular values.
mutate "startsWith mapping: drop the ^ anchor" \
  '(?m)^(\s*\+?\s*)contains Manufacturer \^GE MEDICAL$' '\1contains Manufacturer GE MEDICAL'
mutate "contains mapping: narrow to equals" \
  '(?m)^(\s*\+?\s*)contains ManufacturerModelName \^Discovery$' '\1equals ManufacturerModelName Discovery'
mutate "equals mapping: widen to contains" \
  '(?m)^(\s*\+?\s*)notequals BurnedInAnnotation YES$' '\1notcontains BurnedInAnnotation YES'

# The CTP empty-value convention (r-2-6-9, r-2-6-10): blank/notblank treat an
# absent element as empty, empty/present do not.
mutate "blank -> empty (absent no longer counts)" \
  '(?m)^(\s*\+?\s*)blank ' '\1empty '
mutate "notblank -> present (empty now counts)" \
  '(?m)^(\s*\+?\s*)notblank ' '\1present '

# Granularity: a single lost device rule must still register.
mutate "delete one device label (Hitachi Noblus)" \
  '(?s)LABEL [^\n]*Noblus[^\n]*\n(?:[^\n]+\n)+?\n' ''

echo
if [[ $failures -eq 0 ]]; then
  echo "all mutations caught; the harness discriminates."
else
  echo "$failures check(s) failed: the harness is blind to something it should catch."
fi

cmp "$RECIPE" "$BACKUP" && echo "recipe restored unchanged."
exit $failures
