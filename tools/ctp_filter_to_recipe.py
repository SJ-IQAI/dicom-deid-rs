#!/usr/bin/env python3
"""Convert a CTP DicomFilter script into recipe filter sections.

CTP's DicomFilter stage evaluates one boolean expression per object: true means
the object continues down the pipeline, false means it is quarantined. The
institutional scripts are all shaped

    ( device whitelist ) + !( rejection gauntlet )

i.e. accept = W OR NOT(G). This program turns that into the recipe equivalent,
reject = G AND NOT(W), which is expressed as

    %filter allowlist   <- W, exempts a file from every blacklist rule (r-5-2)
    %filter blacklist   <- G

The recipe condition format has no grouping and no negation of groups: a label's
conditions are a flat left-to-right fold. So each side is converted to
disjunctive normal form, pushing negations down to the atoms via De Morgan, and
one LABEL is emitted per conjunction. Since a file need match only one label,
the disjunction is preserved exactly.

Usage:
    tools/ctp_filter_to_recipe.py ctp_stanford.script \\
        --graylist-from ctp_pixel.txt --output ctp_filter.txt
    tools/ctp_filter_to_recipe.py ctp_stanford.script --print-stats
"""

from __future__ import annotations

import argparse
import itertools
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# ---------------------------------------------------------------------------
# CTP field names -> DICOM keywords
# ---------------------------------------------------------------------------

# Filter fields resolve by DICOM keyword only, and an unresolvable keyword never
# matches -- which would silently disable every rule that uses it. So every tag
# number and every CTP-ism is mapped here explicitly, and anything left
# unrecognised is a hard error rather than a silent pass.
TAG_TO_KEYWORD = {
    "[0008,0016]": "SOPClassUID",
    "[0008,1090]": "ManufacturerModelName",
    "[0018,1018]": "SecondaryCaptureDeviceManufacturerModelName",
    "[0018,1020]": "SoftwareVersions",
    # CTP's own typo: element 1018 of group 0018 written with the group repeated.
    # ctp_pixel.txt treats it the same way.
    "[1018,1018]": "SecondaryCaptureDeviceManufacturerModelName",
}

CTP_ALIAS_TO_KEYWORD = {
    # CTP's label, not the DICOM keyword (which is plural).
    "SoftwareVersion": "SoftwareVersions",
    # CTP abbreviates the sequence name.
    "SeqOfUltrasoundRegions": "SequenceOfUltrasoundRegions",
}


def load_dicom_keywords() -> set[str]:
    """Every keyword in the DICOM dictionary the Rust side resolves against.

    Read out of the vendored dicom-dictionary-std source so validation uses the
    same list the program itself will.
    """
    roots = sorted(Path.home().glob(".cargo/registry/src/*/dicom-dictionary-std-*/src"))
    if not roots:
        return set()
    keywords: set[str] = set()
    for name in ("tags.rs", "uids.rs"):
        path = roots[-1] / name
        if path.exists():
            keywords.update(re.findall(r'alias:\s*"([A-Za-z0-9_]+)"', path.read_text()))
    return keywords


# ---------------------------------------------------------------------------
# Lexing
# ---------------------------------------------------------------------------

LIT = "\x00"  # string-literal placeholder delimiter
COM = "\x01"  # comment placeholder delimiter

ATOM_RE = re.compile(
    r"""
    (?P<field>
        \[[0-9A-Fa-f]{4},[0-9A-Fa-f]{4}\]
      | [A-Za-z_][A-Za-z0-9_]* (?: :: [A-Za-z_][A-Za-z0-9_]* )*
    )
    \s* \. \s*
    (?P<method> [A-Za-z]+ )
    \s* \( \s* (?P<arg> \x00\d+\x00 )? \s* \)
    """,
    re.VERBOSE,
)


@dataclass
class Source:
    """A CTP script with string literals and comments lifted out."""

    text: str
    literals: list[str]
    comments: list[str]

    def literal(self, placeholder: str) -> str:
        return self.literals[int(placeholder.strip(LIT))]

    def comment(self, placeholder: str) -> str:
        """A comment's text, with any literal placeholders restored.

        Literals are lifted before comments, so a comment that quotes a value
        ('as some manufacturers use "MG/PR"') has a placeholder embedded in it.
        Those must come back before the text is used in a label name, or the
        recipe ends up carrying NUL bytes.
        """
        text = self.comments[int(placeholder.strip(COM))]
        return re.sub(
            rf"{LIT}(\d+){LIT}",
            lambda m: f'"{self.literals[int(m.group(1))]}"',
            text,
        )


def lift(raw: str) -> Source:
    """Replace string literals and // comments with placeholders.

    Both have to come out before any paren matching: comments in these scripts
    contain unbalanced parentheses ("Segami Mirage (WB MAC P600)"), and literals
    contain backslashes and other metacharacters.
    """
    literals: list[str] = []
    comments: list[str] = []

    def take_literal(match: re.Match[str]) -> str:
        literals.append(match.group(0)[1:-1])
        return f"{LIT}{len(literals) - 1}{LIT}"

    def take_comment(match: re.Match[str]) -> str:
        comments.append(match.group(0)[2:].strip())
        return f"{COM}{len(comments) - 1}{COM}"

    text = re.sub(r'"[^"]*"', take_literal, raw)
    text = re.sub(r"//[^\n]*", take_comment, text)
    return Source(text=text, literals=literals, comments=comments)


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Atom:
    field: str
    method: str
    value: str


# Expression nodes: ("atom", Atom) | ("not", node) | ("and"|"or", [nodes])
Node = tuple


class Parser:
    """Recursive-descent parser for CTP filter expressions.

    Precedence, loosest first: `+` (OR), `*` (AND), `!` (NOT).
    """

    def __init__(self, source: Source, text: str) -> None:
        self.source = source
        self.tokens = self._tokenize(text)
        self.pos = 0

    def _tokenize(self, text: str) -> list[str]:
        tokens: list[str] = []
        i = 0
        while i < len(text):
            ch = text[i]
            if ch.isspace():
                i += 1
                continue
            if ch == COM:  # comments carry no meaning for evaluation
                end = text.index(COM, i + 1)
                i = end + 1
                continue
            if ch in "*+!()":
                tokens.append(ch)
                i += 1
                continue
            match = ATOM_RE.match(text, i)
            if not match:
                raise ValueError(f"cannot lex at offset {i}: {text[i : i + 60]!r}")
            tokens.append(match.group(0))
            i = match.end()
        return tokens

    def peek(self) -> str | None:
        return self.tokens[self.pos] if self.pos < len(self.tokens) else None

    def parse(self) -> Node:
        node = self.or_expr()
        if self.pos != len(self.tokens):
            raise ValueError(f"trailing tokens at {self.pos}: {self.tokens[self.pos : self.pos + 5]}")
        return node

    def or_expr(self) -> Node:
        parts = [self.and_expr()]
        while self.peek() == "+":
            self.pos += 1
            parts.append(self.and_expr())
        return parts[0] if len(parts) == 1 else ("or", parts)

    def and_expr(self) -> Node:
        parts = [self.unary()]
        while self.peek() == "*":
            self.pos += 1
            parts.append(self.unary())
        return parts[0] if len(parts) == 1 else ("and", parts)

    def unary(self) -> Node:
        token = self.peek()
        if token == "!":
            self.pos += 1
            return ("not", self.unary())
        if token == "(":
            self.pos += 1
            node = self.or_expr()
            if self.peek() != ")":
                raise ValueError(f"expected ) at {self.pos}")
            self.pos += 1
            return node
        if token is None:
            raise ValueError("unexpected end of expression")
        self.pos += 1
        match = ATOM_RE.fullmatch(token)
        if not match:
            raise ValueError(f"not an atom: {token!r}")
        arg = match.group("arg")
        return (
            "atom",
            Atom(
                field=match.group("field"),
                method=match.group("method"),
                value=self.source.literal(arg) if arg else "",
            ),
        )


# ---------------------------------------------------------------------------
# Top-level splitting, for group names
# ---------------------------------------------------------------------------


def split_top_level(text: str, separator: str = "+") -> list[str]:
    """Split on `separator` at paren depth 0. Literals/comments must be lifted."""
    parts: list[str] = []
    depth = 0
    start = 0
    for i, ch in enumerate(text):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == separator and depth == 0:
            parts.append(text[start:i])
            start = i + 1
    parts.append(text[start:])
    return parts


NOISE_COMMENT = re.compile(r"^[/\s]*$")


def comment_texts(source: Source, fragment: str) -> list[str]:
    """Meaningful comment strings in a fragment, in order."""
    found = []
    for placeholder in re.findall(rf"{COM}\d+{COM}", fragment):
        text = source.comment(placeholder)
        if text and not NOISE_COMMENT.match(text):
            found.append(text)
    return found


def leading_comments(source: Source, fragment: str) -> list[str]:
    """Comments before the fragment's expression begins."""
    cut = fragment.find("(")
    return comment_texts(source, fragment if cut < 0 else fragment[:cut])


def trailing_comments(source: Source, fragment: str) -> list[str]:
    """Comments after the fragment's expression ends.

    These belong to the *next* disjunct: the naming comment sits above the `+`
    that introduces its group, so splitting on `+` leaves it at the tail of the
    preceding fragment.
    """
    cut = fragment.rfind(")")
    return comment_texts(source, fragment if cut < 0 else fragment[cut + 1 :])


def name_groups(source: Source, parts: list[str]) -> list[str]:
    """Derive a group name per top-level disjunct.

    One comment often heads a run of disjuncts -- the gauntlet's "Unsupported
    modalities" comment covers five of them -- so a part with no comment of its
    own inherits the last one seen rather than becoming an anonymous "group N".
    """
    names: list[str] = []
    current = ""
    for i, part in enumerate(parts):
        candidates = trailing_comments(source, parts[i - 1]) if i > 0 else []
        if not candidates:
            candidates = leading_comments(source, part)
        if candidates:
            current = clean_name(candidates[-1])
        names.append(current or f"group {i + 1}")
    return names


def clean_name(text: str, limit: int = 70) -> str:
    # `#` starts a comment in the recipe format, so it can never appear in a
    # LABEL name; nor can the placeholder control characters.
    text = text.replace("#", "no.").replace(LIT, "").replace(COM, "")
    text = re.sub(r"\s*--\s*", " - ", text)
    text = re.sub(r"\s+", " ", text).strip(" -/")
    if len(text) > limit:
        text = text[:limit].rstrip(" ,-/(")
    return text


# ---------------------------------------------------------------------------
# DNF
# ---------------------------------------------------------------------------

Term = tuple[Atom, bool]  # (atom, negated)


def is_contradiction(conjunction: list[Term]) -> bool:
    """Whether a conjunction is false for every possible input.

    Expanding a negated group produces these in bulk: the mammo rule's
    `!(SECONDARY AND !(Hologic OR ...))` crosses every listed manufacturer with
    a requirement to *be* Hologic, and the GE ultrasound rule crosses a SOP
    class whitelist with a requirement not to be that SOP class. Such a
    conjunction can never fire, so dropping it cannot change the disjunction --
    but leaving it in would ship labels that are dead on arrival.

    Only provable contradictions are reported, and comparisons are made
    case-insensitively, which is the conservative direction: two values that
    conflict even ignoring case conflict under CTP's case-sensitive tests too.
    """
    by_field: dict[str, list[tuple[str, str, bool]]] = {}
    for atom, negated in conjunction:
        by_field.setdefault(atom.field, []).append((atom.method, atom.value, negated))

    for terms in by_field.values():
        unique = set(terms)
        # The same test both asserted and negated.
        if any((method, value, not negated) in unique for method, value, negated in unique):
            return True

        equals = {v.lower() for m, v, n in unique if not n and m.startswith("equals")}
        prefixes = {v.lower() for m, v, n in unique if not n and m.startswith("startsWith")}
        substrings = {v.lower() for m, v, n in unique if not n and m.startswith("contains")}

        # A field cannot equal two different values.
        if len(equals) > 1:
            return True
        # Nor equal one value while starting with, or containing, something
        # that value does not.
        if equals:
            fixed = next(iter(equals))
            if any(not fixed.startswith(p) for p in prefixes):
                return True
            if any(s not in fixed for s in substrings):
                return True
        # Nor start with two prefixes where neither extends the other.
        if any(
            not a.startswith(b) and not b.startswith(a) for a in prefixes for b in prefixes
        ):
            return True

    return False


def to_dnf(node: Node, negated: bool = False) -> list[list[Term]]:
    """Disjunctive normal form: a list of conjunctions of possibly-negated atoms.

    Negation is pushed down to the atoms as it goes (De Morgan), so `not` never
    survives into the output -- which is what lets each conjunction become a
    flat label.
    """
    kind = node[0]
    if kind == "atom":
        return [[(node[1], negated)]]
    if kind == "not":
        return to_dnf(node[1], not negated)
    children = node[1]
    # OR under no negation, or AND under negation, is a union of conjunctions.
    if (kind == "or") != negated:
        return [conj for child in children for conj in to_dnf(child, negated)]
    # AND under no negation, or OR under negation, is a cross product.
    product: list[list[Term]] = [[]]
    for child in children:
        child_dnf = to_dnf(child, negated)
        product = [left + right for left in product for right in child_dnf]
    return product


# ---------------------------------------------------------------------------
# Emitting recipe conditions
# ---------------------------------------------------------------------------


class ConversionError(Exception):
    pass


# Only the metacharacters Rust's regex crate recognises. Deliberately not
# re.escape, which also escapes spaces and `#`: Rust's regex rejects some
# unrecognised escapes, and a pattern that fails to compile falls back to a
# literal substring search for the pattern text itself, which silently never
# matches.
REGEX_META = set(r"\.+*?()|[]{}^$")


def regex_escape(value: str) -> str:
    return "".join(f"\\{ch}" if ch in REGEX_META else ch for ch in value)


def resolve_field(field: str, keywords: set[str]) -> str:
    if field in TAG_TO_KEYWORD:
        return TAG_TO_KEYWORD[field]
    if field.startswith("["):
        raise ConversionError(f"unmapped tag number {field}; add it to TAG_TO_KEYWORD")
    parts = [CTP_ALIAS_TO_KEYWORD.get(part, part) for part in field.split("::")]
    if keywords:
        for part in parts:
            if part not in keywords:
                raise ConversionError(
                    f"{part!r} is not a DICOM keyword (from field {field!r}); "
                    "an unresolvable field never matches, so this must be mapped explicitly"
                )
    return "::".join(parts)


def condition(term: Term, keywords: set[str]) -> str:
    """Render one possibly-negated CTP atom as a recipe condition."""
    atom, negated = term
    field = resolve_field(atom.field, keywords)
    method = atom.method
    value = atom.value

    # CTP's equals("") cannot distinguish absent from present-but-empty, which
    # is what blank/notblank model (r-2-6-9, r-2-6-10).
    if method in ("equals", "equalsIgnoreCase") and value == "":
        return f"{'notblank' if negated else 'blank'} {field}"

    if method in ("equals", "equalsIgnoreCase"):
        return f"{'notequals' if negated else 'equals'} {field} {value}"

    if method in ("contains", "containsIgnoreCase"):
        return f"{'notcontains' if negated else 'contains'} {field} {regex_escape(value)}"

    if method in ("startsWith", "startsWithIgnoreCase"):
        return f"{'notcontains' if negated else 'contains'} {field} ^{regex_escape(value)}"

    if method == "matches":
        # CTP's matches() is a full-string regex; contains is unanchored.
        return f"{'notcontains' if negated else 'contains'} {field} ^{value}$"

    raise ConversionError(f"unsupported CTP method {method!r} on {atom.field}")


SIGNATURE_FIELDS = (
    "Manufacturer",
    "ManufacturerModelName",
    "SecondaryCaptureDeviceManufacturerModelName",
    "SoftwareVersions",
    "Modality",
)


def label_name(group: str, conjunction: list[Term], keywords: set[str], ordinal: int) -> str:
    """A name that identifies which device/branch a label came from.

    Label names surface in the blacklist report and in golden tests, so they
    carry the device signature rather than just an index.
    """
    values: dict[str, str] = {}
    for atom, negated in conjunction:
        if negated or not atom.value:
            continue
        try:
            field = resolve_field(atom.field, keywords)
        except ConversionError:
            continue
        values.setdefault(field, atom.value)

    bits = [values[field] for field in SIGNATURE_FIELDS if field in values]
    if "Rows" in values and "Columns" in values:
        bits.append(f"{values['Rows']}x{values['Columns']}")

    # Cap the parts separately so the ordinal always survives: it is the only
    # thing distinguishing two conjunctions with an identical signature.
    signature = clean_name(" ".join(bits), limit=70)
    head = f"{group} - {signature}" if signature else group
    return f"{head} [{ordinal}]"


def emit_labels(
    source: Source,
    expression_text: str,
    keywords: set[str],
) -> list[tuple[str, list[str]]]:
    """Convert one side of the script into (label name, conditions) pairs."""
    parts = split_top_level(expression_text)
    names = name_groups(source, parts)

    labels: list[tuple[str, list[str]]] = []
    for name, part in zip(names, parts):
        stripped = re.sub(rf"{COM}\d+{COM}", " ", part).strip()
        if not stripped:
            continue
        node = Parser(source, part).parse()
        live = [c for c in to_dnf(node) if not is_contradiction(c)]
        for ordinal, conjunction in enumerate(live, start=1):
            conditions = [condition(term, keywords) for term in conjunction]
            # Order-insensitive dedup guard: a cross product can restate the
            # same atom twice (e.g. a field constrained in two nested groups).
            seen: set[str] = set()
            unique = [c for c in conditions if not (c in seen or seen.add(c))]
            labels.append((label_name(name, conjunction, keywords, ordinal), unique))
    return labels


def render_section(filter_type: str, labels: list[tuple[str, list[str]]]) -> str:
    out = [f"%filter {filter_type}", ""]
    for name, conditions in labels:
        out.append(f"LABEL {name}")
        out.append(conditions[0])
        out.extend(f"  + {c}" for c in conditions[1:])
        out.append("")
    return "\n".join(out)


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


def parse_script(path: Path) -> tuple[Source, str, str]:
    """Split a CTP filter script into its whitelist and gauntlet expressions."""
    source = lift(path.read_text())
    parts = split_top_level(source.text)
    if len(parts) != 2:
        raise ConversionError(
            f"expected the canonical `( whitelist ) + !( gauntlet )` shape, "
            f"found {len(parts)} top-level disjuncts"
        )
    whitelist, negated_gauntlet = parts

    # Unwrap `( ... )` from the whitelist and `!( ... )` from the gauntlet so
    # each side can be split into its own top-level disjuncts for naming.
    whitelist_body = unwrap(whitelist, expect_not=False)
    gauntlet_body = unwrap(negated_gauntlet, expect_not=True)
    return source, whitelist_body, gauntlet_body


def unwrap(fragment: str, expect_not: bool) -> str:
    """Return the contents of the outermost paren pair, dropping a leading `!`.

    Text outside the parens is naming/framing commentary and is discarded;
    comments *inside* are kept, since they name the groups within.
    """
    i = 0
    saw_not = False
    while i < len(fragment):
        ch = fragment[i]
        if ch.isspace():
            i += 1
        elif ch == COM:
            i = fragment.index(COM, i + 1) + 1
        elif ch == "!":
            saw_not = True
            i += 1
        else:
            break
    if saw_not != expect_not:
        raise ConversionError(
            f"expected {'a negated' if expect_not else 'an un-negated'} fragment"
        )
    if i >= len(fragment) or fragment[i] != "(":
        raise ConversionError("expected a parenthesised fragment")

    depth = 0
    for j in range(i, len(fragment)):
        if fragment[j] == "(":
            depth += 1
        elif fragment[j] == ")":
            depth -= 1
            if depth == 0:
                return fragment[i + 1 : j]
    raise ConversionError("unbalanced parentheses")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("script", type=Path, help="CTP DicomFilter script")
    ap.add_argument("--graylist-from", type=Path, help="recipe whose %filter graylist section to append")
    ap.add_argument("--output", type=Path, help="write the merged recipe here")
    ap.add_argument("--print-stats", action="store_true", help="report label counts and exit")
    args = ap.parse_args()

    keywords = load_dicom_keywords()
    if not keywords:
        print("warning: DICOM dictionary not found; field names unvalidated", file=sys.stderr)

    source, whitelist, gauntlet = parse_script(args.script)
    allowlist = emit_labels(source, whitelist, keywords)
    blacklist = emit_labels(source, gauntlet, keywords)

    if args.print_stats:
        print(f"allowlist labels: {len(allowlist)}")
        print(f"blacklist labels: {len(blacklist)}")
        groups: dict[str, int] = {}
        for name, _ in allowlist:
            groups[name.rsplit(" - ", 1)[0].rsplit(" [", 1)[0]] = (
                groups.get(name.rsplit(" - ", 1)[0].rsplit(" [", 1)[0], 0) + 1
            )
        print("\nlargest allowlist groups:")
        for name, count in sorted(groups.items(), key=lambda kv: -kv[1])[:10]:
            print(f"  {count:5d}  {name}")
        return 0

    if not args.output:
        ap.error("--output is required unless --print-stats is given")

    chunks = [
        HEADER.format(
            script=args.script.name,
            graylist=args.graylist_from.name if args.graylist_from else "(none)",
            allow=len(allowlist),
            block=len(blacklist),
        ),
        render_section("allowlist", allowlist),
        render_section("blacklist", blacklist),
    ]

    if args.graylist_from:
        text = args.graylist_from.read_text()
        marker = text.index("%filter graylist")
        chunks.append(text[marker:].rstrip() + "\n")

    args.output.write_text("\n".join(chunks))
    print(f"wrote {args.output} ({len(allowlist)} allowlist, {len(blacklist)} blacklist labels)")
    return 0


HEADER = """FORMAT dicom

# GENERATED FILE -- do not edit by hand.
# Regenerate with:
#   tools/ctp_filter_to_recipe.py {script} \\
#       --graylist-from {graylist} --output ctp_filter.txt
#
# Merged from two CTP scripts that are designed to be used together:
#
#   {script}
#     A CTP DicomFilter script: admission control. Shaped
#     `( device whitelist ) + !( rejection gauntlet )`, i.e. accept a file if it
#     is a known-good device, or if it is not caught by the gauntlet. Converted
#     here to the equivalent reject rule, `gauntlet AND NOT whitelist`, as a
#     %filter allowlist ({allow} labels) plus a %filter blacklist ({block} labels).
#
#   {graylist}
#     The CTP DicomPixelAnonymizer signature library: where to paint black on
#     devices that carry burned-in annotation. Appended verbatim as
#     %filter graylist.
#
# The two halves are inseparable. The whitelist deliberately admits devices that
# carry burned-in PHI -- the source script annotates them "-- SCRUBBED" -- on the
# understanding that the graylist rules will mask it. Running the allowlist
# without the graylist would emit that PHI; running the graylist without the
# blacklist would let every unlisted ultrasound, secondary capture and scanned
# document through unmasked.
#
# There is no %header section here on purpose. Run this recipe first, then run
# the header recipe over its output:
#
#   dicom-deid-rs <input> <staging> ctp_filter.txt
#   dicom-deid-rs <staging> <output> ctp_default.txt --var DATEINC ... --salt ...
#
# The staging directory holds masked pixels with fully identified headers. It is
# PHI: delete it when the run is done. Files the blacklist rejected are listed in
# blacklisted_files.txt in the working directory, which is where to look before
# starting the second stage.
#
# Conversion notes:
#   - The recipe condition format has no grouping and no negation of groups, so
#     each side of the script was converted to disjunctive normal form: one
#     LABEL per conjunction, negations pushed down to the atoms. A file need
#     match only one label, so the disjunction is preserved exactly.
#   - .containsIgnoreCase / .contains        -> contains (regex, case-insensitive)
#   - .startsWithIgnoreCase / .startsWith    -> contains with a ^ anchor
#   - .equalsIgnoreCase / .equals            -> equals (case-insensitive)
#   - .matches("re")                         -> contains ^re$ (CTP full-match)
#   - .equals("")  / !.equals("")            -> blank / notblank, which treat an
#     absent element as empty the way a CTP script does (r-2-6-9, r-2-6-10)
#   - Seq::Element                            -> preserved; resolved by r-2-6-11
#   - Values for `contains` are regex-escaped, since it compiles them as regexes.
#   - Tag numbers and CTP's own field labels were mapped to DICOM keywords
#     (SoftwareVersion -> SoftwareVersions, SeqOfUltrasoundRegions ->
#     SequenceOfUltrasoundRegions, [1018,1018] -> the (0018,1018) it means).
#     Conversion fails rather than emitting a keyword the dictionary does not
#     know, because an unresolvable field silently never matches.
#   - CTP's case-sensitive .equals/.contains have no case-sensitive counterpart
#     here, so matching is marginally broader. In the blacklist that rejects
#     marginally more; in the allowlist it admits marginally more.
#
# IMPORTANT (carried over from the CTP originals): it remains the user's
# responsibility to review images and confirm all PHI is removed. These rules
# cover known devices and known regions, nothing more.
"""


if __name__ == "__main__":
    sys.exit(main())
