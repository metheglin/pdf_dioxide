# frozen_string_literal: true

module PdfDioxide
  # Pinned mode (see .claude/rules/versioning.md): <upstream pdf_oxide
  # release>.<pdf_dioxide build>. The first three segments are the python.rs
  # release the parity surface tracks; the fourth counts pdf_dioxide-only
  # changes and resets to 0 on every re-sync.
  #
  # A trailing ".fixNNNN" segment marks an exception build (patched upstream):
  # RubyGems treats it as a prerelease, so it sorts below the plain build
  # number and Bundler never selects it unless it is pinned explicitly.
  VERSION = "0.3.78.2.fix20261118"

  # The python.rs baseline the parity surface was ported from (the v0.3.78
  # tag). The build tree carries local patches on top, which UPSTREAM_VERSION
  # reports as "0.3.78+fixtrailer.fix20261118" (see CHANGELOG
  # 0.3.78.2.fix20261118).
  PARITY = { version: "0.3.78", commit: "ad49c4cb3638dc950a29350ef882161372fde473" }.freeze

  # Cargo features the extension is built with (mirror of
  # ext/pdf_dioxide/Cargo.toml; keep in sync by hand).
  CARGO_FEATURES = %w[rendering signatures barcodes].freeze
end
