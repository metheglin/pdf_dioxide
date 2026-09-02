# Changelog

Version scheme: see `.claude/rules/versioning.md`. While in tracking mode
the gem uses `0.1.x` ("built against upstream main"); pinned mode switches
to `<upstream version>.<build>`. Ext additions and exception builds always get
an entry here.

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
