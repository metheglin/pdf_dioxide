# frozen_string_literal: true

module PdfDioxide
  # Pinned mode (see .claude/rules/versioning.md): <upstream pdf_oxide
  # release>.<pdf_dioxide build>. The first three segments are the python.rs
  # release the parity surface tracks; the fourth counts pdf_dioxide-only
  # changes and resets to 0 on every re-sync.
  VERSION = "0.3.78.0"

  # The python.rs baseline the parity surface was ported from (the v0.3.78
  # tag). The build tree carries one local patch on top, which
  # UPSTREAM_VERSION reports as "0.3.78+fixtrailer" (see CHANGELOG 0.3.78.0).
  PARITY = { version: "0.3.78", commit: "ad49c4cb3638dc950a29350ef882161372fde473" }.freeze

  # Cargo features the extension is built with (mirror of
  # ext/pdf_dioxide/Cargo.toml; keep in sync by hand).
  CARGO_FEATURES = %w[rendering signatures barcodes].freeze
end
