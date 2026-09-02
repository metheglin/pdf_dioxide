# frozen_string_literal: true

module PdfDioxide
  # Tracking mode (see .claude/rules/versioning.md): the 0.1.x series means
  # "built against upstream main, not tied to a release". Switches to the
  # four-segment <upstream>.<build> scheme when the gem enters pinned mode.
  VERSION = "0.1.1"

  # The upstream commit this build was reconciled against (unreleased main;
  # the version part is the crate version at that commit).
  PARITY = { version: "0.3.77+main@3be1951", commit: "3be1951b171edb9d69a10f42ef72ee73f52e51bf" }.freeze

  # Cargo features the extension is built with (mirror of
  # ext/pdf_dioxide/Cargo.toml; keep in sync by hand).
  CARGO_FEATURES = %w[rendering signatures barcodes].freeze
end
