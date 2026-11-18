#!/usr/bin/env python3
"""Patch pdf_oxide's Adobe-Japan1 CID -> Unicode table from Adobe's own mapping.

Exception-build tool (see CHANGELOG, .claude/rules/versioning.md). It rewrites
`src/fonts/cid_mappings/adobe_japan1.rs` in the sibling pdf_oxide checkout.

    python3 tools/gen_japan1_cid_table.py <Adobe-Japan1-UCS2> <adobe_japan1.rs>

`Adobe-Japan1-UCS2` is the CID -> Unicode CMap Adobe publishes in
https://github.com/adobe-type-tools/mapping-resources-pdf (pdf2unicode/,
BSD-3-Clause). The existing table was built from cid2code.txt by taking the
first (numerically smallest) code listed for a CID, which has two effects:

* a CID whose codes include a radical form (U+2E80..2FDF) got the radical
  instead of the ideograph (CID 2260 -> U+2EAC, not U+793A);
* a CID that has no code in the UniJIS-UCS2 / UTF16 columns is absent, which
  drops the half-width Latin, digit and symbol CIDs 232..632 among others.

What this script changes, and nothing else:

1. Every CID Adobe maps and the table lacks is added.
2. Every entry whose value is a radical form is replaced by Adobe's value,
   unless Adobe maps that CID to the same radical.

All other existing entries are kept as they are, so text that already
extracted keeps its exact characters.

The table holds one code point per CID. Adobe's value for a variant glyph is
its base character followed by a variation selector; the base character is
kept. A CID that Adobe maps to a sequence of several characters (a ligature
such as CID 8321, the four characters of a company-form square) has no
single-code-point value and is left out.
"""
import re
import sys


def is_variation_selector(cp):
    return 0xFE00 <= cp <= 0xFE0F or 0xE0100 <= cp <= 0xE01EF


def is_radical(cp):
    return 0x2E80 <= cp <= 0x2FDF


def parse_adobe_ucs2(path):
    """CID -> list of Unicode scalar values."""
    text = open(path, encoding="latin-1").read()

    def decode(hexstr):
        return [ord(c) for c in bytes.fromhex(hexstr).decode("utf-16-be")]

    table = {}
    for block in re.findall(r"beginbfchar(.*?)endbfchar", text, re.S):
        for src, dst in re.findall(r"<([0-9A-Fa-f]+)>\s*<([0-9A-Fa-f]+)>", block):
            table[int(src, 16)] = decode(dst)
    for block in re.findall(r"beginbfrange(.*?)endbfrange", text, re.S):
        for line in block.strip().splitlines():
            m = re.match(r"\s*<([0-9A-Fa-f]+)>\s*<([0-9A-Fa-f]+)>\s*(.*)", line)
            if not m:
                continue
            lo, hi, rest = int(m.group(1), 16), int(m.group(2), 16), m.group(3).strip()
            if rest.startswith("["):
                for i, dst in enumerate(re.findall(r"<([0-9A-Fa-f]+)>", rest)):
                    table[lo + i] = decode(dst)
            else:
                base = decode(re.match(r"<([0-9A-Fa-f]+)>", rest).group(1))
                for i in range(hi - lo + 1):
                    table[lo + i] = base[:-1] + [base[-1] + i]
    return table


def single_code_point(scalars):
    """The one code point the table can hold for this value, or None."""
    base = [cp for cp in scalars if not is_variation_selector(cp)]
    return base[0] if len(base) == 1 else None


ENTRY = re.compile(r"^    (\d+)u16 => 0x([0-9A-Fa-f]+),$", re.M)


def main(adobe_path, rs_path):
    adobe = parse_adobe_ucs2(adobe_path)
    source = open(rs_path).read()
    current = {int(c): int(u, 16) for c, u in ENTRY.findall(source)}
    if not current:
        sys.exit("no table entries found in " + rs_path)

    merged = dict(current)
    added = replaced = skipped_sequences = 0
    for cid, scalars in adobe.items():
        if cid == 0:
            continue
        cp = single_code_point(scalars)
        if cid not in current:
            if cp is None:
                skipped_sequences += 1
                continue
            merged[cid] = cp
            added += 1
        elif is_radical(current[cid]) and cp is not None and cp != current[cid]:
            merged[cid] = cp
            replaced += 1

    first = ENTRY.search(source).start()
    last = [m for m in ENTRY.finditer(source)][-1].end()
    body = "\n".join(
        "    %du16 => 0x%04X," % (cid, merged[cid]) for cid in sorted(merged)
    )
    out = source[:first] + body + source[last:]

    old_count = str(len(current))
    out = out.replace(
        "//! Generated from Adobe cid2code.txt (UniJIS-UCS2-H + UTF16/UTF32 fallback)\n"
        "//! Source: <https://github.com/adobe-type-tools/cmap-resources>\n",
        "//! Generated from Adobe cid2code.txt (UniJIS-UCS2-H + UTF16/UTF32 fallback)\n"
        "//! Source: <https://github.com/adobe-type-tools/cmap-resources>\n"
        "//!\n"
        "//! Completed from Adobe's CID-to-Unicode CMap `Adobe-Japan1-UCS2`\n"
        "//! (<https://github.com/adobe-type-tools/mapping-resources-pdf>,\n"
        "//! Copyright 1990-2023 Adobe, BSD-3-Clause): the CIDs the first pass\n"
        "//! left out were added, and the entries that pointed at a radical form\n"
        "//! (U+2E80..2FDF) now point at the character Adobe gives for the CID.\n"
        "//! A variant glyph maps to its base character; a CID Adobe maps to a\n"
        "//! sequence of several characters has no entry.\n",
    )
    out = out.replace(
        "//! Coverage: %s CID-to-Unicode mappings" % old_count,
        "//! Coverage: %d CID-to-Unicode mappings" % len(merged),
    )
    out = out.replace(
        "/// %s entries merged from UniJIS-UCS2-H, UTF16, and UTF32 CMaps." % old_count,
        "/// %d entries: UniJIS-UCS2-H, UTF16 and UTF32 CMaps, completed from\n"
        "/// Adobe-Japan1-UCS2." % len(merged),
    )
    open(rs_path, "w").write(out)
    print(
        "entries %d -> %d (added %d, radical forms replaced %d, "
        "multi-character CIDs left out %d)"
        % (len(current), len(merged), added, replaced, skipped_sequences)
    )


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
