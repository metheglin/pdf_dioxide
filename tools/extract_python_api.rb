# frozen_string_literal: true

# Extracts the portable API surface from pdf_oxide's src/python.rs:
#   - #[pyclass] structs/enums (with their Python-visible name)
#   - #[pymethods] items: methods, getters, setters, static/class methods, #[new]
#   - #[pyfunction] module-level functions
# Emits CSV rows: kind,container,rust_name,python_name,line,ported,tested

require "csv"

SRC = ARGV[0] or abort "usage: extract_python_api.rb <python.rs> <out.csv>"
OUT = ARGV[1] or abort "usage: extract_python_api.rb <python.rs> <out.csv>"

lines = File.readlines(SRC, encoding: "UTF-8")

rows = []

# ---- pass 1: pyclass declarations -----------------------------------------
# #[pyclass(...)] may span lines; collect attr text until the struct/enum line.
i = 0
while i < lines.size
  line = lines[i]
  if line =~ /^\s*#\[pyclass/
    attr_text = +""
    j = i
    while j < lines.size
      attr_text << lines[j]
      break if lines[j] =~ /\]\s*$/ && attr_text.count("(") <= attr_text.count(")")
      j += 1
    end
    # find the struct/enum after remaining attributes/doc lines
    k = j + 1
    k += 1 while k < lines.size && lines[k] !~ /^\s*(pub\s+)?(struct|enum)\s+\w+/
    if k < lines.size && lines[k] =~ /^\s*(?:pub\s+)?(?:struct|enum)\s+(\w+)/
      rust_name = Regexp.last_match(1)
      py_name = attr_text[/name\s*=\s*"([^"]+)"/, 1] || rust_name
      rows << ["class", "", rust_name, py_name, k + 1]
    end
    i = k
  end
  i += 1
end

class_by_rust = rows.select { |r| r[0] == "class" }.to_h { |r| [r[2], r[3]] }

# ---- pass 2: pymethods blocks and pyfunctions -----------------------------
current_impl = nil
impl_depth = 0
depth = 0
pending_attrs = []

lines.each_with_index do |line, idx|
  lineno = idx + 1
  stripped = line.strip

  if stripped =~ /^#\[pymethods\]/
    pending_attrs = [:pymethods]
    next
  end

  if pending_attrs.include?(:pymethods) && stripped =~ /^impl\s+(\w+)/
    current_impl = Regexp.last_match(1)
    impl_depth = depth
    pending_attrs = []
  end

  # track method-level attributes inside an impl (reset on fn)
  if current_impl
    case stripped
    when /^#\[(getter|setter|staticmethod|classmethod|new)\b/
      pending_attrs << Regexp.last_match(1).to_sym
    when /^#\[pyo3\([^)]*name\s*=\s*"([^"]+)"/
      pending_attrs << [:pyname, Regexp.last_match(1)]
    end

    if stripped =~ /^(?:pub\s+)?fn\s+(\w+)/
      rust_name = Regexp.last_match(1)
      kind =
        if pending_attrs.include?(:new) then "constructor"
        elsif pending_attrs.include?(:getter) then "getter"
        elsif pending_attrs.include?(:setter) then "setter"
        elsif pending_attrs.include?(:staticmethod) || pending_attrs.include?(:classmethod) then "static_method"
        else "method"
        end
      pyname_pair = pending_attrs.find { |a| a.is_a?(Array) && a[0] == :pyname }
      py_name = pyname_pair ? pyname_pair[1] : rust_name
      container = class_by_rust.fetch(current_impl, current_impl)
      rows << [kind, container, rust_name, py_name, lineno]
      pending_attrs = []
    end
  end

  if stripped =~ /^#\[pyfunction/
    pending_attrs = [:pyfunction]
  elsif pending_attrs.include?(:pyfunction) && stripped =~ /^(?:pub\s+)?fn\s+(\w+)/
    rows << ["function", "", Regexp.last_match(1), Regexp.last_match(1), lineno]
    pending_attrs = []
  end

  # brace tracking to know when an impl block ends
  depth += line.count("{") - line.count("}")
  current_impl = nil if current_impl && depth <= impl_depth && line.include?("}")
end

# ---- mark what the Ruby binding already has -------------------------------
DONE = {
  # container(py name) / rust_name => [ported, tested]
  # Python's PdfDocument(path) constructor == Ruby's PdfOxide::PdfDocument.open
  %w[PdfDocument new] => [true, true],
  %w[PdfDocument extract_text] => [true, true],
  %w[PdfDocument page_count] => [true, true],
}.freeze

# Preserve progress already recorded in an existing CSV: a row once marked
# true stays true across regenerations (line numbers may shift with upstream),
# and any comment carries forward verbatim.
existing = {}
if File.exist?(OUT)
  CSV.foreach(OUT, headers: true, encoding: "UTF-8") do |row|
    # `.to_s`: an empty container may round-trip as "" or nil depending on
    # how the CSV was last written (quoted vs bare empty field); the merge
    # key must not care.
    existing[[row["container"].to_s, row["rust_name"].to_s]] =
      [row["ported"] == "true", row["tested"] == "true", row["comment"].to_s]
  end
end

CSV.open(OUT, "w", encoding: "UTF-8") do |csv|
  csv << %w[kind container rust_name python_name python_rs_line ported tested comment]
  rows.sort_by { |r| r[4] }.each do |kind, container, rust_name, py_name, lineno|
    ported, tested = DONE.fetch([container, rust_name], [false, false])
    old_ported, old_tested, comment = existing.fetch([container.to_s, rust_name.to_s], [false, false, ""])
    csv << [kind, container, rust_name, py_name, lineno,
            ported || old_ported, tested || old_tested, comment]
  end
end

puts "#{rows.size} rows -> #{OUT}"
kinds = rows.group_by(&:first).transform_values(&:size)
puts kinds.map { |k, v| "#{k}: #{v}" }.join(", ")
