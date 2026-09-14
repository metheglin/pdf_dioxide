#!/usr/bin/env bash
# Build a precompiled (native) gem for a Linux target with rb-sys-dock.
#
#   tools/cross_build.sh                 # aarch64-linux, Ruby 3.4 + 4.0
#   tools/cross_build.sh x86_64-linux    # other target (see rb-sys-dock --list-platforms)
#   RUBY_VERSIONS=3.3,3.4,4.0 tools/cross_build.sh
#
# Why the odd invocation:
# - It runs from the PARENT directory: rb-sys-dock bind-mounts $(pwd) at the
#   same absolute path inside the container, so the sibling ../pdf_oxide
#   checkout (the Cargo path dependency, absolute in ext/pdf_dioxide/Cargo.toml)
#   resolves without any manifest change or push.
# - At least two Ruby versions are required: rake-compiler only uses the
#   per-version lib/pdf_dioxide/<major.minor>/ layout when targeting several
#   Rubies; a single version collides with the host build (see Rakefile).
# - Inside the container git refuses the mounted repo ("dubious ownership");
#   the gemspec falls back to a glob for spec.files in that case.
#
# Output: pkg/pdf_dioxide-<version>-<platform>.gem. Verify with
#   tools/verify_native_gem.sh pkg/pdf_dioxide-*-aarch64-linux.gem
set -euo pipefail
platform=${1:-aarch64-linux}
ruby_versions=${RUBY_VERSIONS:-3.4,4.0}
gem_dir=$(cd "$(dirname "$0")/.." && pwd)
parent=$(dirname "$gem_dir")

cd "$parent"
BUNDLE_GEMFILE="$gem_dir/Gemfile" bundle exec rb-sys-dock \
  --platform "$platform" --ruby-versions "$ruby_versions" \
  --directory "$gem_dir" --build
ls -la "$gem_dir"/pkg/*-"$platform".gem
