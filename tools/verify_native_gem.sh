#!/usr/bin/env bash
# Verify a precompiled (native) gem in a pristine Ruby container WITHOUT a
# Rust toolchain. Usage:
#   tools/verify_native_gem.sh pkg/pdf_dioxide-0.1.1-aarch64-linux.gem [linux/arm64] [ruby:4.0]
set -euo pipefail
gem_path=${1:?gem file}; platform=${2:-linux/arm64}; image=${3:-ruby:4.0}
gem_dir=$(cd "$(dirname "$gem_path")" && pwd); gem_file=$(basename "$gem_path")
fixtures=$(cd "$(dirname "$0")/../test/fixtures" && pwd)

docker run --rm --platform "$platform" \
  -v "$gem_dir:/gems:ro" -v "$fixtures:/fixtures:ro" \
  "$image" bash -c '
set -e
echo "== host: $(uname -m), $(ruby -v)"
command -v cargo >/dev/null && { echo "cargo present — not a clean test"; exit 1; } || echo "== no cargo (good)"
gem install --local --no-document /gems/'"$gem_file"' >/dev/null
echo "== installed: $(gem list pdf_dioxide | tr -d "\n")"
ruby -e "
require \"pdf_dioxide/ext\"
doc = PdfDioxide::PdfDocument.open(\"/fixtures/form_xobject_text.pdf\")
puts \"version_info: #{PdfDioxide.version_info.slice(:gem, :upstream_crate)}\"
puts \"page_count: #{doc.page_count}, text: #{doc.extract_text(0).lines.first.strip.inspect}\"
a = doc.render_page(0, dpi: 40); b = doc.experimental_render_page(0, dpi: 40)
puts \"render_page: #{a.bytesize} bytes, experimental_render_page: #{b.bytesize} bytes, differ: #{a != b}\"
puts \"loaded from: #{\$LOADED_FEATURES.grep(/pdf_dioxide\\.(so|bundle)/).first}\"
"
echo "== OK"'
