#!/usr/bin/env python3
"""Generate differential test vectors for the converted CTP filter script.

The converted recipe is only trustworthy if it makes the same accept/reject
decision as the CTP script it came from. This program establishes the reference
answer independently of the conversion: a file is accepted when the whitelist
matches it, or when the rejection gauntlet does not. That decision is computed
by interpreting the parsed CTP expression tree directly, with CTP's own
predicate semantics -- including its treatment of an absent element as the empty
string. The conversion's DNF output is used only to choose which headers are
worth testing, never to decide the expected answer.

Each vector is a synthetic DICOM header plus the reference decision. A Rust test
(tests/ctp_filter_differential.rs) replays them through the real evaluator on the
real generated recipe and asserts that

    allowlist matches OR blacklist does not match

agrees with the reference decision on every vector.

Vector families:
  satisfying   one header per DNF conjunction, built to satisfy that conjunction
               -- so every label in the recipe is exercised at least once
  mutated      each satisfying header with one field dropped or replaced by a
               non-matching value -- catches labels that match too broadly
  random       independent draws from each field's value pool -- catches
               interactions the first two families miss

Two things are deliberately out of scope. CTP's case-sensitive `.equals` and
`.contains` have no case-sensitive counterpart in the recipe format, so matching
there is knowingly broader (documented in ctp_filter.txt); generated values keep
the script's own casing so that divergence never fires and every mismatch the
harness reports is a real defect. And the CTP-field-to-DICOM-keyword table is
shared with the converter, so it is checked by reading and by the Rust test
asserting each mapped keyword exists, not by this harness.

Usage:
    tools/ctp_filter_diff_vectors.py ctp_stanford.script \\
        --output tests/fixtures/ctp_filter_vectors.json
"""

from __future__ import annotations

import argparse
import random
import re
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from ctp_filter_to_recipe import (  # noqa: E402
    Atom,
    Parser,
    load_dicom_keywords,
    parse_script,
    is_contradiction,
    resolve_field,
    split_top_level,
    to_dnf,
)

# Fields the Rust side must write as numeric elements; their value pools are
# restricted to digit strings so they can be.
# RegionDataType is US; it only ever appears under a blank/notblank test, so it
# never draws a literal from the script and must still get a writable value.
NUMERIC_FIELDS = {
    "Rows",
    "Columns",
    "SeriesNumber",
    "SequenceOfUltrasoundRegions::RegionDataType",
}

NO_MATCH = "ZZ-UNMATCHED-ZZ"
NO_MATCH_NUMERIC = "31337"

# Cap on near-miss variants per field, to keep the fixture a workable size.
NEAR_MISS_PER_FIELD = 3


# ---------------------------------------------------------------------------
# Reference interpreter: CTP semantics, straight off the expression tree
# ---------------------------------------------------------------------------


def ctp_value(header: dict[str, str | None], field: str) -> str:
    """CTP reads an absent element as the empty string."""
    value = header.get(field)
    return "" if value is None else value


def interpret_atom(atom: Atom, header: dict[str, str | None], keywords: set[str]) -> bool:
    field = resolve_field(atom.field, keywords)
    subject = ctp_value(header, field)
    arg = atom.value
    method = atom.method

    if method == "equals":
        return subject == arg
    if method == "equalsIgnoreCase":
        return subject.lower() == arg.lower()
    if method == "contains":
        return arg in subject
    if method == "containsIgnoreCase":
        return arg.lower() in subject.lower()
    if method == "startsWith":
        return subject.startswith(arg)
    if method == "startsWithIgnoreCase":
        return subject.lower().startswith(arg.lower())
    if method == "matches":
        return re.fullmatch(arg, subject) is not None
    raise ValueError(f"unsupported CTP method {method!r}")


def interpret(node: tuple, header: dict[str, str | None], keywords: set[str]) -> bool:
    kind = node[0]
    if kind == "atom":
        return interpret_atom(node[1], header, keywords)
    if kind == "not":
        return not interpret(node[1], header, keywords)
    if kind == "and":
        return all(interpret(child, header, keywords) for child in node[1])
    if kind == "or":
        return any(interpret(child, header, keywords) for child in node[1])
    raise ValueError(f"unknown node {kind!r}")


# ---------------------------------------------------------------------------
# Value pools
# ---------------------------------------------------------------------------


def collect_atoms(node: tuple, out: list[Atom]) -> None:
    if node[0] == "atom":
        out.append(node[1])
    elif node[0] == "not":
        collect_atoms(node[1], out)
    else:
        for child in node[1]:
            collect_atoms(child, out)


def build_pools(atoms: list[Atom], keywords: set[str]) -> dict[str, list[str | None]]:
    """Per-field candidate values.

    Everything the script tests for, plus variants that satisfy prefix and
    substring tests, plus a value that matches nothing.
    """
    by_field: dict[str, set[str]] = defaultdict(set)
    for atom in atoms:
        field = resolve_field(atom.field, keywords)
        value = atom.value
        # Register the field even when its only test is for emptiness
        # (`equals("")`), so it still reaches the random family; otherwise the
        # blank/notblank rules covering it would never be exercised.
        by_field[field]
        if not value:
            continue
        by_field[field].add(value)
        # Both variants for every literal, whichever test it came from. The
        # extended form separates a prefix match from an exact one; the
        # embedded form separates a substring match from a prefix one. Deriving
        # them only from the test that happens to use each literal leaves a
        # recipe that dropped its ^ anchors, or widened equals to contains,
        # indistinguishable from a correct one.
        by_field[field].add(value + "-SUFFIX")
        by_field[field].add("PREFIX-" + value + "-SUFFIX")
        if atom.method == "matches":
            # A digit string satisfying the only matches() rule in these scripts.
            by_field[field].add("1234")

    pools: dict[str, list[str | None]] = {}
    for field, values in by_field.items():
        if field in NUMERIC_FIELDS:
            kept = sorted(v for v in values if v.isdigit())
            kept.append(NO_MATCH_NUMERIC)
        else:
            kept = sorted(values)
            kept.append(NO_MATCH)
        # None models an absent element, "" a present-but-empty one; the
        # blank/notblank conversion turns on that difference being handled.
        pools[field] = [None, "", *kept]
    return pools


# ---------------------------------------------------------------------------
# Building a header that satisfies one DNF conjunction
# ---------------------------------------------------------------------------


def satisfies(term: tuple[Atom, bool], value: str | None, keywords: set[str]) -> bool:
    atom, negated = term
    field = resolve_field(atom.field, keywords)
    return interpret_atom(atom, {field: value}, keywords) != negated


def synthesize(terms: list[tuple[Atom, bool]]) -> str | None:
    """Build a value satisfying several positive substring tests at once.

    A conjunction can constrain one field several times -- the Toshiba CT rule
    wants an ImageType containing both "DERIVED" and "MPR" -- and no single
    literal from the script satisfies that. Joining the required fragments with
    the DICOM multi-value delimiter produces the shape a real element takes.
    """
    prefixes, fragments = [], []
    for atom, negated in terms:
        if negated:
            continue
        if atom.method.startswith("equals"):
            return None  # equality pins the value; the pool already holds it
        if atom.method.startswith("startsWith"):
            prefixes.append(atom.value)
        elif atom.method.startswith("contains"):
            fragments.append(atom.value)
    if not fragments and not prefixes:
        return None
    # Two different required prefixes can only hold if one extends the other,
    # in which case the longer one implies the shorter.
    head = max(prefixes, key=len) if prefixes else ""
    return "\\".join([head, *fragments]) if head else "\\".join(fragments)


def build_satisfying_header(
    conjunction: list[tuple[Atom, bool]],
    pools: dict[str, list[str | None]],
    keywords: set[str],
) -> dict[str, str | None] | None:
    """Pick, per field, a value satisfying every term constraining it.

    Each term constrains exactly one field, so the fields are independent and a
    per-field scan suffices; no search is required. Where no candidate from the
    pool works, one is synthesized from the positive constraints.
    """
    by_field: dict[str, list[tuple[Atom, bool]]] = defaultdict(list)
    for term in conjunction:
        by_field[resolve_field(term[0].field, keywords)].append(term)

    header: dict[str, str | None] = {}
    for field, terms in by_field.items():
        default = [None, "", NO_MATCH_NUMERIC if field in NUMERIC_FIELDS else NO_MATCH]
        pool = pools.get(field, default)
        candidates = [*pool, synthesize(terms)]
        choice = next(
            (
                value
                for value in candidates
                if all(satisfies(term, value, keywords) for term in terms)
            ),
            "\x00unsatisfiable",
        )
        if choice == "\x00unsatisfiable":
            return None  # reported by the caller
        header[field] = choice
    return header


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


def is_expression(fragment: str) -> bool:
    """Whether a split fragment holds an expression rather than only comments."""
    return bool(re.sub(r"[\x00\x01\d\s]", "", fragment))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("script", type=Path)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--random-vectors", type=int, default=4000)
    ap.add_argument("--seed", type=int, default=20260819)
    args = ap.parse_args()

    keywords = load_dicom_keywords()
    source, whitelist_text, gauntlet_text = parse_script(args.script)

    # Reference trees. Each side is a disjunction of its top-level fragments,
    # which is how the script's own `+` chain reads.
    def tree(text: str) -> tuple:
        nodes = [Parser(source, part).parse() for part in split_top_level(text) if is_expression(part)]
        return nodes[0] if len(nodes) == 1 else ("or", nodes)

    whitelist_tree = tree(whitelist_text)
    gauntlet_tree = tree(gauntlet_text)

    def reference_accept(header: dict[str, str | None]) -> bool:
        """Accept when the whitelist matches, or the gauntlet does not."""
        if interpret(whitelist_tree, header, keywords):
            return True
        return not interpret(gauntlet_tree, header, keywords)

    atoms: list[Atom] = []
    collect_atoms(whitelist_tree, atoms)
    collect_atoms(gauntlet_tree, atoms)
    pools = build_pools(atoms, keywords)
    all_fields = sorted(pools)

    vectors: list[dict] = []
    seen: set[tuple] = set()

    def add(header: dict[str, str | None], origin: str) -> None:
        key = tuple(sorted(header.items()))
        if key in seen:
            return
        seen.add(key)
        vectors.append(
            {"origin": origin, "fields": header, "accept": reference_accept(header)}
        )

    # Family 1: one satisfying header per DNF conjunction, so every emitted
    # label is exercised.
    unsatisfiable = 0
    satisfying: list[tuple[dict, list]] = []
    for side, text in (("allow", whitelist_text), ("block", gauntlet_text)):
        for part in split_top_level(text):
            if not is_expression(part):
                continue
            for conjunction in to_dnf(Parser(source, part).parse()):
                # Contradictory conjunctions are pruned by the converter and
                # have no satisfying header by construction.
                if is_contradiction(conjunction):
                    continue
                header = build_satisfying_header(conjunction, pools, keywords)
                if header is None:
                    unsatisfiable += 1
                    continue
                add(header, f"satisfying:{side}")
                satisfying.append((header, conjunction))

    # Family 2: one-field mutations of every satisfying header.
    for header, _ in satisfying:
        for field in list(header):
            miss = NO_MATCH_NUMERIC if field in NUMERIC_FIELDS else NO_MATCH
            # "" is what separates blank from empty and notblank from present.
            for replacement in (None, "", miss):
                mutated = dict(header)
                mutated[field] = replacement
                add(mutated, "mutated")

    # Family 2b: near misses, driven by the literals each conjunction actually
    # constrains rather than by the value that happened to satisfy it. For every
    # such literal all three forms are tried: the literal itself, the literal
    # extended, and the literal embedded. Those are what separate an exact match
    # from a prefix match from a substring match, and so what would expose a
    # mis-converted equals, startsWith or contains.
    #
    # Driving this from the literals matters in two ways that selecting by value
    # does not cover: a field satisfied by being *absent* still gets probed, and
    # the embedded form is always included rather than being cut by a cap that
    # sorts it last.
    for header, conjunction in satisfying:
        constrained: dict[str, set[str]] = defaultdict(set)
        for atom, _ in conjunction:
            if atom.value:
                constrained[resolve_field(atom.field, keywords)].add(atom.value)

        for field, literals in constrained.items():
            if field in NUMERIC_FIELDS:
                candidates = [*literals, NO_MATCH_NUMERIC]
            else:
                candidates = [
                    form
                    for literal in sorted(literals)
                    for form in (literal, f"{literal}-SUFFIX", f"PREFIX-{literal}-SUFFIX")
                ]
            for candidate in dict.fromkeys(candidates):
                if candidate == header.get(field):
                    continue
                near = dict(header)
                near[field] = candidate
                add(near, "near-miss")

    # Family 3: independent random draws across every field the script reads.
    rng = random.Random(args.seed)
    for _ in range(args.random_vectors):
        header = {}
        for field in all_fields:
            # Keep headers sparse, the way real files are.
            if rng.random() < 0.45:
                continue
            header[field] = rng.choice(pools[field])
        add(header, "random")

    # Tab-separated, so the Rust side can read it with std alone: a
    # de-identification tool should not grow a JSON dependency to run its tests.
    #   origin <TAB> accept <TAB> Field=Value <TAB> ...
    # An absent element is written by omitting the field; a present but empty
    # one is written as `Field=`.
    lines = [f"# generated from {args.script.name} by {Path(__file__).name}"]
    for vector in vectors:
        cells = [vector["origin"], "accept" if vector["accept"] else "reject"]
        for field, value in sorted(vector["fields"].items()):
            if value is None:
                continue
            if "\t" in value or "\n" in value or "=" in field:
                raise ValueError(f"value for {field} is not TSV-safe: {value!r}")
            cells.append(f"{field}={value}")
        lines.append("\t".join(cells))

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("\n".join(lines) + "\n")

    accepted = sum(1 for v in vectors if v["accept"])
    by_origin: dict[str, int] = defaultdict(int)
    for v in vectors:
        by_origin[v["origin"]] += 1

    print(f"wrote {args.output} with {len(vectors)} vectors")
    for origin, count in sorted(by_origin.items()):
        print(f"  {count:6d}  {origin}")
    print(f"  reference accepts {accepted}, rejects {len(vectors) - accepted}")
    if unsatisfiable:
        print(f"  note: {unsatisfiable} conjunction(s) had no satisfying header", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
