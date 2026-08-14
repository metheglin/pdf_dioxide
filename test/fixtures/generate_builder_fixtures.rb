# frozen_string_literal: true

# Regenerates the DocumentBuilder-produced fixtures. Unlike the hand-built
# hello.pdf / encrypted_stub.pdf generators, these use the gem itself
# (compile first: `bundle exec rake compile`), which keeps them in sync with
# whatever the writer emits.
#
#   bundle exec ruby test/fixtures/generate_builder_fixtures.rb
#
# Produces:
#   multipage.pdf   — 3 pages of plain text; for page iteration / page-ops /
#                     split / merge tests.
#   form_fields.pdf — text field + checkbox + combo box + annotations
#                     (link, highlight, sticky note, watermark) and a table;
#                     for get_form_fields / get_annotations / extract_tables
#                     tests.

$LOAD_PATH.unshift File.expand_path("../../lib", __dir__)
require "pdf_dioxide"

dir = __dir__

# --- multipage.pdf ----------------------------------------------------------
PdfDioxide::DocumentBuilder.new
  .title("Multipage Fixture")
  .author("pdf_dioxide")
  .a4_page
    .font("Helvetica", 14.0)
    .at(72, 770).text("Page one")
    .paragraph("First page body text.")
    .done
  .a4_page
    .at(72, 770).text("Page two")
    .paragraph("Second page body text.")
    .done
  .a4_page
    .at(72, 770).text("Page three")
    .paragraph("Third page body text.")
    .done
  .save(File.join(dir, "multipage.pdf"))
puts "wrote multipage.pdf"

# --- form_fields.pdf --------------------------------------------------------
col = PdfDioxide::Column
table = PdfDioxide::Table.new(
  [col.new("Item", 150.0), col.new("Qty", 60.0, :right)],
  [["Apple", "3"], ["Banana", "12"]],
  true
)

PdfDioxide::DocumentBuilder.new
  .title("Form Fields Fixture")
  .a4_page
    .font("Helvetica", 12.0)
    .at(72, 780).text("Form fixture")
    .text_field("name_field", 72, 700, 200, 24, "prefilled")
    .checkbox("agree", 72, 660, 16, 16, true)
    .combo_box("color", 72, 620, 140, 24, %w[red green blue], "green")
    .at(72, 580).link_url("https://example.com")
    .highlight([1.0, 1.0, 0.0])
    .sticky_note_at(400, 700, "reviewer note")
    .watermark("FIXTURE")
    .at(72, 540).table(table)
    .done
  .save(File.join(dir, "form_fields.pdf"))
puts "wrote form_fields.pdf"
