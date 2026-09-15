# Upstream maintenance policy

How to keep this gem (pdf_dioxide) in sync with the upstream `pdf_oxide`
crate. The port's reference surface is upstream's **`src/python.rs`** (the
PyO3 binding): every Ruby API here was derived from it 1:1, so upstream
tracking means tracking python.rs, not the whole crate.

## Baseline

The current implementation was ported from:

| | |
|---|---|
| upstream crate version | `pdf_oxide 0.3.78` (release tag; the build tree carries one local patch, so `UPSTREAM_VERSION` reads `0.3.78+fixtrailer` — see CHANGELOG 0.1.2) |
| upstream commit | `ad49c4cb` (2026-09-08, tag `v0.3.78`) — build tree is on fork branch `pdf_dioxide/ad49c4cb-fixtrailer` |
| python.rs size at baseline | 8,659 lines |
| dependency form | `path = "../../../pdf_oxide"` (sibling working tree) |
| enabled features | `rendering`, `signatures`, `barcodes` (subset of upstream's `python` feature bundle) |

**Update this table every time the port is re-synced to a newer upstream.**
The commit hash is the anchor for diffing; the version alone is not enough
because the sibling working tree may sit between releases.

## Detecting upstream changes

Two complementary detectors; run both when re-syncing:

1. **API-surface diff (automated).** Regenerate the porting tracker:

   ```bash
   ruby tools/extract_python_api.rb ../pdf_oxide/src/python.rs tools/python_port_progress.csv
   ```

   The script re-extracts every `#[pyclass]` / `#[pymethods]` /
   `#[pyfunction]` from python.rs and **merges** with the existing CSV:
   rows already `ported=true` stay true (line numbers re-anchor
   automatically), and comments carry forward. Anything upstream added or
   renamed shows up as a fresh `ported=false` row — that is the signal for
   new work. Rows that disappear from the CSV indicate upstream removals.

2. **Behavior diff (manual).** Diff python.rs itself between the baseline
   commit and the new target:

   ```bash
   cd ../pdf_oxide && git diff <baseline-commit>..HEAD -- src/python.rs
   ```

   The CSV catches additions/removals but NOT behavior changes inside an
   existing method body (changed defaults, different core calls, new error
   mapping). Read every hunk that touches a method already marked
   `ported=true`. Also skim `CHANGELOG.md` and the `[features]` section of
   upstream `Cargo.toml` (the `python = [...]` bundle in particular — if it
   grows a new feature, decide whether this gem should enable it too).

## IMPORTANT: namespace conversion (PdfOxide → PdfDioxide)

Upstream code, docs, and every other binding use the **`PdfOxide`**
namespace (`PdfOxide::PdfDocument`, `pdf_oxide.PdfDocument`, ...). This gem
deliberately does NOT: to avoid confusion with the upstream trademarks
("PDFOxide" / "pdf_oxide" / "PdfOxide" — see upstream `TRADEMARKS.md`),
**everything user-visible here lives under `PdfDioxide`**. Every port from
python.rs must apply this conversion; it is easy to leak the upstream name
by copying code verbatim.

Convert (upstream → this gem):

- `#[pyclass(... name = "X")]` / `PdfOxide::X` in examples
  → `#[magnus::wrap(class = "PdfDioxide::X", ...)]` — the wrap attribute
  string is **functional** (runtime class lookup when wrapping values), not
  a comment; a leaked `"PdfOxide::X"` compiles fine and then fails at
  runtime the first time an instance is returned.
- New classes must be registered on the module handle created by
  `define_module("PdfDioxide")` (`ruby.get_inner(&PDF_OXIDE)` — the static
  is still named `PDF_OXIDE` internally; only the Ruby-visible string
  matters).
- Pure-Ruby additions in `lib/pdf_dioxide.rb` go inside `module PdfDioxide`;
  fully-qualified references in method bodies (`PdfDioxide::Error`,
  `PdfDioxide._measure_text`, `is_a?(PdfDioxide::Table)`) and `inspect`
  strings (`#<PdfDioxide::X ...>`) must use the new name.
- Doc comments, tests, RBS signatures: use `PdfDioxide::` throughout.

Do NOT convert:

- the crate/package name `pdf_oxide` (Cargo dependency, `use pdf_oxide::…`
  paths, crates.io references) — that is the upstream Rust crate itself;
- prose that genuinely refers to the upstream project ("derived from
  PDFOxide", the trademark note in README).

After any port, verify no leak:

```bash
grep -rn "PdfOxide" ext lib test sig | grep -v "PdfDioxide"   # expect empty
```

(The strings `PdfOxide`/`PdfDioxide` have no substring overlap in that
direction, so a plain replace of leaked occurrences is safe.)

## Policy: newly added features (additive)

1. Regenerate the CSV; new rows appear as `ported=false`.
2. Port them following the established patterns in
   `ext/pdf_dioxide/src/lib.rs` — pick the one matching the upstream shape:
   - **Plain method on PdfDocument** → method on `RbPdfDocument`, errors via
     `map_pdf_error`, optional args via `scan_args`, keyword args via
     `get_kwargs` (arity `-1`).
   - **PyO3 returns a dict / list of dicts** → return `RHash` / `RArray` of
     hashes (see `path_to_hash`, `redaction_report_to_hash`).
   - **New `#[pyclass]` wrapping a Rust value** → `#[magnus::wrap]` struct
     (`RefCell` only if methods need `&mut`).
   - **Class holding a back-reference to the document/builder** → pure Ruby
     delegation class in `lib/pdf_dioxide.rb` (see `Page`, `PdfPageRegion`);
     never hold a Ruby object inside a wrapped Rust struct.
   - **Builder API that borrows another builder** → buffer ops Ruby-side and
     replay in one native call (see `FluentPageBuilder` / `_apply_page`).
3. Set `ported=true` in the CSV, smoke-test the new API, keep
   `bundle exec rake` green.
4. If the addition is feature-gated upstream, either enable the feature in
   `ext/pdf_dioxide/Cargo.toml` or add the row's reason to the CSV
   `comment` column (like the existing ocr / tsa-client entries).

## Policy: changed behavior in existing APIs (spec changes)

The port is **faithful by default**: when upstream changes semantics
(defaults, return shapes, error kinds), follow it, even if breaking for
Ruby users — then reflect the break in this gem's own version bump.
Exceptions are the deliberate Ruby-side divergences, which must be
**preserved** across re-syncs (do not "fix" them back to Python shapes):

- wrong password raises `PdfDioxide::PasswordError` (Python: RuntimeError)
- `page_count` returns a plain Integer (Python: `_PageCount` wrapper)
- `render_pixmap` / `render_separations` return Hashes (Python: helper classes)
- `PadesLevel` is a String `"B_B"`... (Python: enum class)
- `BlendMode` / `LineCap` / `LineJoin` constructors are lowercase
- `Align` is a Ruby constants module + Symbols accepted
- `__enter__`/`__exit__` → block-form `PdfDocument.open`
- dunders map to Ruby idioms: `__len__`→`length`, `__getitem__`→`[]`,
  `__iter__`→`each`/Enumerable, `__repr__`→`inspect`
- `merge_from` distinguishes path vs bytes by `%PDF-` magic (one String type)

When a spec change lands, update the affected method, note anything
user-visible in the README, and re-run the full smoke of that area.

## Policy: upstream removals / renames

A row vanishing from the regenerated CSV means upstream removed or renamed
the API. Mirror the removal (or rename) in the same change; do not keep
dead wrappers — the CSV is the single source of truth for the surface.

## Known fragile coupling points

Check these first when a re-sync breaks the build:

- `sync_editor_erasures` is unportable while `PdfDocument.erase_regions` is
  `pub(crate)`; if upstream ever exposes an accessor, port it and drop the
  workaround notes on `remove_headers` etc.
- `#[non_exhaustive]` upstream structs (`RedactionOptions`,
  `RevocationMaterial`, `PadesLevel`) are built via `default()` + field
  assignment / matched with wildcard arms — new upstream fields/variants
  compile silently; re-check them on each sync.
- `compliance` submodules are private; only the `pdf_oxide::compliance::*`
  re-exports are usable.
- `content::Operator` is a plain (not `#[non_exhaustive]`) enum and
  `ext.rs`'s `operator_to_ruby` matches it exhaustively, so a new upstream
  operator breaks `cargo check` with "non-exhaustive patterns". That is the
  intended failure: add the arm with the PDF mnemonic from
  `content/parser.rs` (0.3.78 added `CloseAndStroke` -> `"s"`).
- Feature parity: this gem enables `rendering`, `signatures`, `barcodes`;
  upstream's `python` bundle also has `parallel`, `logging`, `tsa-client`,
  plus optional `ocr`. Revisit when porting the remaining CSV rows.
- The dependency is a **path dep on the sibling working tree**, written as
  an ABSOLUTE path so `rake install` works (an installed gem compiles its
  extension inside the gem dir, where a relative path cannot reach the
  checkout). This ties local installs to this machine. Before any publish,
  switch `ext/pdf_dioxide/Cargo.toml` to a pinned crates.io version
  (`pdf_oxide = "=X.Y.Z"`), and only bump it together with a re-sync pass
  described above.

## Re-sync checklist

```text
[ ] note new upstream commit/version; update the Baseline table above
[ ] regenerate CSV (detector 1) — list new/removed rows
[ ] git diff python.rs (detector 2) — list behavior changes to ported rows
[ ] port additions; mirror removals; apply spec changes (policies above)
[ ] cargo check → bundle exec rake compile → bundle exec rake (tests)
[ ] smoke-test touched areas; update CSV ported/tested + comments
[ ] update README if user-visible behavior changed
```