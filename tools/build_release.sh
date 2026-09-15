#!/usr/bin/env bash
# Build every distributed artifact for a release.
#
# This project ships ONLY precompiled (native) gems: installing must never
# require a Rust toolchain. No source gem is published.
#
#   tools/build_release.sh              # host (darwin) + aarch64-linux + x86_64-linux
#   tools/build_release.sh host         # host only (fast; your own machine)
#   tools/build_release.sh aarch64-linux x86_64-linux
#
# Linux targets cross-compile in Docker via tools/cross_build.sh (Ruby 3.4 +
# 4.0 per gem). The host target builds natively for the running Ruby only.
# Verify Linux artifacts afterwards with tools/verify_native_gem.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

targets=("$@")
[ ${#targets[@]} -eq 0 ] && targets=(host aarch64-linux x86_64-linux)

version=$(ruby -Ilib -e 'require "pdf_dioxide/version"; print PdfDioxide::VERSION')
echo "== pdf_dioxide $version =="

for t in "${targets[@]}"; do
  echo "== building $t =="
  if [ "$t" = "host" ]; then
    bundle exec rake native gem
  else
    tools/cross_build.sh "$t"
  fi
done

echo
echo "== artifacts for $version =="
ls -la pkg/pdf_dioxide-"$version"-*.gem
echo
echo "None of these declare extensions (install needs no Rust):"
for g in pkg/pdf_dioxide-"$version"-*.gem; do
  printf '  %-46s extensions=%s\n' "$(basename "$g")" "$(gem spec "$g" extensions | tr -d '\n' | tr -s ' ')"
done
