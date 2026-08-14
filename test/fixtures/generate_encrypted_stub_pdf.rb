# frozen_string_literal: true

# Regenerates test/fixtures/encrypted_stub.pdf — a minimal PDF whose trailer
# carries an RC4 encryption dictionary with dummy /O and /U strings, so it is
# recognised as encrypted but can never be decrypted. pdf_oxide's `page_count`
# surfaces this as Error::EncryptedPdf (see upstream
# tests/test_extraction_robustness.rs, "Section 2"), which the binding maps to
# PdfDioxide::PasswordError.
#
#   ruby test/fixtures/generate_encrypted_stub_pdf.rb

objects = [
  "<< /Type /Catalog /Pages 2 0 R /Encrypt 3 0 R >>",
  # /Kids and /Count both point at missing objects so the page tree cannot be
  # read at all — a literal /Count would still be readable without decryption
  # and page_count would happily return it. With the read failing on an
  # encrypted document, pdf_oxide surfaces Error::EncryptedPdf.
  "<< /Type /Pages /Kids [99 0 R] /Count 98 0 R >>",
  "<< /Filter /Standard /V 1 /R 2 /O (#{"x" * 32}) /U (#{"y" * 32}) /P -4 >>"
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
pdf << "trailer\n<< /Size #{objects.size + 1} /Root 1 0 R /Encrypt 3 0 R >>\n"
pdf << "startxref\n#{xref_offset}\n%%EOF\n"

path = File.expand_path("encrypted_stub.pdf", __dir__)
File.binwrite(path, pdf)
puts "wrote #{path} (#{pdf.bytesize} bytes)"
