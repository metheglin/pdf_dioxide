# Changelog

Version scheme: `<upstream pdf_oxide version>.<pdf_dioxide build>`
(`.claude/rules/versioning.md`). Every fourth-segment bump gets an entry here;
breaking changes to the wrapper or the Ext surface are called out explicitly.

## 0.3.78.0

**Switched to pinned mode.** The sibling checkout now sits on the `v0.3.78`
release tag instead of tracking `main`, so the gem leaves the `0.1.x`
tracking series and takes the `<upstream release>.<build>` scheme. Upstream
baseline: pdf_oxide **0.3.78** (tag `ad49c4cb`) plus the local trailer patch
(`UPSTREAM_VERSION` reports `0.3.78+fixtrailer`).

No API change from 0.1.2 — same parity surface, same Ext surface.

### Distribution: precompiled gems only

The gem is now shipped **exclusively as platform (native) gems** with the
compiled extension bundled. Installing one needs no Rust toolchain, no cargo
and no network fetch of crates; `gem install` just unpacks.

| platform | Ruby |
|---|---|
| `arm64-darwin-24` | the building host's Ruby (4.0) |
| `aarch64-linux` | 3.4 and 4.0 |
| `x86_64-linux` | 3.4 and 4.0 |

- Build them all with `tools/build_release.sh`; verify a Linux artifact in a
  Rust-less container with `tools/verify_native_gem.sh`.
- **No source gem is published.** `rake build` / `rake install` still exist for
  development, but they compile Rust at install time and are not the delivery
  path.

## 0.1.2

Parity baseline: pdf_oxide **0.3.78** (tag `ad49c4cb`). The previous baseline
was unreleased main (`3be1951`); the sibling checkout now sits on a release tag.

### Parity (re-sync to 0.3.78)

- `LineCap#inspect` / `LineJoin#inspect` (upstream added `__repr__` to both).
- `PdfDioxide::Ext.content_operators` now emits `"s"` for the new
  `Operator::CloseAndStroke` variant (ISO 32000-1 Table 60: close and stroke,
  `h S`). Previously that operator did not exist upstream.
- No behaviour changes to existing methods: the large python.rs diff is a pure
  move of `new` / `from_bytes` / `authenticate` / `save_encrypted` /
  `to_bytes_encrypted` into a separate `#[pymethods]` block (upstream did it so
  static analysis stops treating a `password` argument as tainting the whole
  block). Bodies are identical.

### Ext: `experimental_render_page` graduated

Upstream 0.3.78 fixes issue #1309 in `render_page` itself (it now resolves an
indirect Form XObject `/Resources` before loading fonts), so the 0.1.1 fork
patch and its opt-in flag are gone.

- **`PdfDocument#render_page` is now correct on its own** — use it.
- `experimental_render_page` is kept for one release as a deprecating shim:
  it warns once per process and delegates to `render_page`. Registry entry is
  `status: :graduated, graduated_in_upstream: "0.3.78"`. It will be deleted in
  a later release.

### Exception build (patched upstream) — expires at the next sync

- Fork branch (own fork `metheglin/pdf_oxide`): `pdf_dioxide/ad49c4cb-fixtrailer`,
  cut from the `v0.3.78` tag. Crate version `0.3.78+fixtrailer`, visible as
  `PdfDioxide::UPSTREAM_VERSION`.
- Patch: `src/xref.rs` traditional-xref parser. ISO 32000-1 7.5.5 does not
  require whitespace between the `trailer` keyword and its dictionary, so
  `trailer<</Size 5>>` (what linearized writers commonly emit) is well-formed;
  the parser dropped the dict, losing `/Prev` and leaving every earlier xref
  section unmerged. Upstream issue: not yet filed.
- Deviation from `.claude/rules/versioning.md`: the rule wants an exception
  patch to be **opt-in behind a flag** so parity methods stay byte-identical.
  That does not apply to a parser-level fix — there is no meaningful "parse the
  trailer incorrectly" mode, and no Ext method to gate it behind. The patch is
  therefore unconditional. It only *adds* handling for the glued form, so files
  that already parsed are unaffected.

## 0.1.1

Parity baseline: pdf_oxide main @ `3be1951` (crate version 0.3.77 + 95
unreleased commits); re-synced (python.rs +8 lines → 1 new getter, below).

### Parity (re-sync to main @ `3be1951`)

- `TextSpan#page_bbox` (new upstream getter: bbox with text-matrix rotation
  resolved to an axis-aligned page-space hull).

### Exception build (patched upstream) — expires at the next sync

- Upstream issue: https://github.com/yfedoseev/pdf_oxide/issues/1309 (text in
  a Form XObject whose `/Resources` is an indirect reference is not rendered).
- Fork branch (own fork `metheglin/pdf_oxide`): `pdf_dioxide/3be1951-fix1309`,
  cut from `3be1951`; one commit adding the opt-in
  `RenderOptions::resolve_form_resources` (default off — `render_page` is
  byte-identical to upstream). Crate version `0.3.77+fix1309`, visible as
  `PdfDioxide::UPSTREAM_VERSION`.
- Exposed only through Ext: `PdfDocument#experimental_render_page(page, **opts)`
  (same kwargs as `render_page`). Graduates into `render_page` when the
  upstream fix lands.

### Ext (pdf_dioxide-only, `require "pdf_dioxide/ext"`)

Object-model and content-stream access — the layer upstream exposes to Rust
but to no binding:

- `PdfDioxide::Ext::ObjectRef` (`5 0 R`, value semantics) and
  `PdfDioxide::Ext::Stream` (`#dict` / `#data` / `#raw_data`).
- `PdfDocument#page_object`, `#catalog_object`, `#trailer_object`,
  `#load_object(ref)`, `#resolve(obj)`, `#form_xobject?(ref)`,
  `#page_content(index)`, `#stream_data(ref)`.
- `PdfDioxide::Ext.content_operators(bytes, text_only: false)` — content
  stream as `[mnemonic, *operands]` arrays.
- Object mapping: Dictionary→Hash (String keys), Name→Symbol, String→binary
  String, Reference→ObjectRef, Stream→Stream.
- Not available for encrypted PDFs: `Stream#data` / `#stream_data` raise
  `PdfDioxide::UnsupportedError` (stream decryption is `pub(crate)` upstream).

### Versioning

- `VERSION` switched to the four-segment scheme; added `PdfDioxide::PARITY`,
  `PdfDioxide::UPSTREAM_VERSION` (linked crate, set natively),
  `PdfDioxide::CARGO_FEATURES` and `PdfDioxide.version_info`.

## 0.1.0

Initial port of python.rs (594 / 610 tracked items).
