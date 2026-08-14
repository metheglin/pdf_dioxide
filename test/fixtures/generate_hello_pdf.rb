# frozen_string_literal: true

# Regenerates test/fixtures/hello.pdf — a hand-built, dependency-free PDF 1.4
# file with a single page carrying one Helvetica text run. Kept in the repo so
# the fixture can be rebuilt/inspected instead of being an opaque binary.
#
#   ruby test/fixtures/generate_hello_pdf.rb

TEXT = "Hello from pdf_oxide"

content = <<~STREAM
  BT
  /F1 24 Tf
  72 700 Td
  (#{TEXT}) Tj
  ET
STREAM

objects = [
  "<< /Type /Catalog /Pages 2 0 R >>",
  "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
  "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] " \
    "/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
  "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
  "<< /Length #{content.bytesize} >>\nstream\n#{content}endstream"
]

pdf = +"%PDF-1.4\n"
offsets = objects.map.with_index(1) do |body, num|
  offset = pdf.bytesize
  pdf << "#{num} 0 obj\n#{body}\nendobj\n"
  offset
end

xref_offset = pdf.bytesize
pdf << "xref\n0 #{objects.size + 1}\n"
pdf << "0000000000 65535 f \n"
offsets.each { |offset| pdf << format("%010d 00000 n \n", offset) }
pdf << "trailer\n<< /Size #{objects.size + 1} /Root 1 0 R >>\n"
pdf << "startxref\n#{xref_offset}\n%%EOF\n"

path = File.expand_path("hello.pdf", __dir__)
File.binwrite(path, pdf)
puts "wrote #{path} (#{pdf.bytesize} bytes)"
