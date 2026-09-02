# frozen_string_literal: true

# Regenerates test/fixtures/form_xobject_text.pdf — hand-built, dependency-free.
# Exercises the Ext method `form_xobjects_with_text`:
#
#   page 0 resources: /FmText (Form, shows text), /FmGfx (Form, graphics
#     only), /FmNested (Form with no text of its own that invokes /FmText
#     through its OWN /Resources), /Im1 (Image XObject, must be skipped),
#     /FmRefRes (Form whose /Resources is an INDIRECT reference — the
#     upstream #1309 render bug trigger)
#   page 1 resources: /FmGfx, /FmNested only
#
#   ruby test/fixtures/generate_form_xobject_pdf.rb

def stream(dict, body)
  "<< #{dict} /Length #{body.bytesize} >>\nstream\n#{body}\nendstream"
end

objects = [
  # 1 catalog / 2 pages
  "<< /Type /Catalog /Pages 2 0 R >>",
  "<< /Type /Pages /Kids [3 0 R 10 0 R] /Count 2 >>",
  # 3 page 0
  "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] " \
    "/Resources << /Font << /F1 4 0 R >> " \
    "/XObject << /FmText 5 0 R /FmGfx 6 0 R /FmNested 7 0 R /Im1 8 0 R /FmRefRes 13 0 R >> >> " \
    "/Contents 9 0 R >>",
  # 4 font
  "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
  # 5 form with text
  stream("/Type /XObject /Subtype /Form /BBox [0 0 200 50] /Resources << /Font << /F1 4 0 R >> >>",
         "BT /F1 12 Tf 10 20 Td (Hidden in form) Tj ET"),
  # 6 form, graphics only
  stream("/Type /XObject /Subtype /Form /BBox [0 0 100 100]",
         "0 0 100 100 re f"),
  # 7 nested form: no text itself, invokes FmText via its own resources
  stream("/Type /XObject /Subtype /Form /BBox [0 0 200 50] /Resources << /XObject << /Inner 5 0 R >> >>",
         "q /Inner Do Q"),
  # 8 image xobject (1x1 gray)
  stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
         "\xff"),
  # 9 page 0 content: invokes everything, plus direct page text
  stream("", "q /FmText Do Q q /FmGfx Do Q q /FmNested Do Q q /Im1 Do Q " \
              "q 1 0 0 1 72 400 cm /FmRefRes Do Q BT /F1 12 Tf 72 700 Td (Page text) Tj ET"),
  # 10 page 1: only the text-free form and the nested one
  "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] " \
    "/Resources << /XObject << /FmGfx 6 0 R /FmNested 7 0 R >> >> /Contents 11 0 R >>",
  # 11 page 1 content
  stream("", "q /FmGfx Do Q q /FmNested Do Q"),
  # 12 a Resources dictionary as its own indirect object; the font name
  #    /FRef exists ONLY here (the page has /F1), so the Form's text can be
  #    drawn only if this dictionary is actually loaded
  "<< /Font << /FRef 4 0 R >> >>",
  # 13 ... referenced by this Form: the upstream #1309 trigger (text not
  #    rendered because /Resources is a Reference, not a direct dict)
  stream("/Type /XObject /Subtype /Form /BBox [0 0 300 60] /Resources 12 0 R",
         "BT /FRef 24 Tf 10 20 Td (REF RESOURCES TEXT) Tj ET")
]

pdf = +"%PDF-1.4\n"
pdf.force_encoding(Encoding::BINARY)
offsets = objects.map.with_index(1) do |body, num|
  offset = pdf.bytesize
  pdf << "#{num} 0 obj\n".b << body.b << "\nendobj\n".b
  offset
end
xref_offset = pdf.bytesize
pdf << "xref\n0 #{objects.size + 1}\n".b
pdf << "0000000000 65535 f \n".b
offsets.each { |o| pdf << format("%010d 00000 n \n", o).b }
pdf << "trailer\n<< /Size #{objects.size + 1} /Root 1 0 R >>\nstartxref\n#{xref_offset}\n%%EOF\n".b

path = File.expand_path("form_xobject_text.pdf", __dir__)
File.binwrite(path, pdf)
puts "wrote #{path} (#{pdf.bytesize} bytes)"
