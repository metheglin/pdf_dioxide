# pdf_dioxide-only extensions ("Ext") and versioning policy

pdf_dioxide is positioned as a **1:1 port of upstream's Python binding**
(`src/python.rs`). Some capabilities exist in the Rust core but are not
exposed to Python; this gem may ship them ahead of an upstream feature
request. Such additions must be **unmistakably distinct** from the parity
surface. This file defines how.

`.claude/rules/maintenance.md` governs the parity surface; this file governs
everything that is NOT in python.rs.

## Definitions

- **Parity surface** — everything derived from python.rs. Lives in
  `ext/pdf_dioxide/src/lib.rs` and `lib/pdf_dioxide.rb`. Tracked by
  `tools/python_port_progress.csv`.
- **Ext surface** — pdf_dioxide-only additions. Lives in
  `ext/pdf_dioxide/src/ext.rs` and `lib/pdf_dioxide/ext.rb`. Tracked by the
  `PdfDioxide::Ext::FEATURES` registry inside `ext.rb` (no CSV — see
  "Registry" below). Ruby namespace `PdfDioxide::Ext`.

## Code layout

```
ext/pdf_dioxide/src/
  lib.rs      parity only. `init` registers parity, then calls `ext::init`.
  ext.rs      Ext only. Depends on lib.rs helpers (pub(crate)); never the reverse.
lib/
  pdf_dioxide.rb        parity pure-Ruby layer
  pdf_dioxide/ext.rb    Ext pure-Ruby layer — loaded ONLY via `require "pdf_dioxide/ext"`
tools/
  python_port_progress.csv   parity tracker (see maintenance.md); Ext has no CSV
```

Rules for `ext.rs`:

- Every item carries a doc comment starting with `EXT:` giving the rationale,
  the Rust core API it exposes, and the upstream issue/PR link (or "not yet
  filed").
- Native Ext methods are defined on **modules** (`PdfDioxide::Ext::Document`,
  `PdfDioxide::Ext::Page`, ...), never directly on the parity classes. Ruby
  decides how they attach (below).
- Native implementations that back an *override* of a parity method are
  named `_ext_<name>` and left private-ish; the override itself is pure
  Ruby (Rust cannot call `super` cleanly).
- Reuse `map_pdf_error`, the `PdfDioxide::*Error` classes and the existing
  wrap patterns from lib.rs. Expose what ext.rs needs as `pub(crate)`.

## Opt-in loading (the "require boundary")

`require "pdf_dioxide"` alone must yield **exactly the parity surface** —
nothing more. That guarantee is the whole point.

`require "pdf_dioxide/ext"` attaches the Ext surface:

```ruby
# lib/pdf_dioxide/ext.rb
PdfDioxide::PdfDocument.include  PdfDioxide::Ext::Document           # new methods
PdfDioxide::PdfDocument.prepend  PdfDioxide::Ext::DocumentOverrides  # extended methods
```

- **`include`** for brand-new methods. (`include` sits above the class in
  the ancestor chain, so it can never shadow a parity method — safe by
  construction.)
- **`prepend`** for extending an existing parity method. The override is
  pure Ruby, must call `super` for the parity path, and must obey the
  compatibility rule below.

Introspection: `doc.method(:x).owner` returns the `PdfDioxide::Ext::*`
module for anything Ext-provided. `PdfDioxide::Ext.features` returns the
registry entries (below) at runtime.

## Compatibility rule for overrides (non-negotiable)

An Ext override may only **add** behaviour reachable through new optional
arguments. **With the new arguments omitted, the call must behave exactly as
the parity method** — same result, same errors. Loading `pdf_dioxide/ext`
must never change what existing parity calls return.

```ruby
def extract_image_bytes(page, index: nil)
  return super(page) if index.nil?           # untouched parity path
  _ext_extract_image_bytes_at(page, index)   # Ext path
end
```

If the desired behaviour cannot be expressed that way, give it a **new
method name** instead of overriding.

## Naming

- Use the name upstream would most plausibly choose — normally the Rust
  core function's name. No `dx_`/`ext_` prefixes on the public Ruby name:
  prefixes force a rename at graduation.
- Keyword arguments over positional for any new option (easier to keep
  parity-compatible and to graduate).

## Graduation (upstream adopts the feature)

Detected by the normal re-sync: the regenerated parity CSV shows a new
`ported=false` row matching an Ext entry.

1. Port the upstream version into the parity surface (lib.rs) per
   maintenance.md — faithfully, even if it differs from the Ext version.
2. Turn the Ext method into a thin shim: delegate to the parity method and
   `warn` (once per process) that it graduated, naming the parity call.
3. Keep the shim for **one minor release** of pdf_dioxide, then delete it.
4. Update the registry entry: `status: :graduated`,
   `graduated_in_upstream: "<version>"`; when the shim is deleted, either
   drop the entry or keep it with `status: :removed` (keep it if the name
   is likely to be searched for).

If upstream adopts a *different* signature, the shim adapts arguments where
possible and the CHANGELOG calls out the difference.

## Registry (single source of truth, in code)

Ext items are few and hand-written, so they are NOT tracked in a CSV (the
parity CSV exists only because it is machine-generated from python.rs and
re-merged on every re-sync). Instead the registry is a Ruby constant next to
the code it describes:

```ruby
# lib/pdf_dioxide/ext.rb
module PdfDioxide::Ext
  FEATURES = [
    { name: :extract_image_bytes, kind: :override, attach: :prepend,
      rust_api: "extractors::PdfImage::to_png_bytes (single index)",
      since: "0.3.77.1", upstream_issue: "not yet filed",
      status: :extension, graduated_in_upstream: nil },
  ].freeze

  def self.features = FEATURES
end
```

- `kind`: `:method` | `:override` | `:class` | `:function`
- `attach`: `:include` | `:prepend` | `:module_function`
- `status`: `:extension` | `:graduated` | `:removed`

Add the entry in the same change that adds the code. `Ext.features` returns
it verbatim, so registry and runtime can never disagree. During a re-sync,
cross-check every `FEATURES[].name` against new `ported=false` rows in the
parity CSV to detect graduations.

## Versioning

**Gem version = upstream python.rs version + a fourth build segment**
(four-segment versions are valid RubyGems versions; precedent: binding gems
such as `libv8-node`):

```
PdfDioxide::VERSION = "0.3.77.1"
                       ^^^^^^ ^
                       upstream pdf_oxide version the parity surface tracks
                              pdf_dioxide build number
```

Rules:

- Re-syncing the parity surface to a new upstream version replaces the
  first three segments and **resets the fourth to 0**.
- Any pdf_dioxide-only change (Ext addition, wrapper fix, build change)
  bumps the fourth segment.
- Because the fourth segment cannot express "breaking", **every fourth-segment
  bump requires a CHANGELOG entry**, and any breaking change to the wrapper
  or Ext surface must be called out there explicitly (and should be rare —
  prefer new names over changed behaviour).
- The upstream version is derivable: `VERSION.split(".")[0, 3].join(".")`.

Complementary runtime constants (keep; they detect drift):

- `PdfDioxide::UPSTREAM_VERSION` — the pdf_oxide crate version actually
  linked into the extension (embedded at build time). Should equal the first
  three segments of `VERSION`; a mismatch means the Cargo pin and the parity
  baseline have drifted.
- `PdfDioxide::PARITY` — `{ version:, commit: }` of the python.rs baseline
  (same values as the Baseline table in maintenance.md).
- `PdfDioxide.version_info` — one Hash with gem, upstream crate, parity
  baseline, enabled Cargo features, and `Ext.features` — for bug reports.

## Checklist for adding an Ext item

```text
[ ] confirm it is NOT in python.rs (grep; otherwise it belongs to parity)
[ ] file/locate the upstream feature request; record the link
[ ] implement in ext.rs (EXT: doc comment) + attach in ext.rb (include/prepend)
[ ] override? → super-path untouched, new args optional, compatibility rule holds
[ ] add the entry to PdfDioxide::Ext::FEATURES in ext.rb
[ ] bump fourth version segment + CHANGELOG entry
[ ] `require "pdf_dioxide"` alone still exposes only the parity surface
```
