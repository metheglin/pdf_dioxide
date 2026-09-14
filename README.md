# pdf_dioxide

Ruby bindings for the [pdf_oxide](https://crates.io/crates/pdf_oxide) PDF engine,
built as a native extension with [Magnus](https://github.com/matsadler/magnus).

> **Naming note**: `pdf_dioxide` is a provisional name while this gem is
> private and may change before any public release. "PDFOxide" /
> "pdf_oxide" are trademarks of the upstream project (see its TRADEMARKS.md) —
> clear naming/namespace with upstream before publishing.

This is the Ruby counterpart to pdf_oxide's PyO3 bindings for Python: instead of
talking to a C ABI over FFI, the Rust types are wrapped as real Ruby objects.

| Role | Python side | Ruby side (here) |
|---|---|---|
| Binding crate | PyO3 | Magnus (rb-sys underneath) |
| Build tool | maturin | rb_sys + rake-compiler |
| Entry point | `#[pymodule]` | `#[magnus::init]` |
| Class export | `#[pyclass]` | `#[magnus::wrap]` |
| Dev build | `maturin develop` | `rake compile` |

## Requirements

- Ruby >= 3.2
- A Rust toolchain (stable; install via [rustup](https://rustup.rs))
- A C compiler for the extension link step

## Development

The extension currently depends on the sibling `pdf_oxide` working tree via a
`path` dependency in `ext/pdf_dioxide/Cargo.toml`, so the checkout is
expected to sit next to this one:

```
pdf/
├── pdf_oxide/            # the Rust crate
└── pdf_dioxide/          # this gem
```

Switch that to `pdf_oxide = "0.3"` (crates.io) before publishing.

```bash
bundle install
bundle exec rake compile   # builds ext/ into lib/pdf_dioxide/ (≈ maturin develop)
bundle exec rake test
```

`rake` with no arguments runs `compile` then `test`.

## Precompiled gem (no Rust on the target)

`tools/cross_build.sh` builds a platform gem with [rb-sys-dock](https://github.com/oxidize-rb/rb-sys)
(Docker), default target `aarch64-linux` with binaries for Ruby 3.4 and 4.0:

```bash
tools/cross_build.sh                                   # -> pkg/pdf_dioxide-<ver>-aarch64-linux.gem
tools/verify_native_gem.sh pkg/pdf_dioxide-*-aarch64-linux.gem   # installs it in a Rust-less ruby:4.0 arm64 container
```

The resulting gem has no `extensions`, so `gem install` on the target needs
no Rust toolchain. The loader picks `lib/pdf_dioxide/<major.minor>/` per
Ruby version. See the script header for the mount/version caveats.

## Usage

```ruby
require "pdf_dioxide"

doc = PdfDioxide::PdfDocument.open("report.pdf")
doc.page_count             #=> Integer
puts doc.extract_text(0)   # page index is 0-based
```

## License

MIT. pdf_oxide itself is dual-licensed MIT / Apache-2.0; keep its license notice
in any distributed build.
