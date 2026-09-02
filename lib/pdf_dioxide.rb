# frozen_string_literal: true

require_relative "pdf_dioxide/version"
require "pdf_dioxide/pdf_dioxide"

# Single gem namespace (RubyGems convention: gem name pdf_dioxide <->
# constant PdfDioxide): VERSION lives in version.rb, the native extension
# defines the core classes in `#[magnus::init]`
# (see ext/pdf_dioxide/src/lib.rs), and the pure-Ruby layer below reopens
# the module to add the delegation classes and sugar.
#
# API shape mirrors the Rust crate and the Python bindings.
#
#   doc = PdfDioxide::PdfDocument.open("report.pdf")
#   doc.page_count
#   doc.extract_text(0)
#   doc.each { |page| puts page.text }
#
# Errors raise subclasses of PdfDioxide::Error (IoError, ParseError,
# PasswordError, UnsupportedError), all defined by the native extension.
#
# The delegation classes below ({PdfDioxide::Page}, {PdfDioxide::PdfPageRegion})
# are pure Ruby: their Python counterparts (`Page`, `PdfPageRegion` in
# python.rs) exist only to hold a document reference plus a page index /
# region, forwarding every call back to the document — which Ruby expresses
# naturally without native code (and without native-side GC bookkeeping for
# the back-reference).
module PdfDioxide
  # One-stop version report for bug reports and CI logs: gem version,
  # the pdf_oxide crate actually linked (UPSTREAM_VERSION, set natively),
  # the python.rs parity baseline, Cargo features, and — only when
  # `pdf_dioxide/ext` has been required — the Ext registry.
  def self.version_info
    {
      gem: VERSION,
      upstream_crate: UPSTREAM_VERSION,
      parity: PARITY,
      cargo_features: CARGO_FEATURES,
      extensions: Ext.respond_to?(:features) ? Ext.features : []
    }
  end

  # Horizontal alignment values accepted by {Column} (python.rs's `Align`
  # int-enum pyclass, expressed as plain Ruby constants; `Column.new` also
  # accepts :left / :center / :right symbols).
  module Align
    LEFT = 0
    CENTER = 1
    RIGHT = 2
  end

  # A lightweight per-page view over a {PdfDocument}, yielded by
  # {PdfDocument#each} / {PdfDocument#[]}. Mirrors python.rs's `Page`.
  class Page
    attr_reader :index

    def initialize(doc, index)
      @doc = doc
      @index = index
    end

    # MediaBox as `[llx, lly, urx, ury]`.
    def bbox = @doc.page_media_box(@index)

    def width
      b = bbox
      b[2] - b[0]
    end

    def height
      b = bbox
      b[3] - b[1]
    end

    def text = @doc.extract_text(@index)
    def chars = @doc.extract_chars(@index)
    def words = @doc.extract_words(@index)
    def lines = @doc.extract_text_lines(@index)
    def spans = @doc.extract_spans(@index)
    def tables = @doc.extract_tables(@index)
    def images = @doc.extract_images(@index)
    def annotations = @doc.get_annotations(@index)
    def paths = @doc.extract_paths(@index)

    def markdown(**opts) = @doc.to_markdown(@index, **opts)
    def plain_text(**opts) = @doc.to_plain_text(@index, **opts)
    def html(**opts) = @doc.to_html(@index, **opts)
    def render(**opts) = @doc.render_page(@index, **opts)
    def render_pixmap(**opts) = @doc.render_pixmap(@index, **opts)

    # Unlike PdfDocument#search (unlimited), Python's Page.search defaults
    # to max_results=100; keep that here.
    def search(pattern, max_results: 100, **opts)
      @doc.search_page(@index, pattern, max_results: max_results, **opts)
    end

    def region(x, y, width, height)
      PdfPageRegion.new(@doc, @index, [x, y, width, height])
    end

    def inspect = "#<PdfDioxide::Page index=#{@index}>"
  end

  # Extraction scoped to a rectangular region of one page, from
  # {PdfDocument#within} / {Page#region}. Mirrors python.rs's `PdfPageRegion`.
  class PdfPageRegion
    attr_reader :page_index, :bbox

    def initialize(doc, page_index, bbox)
      @doc = doc
      @page_index = page_index
      @bbox = bbox
    end

    def extract_text = @doc.extract_text(@page_index, region: @bbox)
    def extract_words = @doc.extract_words(@page_index, region: @bbox)
    def extract_text_lines = @doc.extract_text_lines(@page_index, region: @bbox)
    def extract_images = @doc.extract_images(@page_index, region: @bbox)
    def extract_paths = @doc.extract_paths(@page_index, region: @bbox)

    def extract_tables(table_settings: nil)
      if table_settings
        @doc.extract_tables(@page_index, region: @bbox, table_settings: table_settings)
      else
        @doc.extract_tables(@page_index, region: @bbox)
      end
    end

    def inspect = "#<PdfDioxide::PdfPageRegion page=#{@page_index} bbox=#{@bbox.inspect}>"
  end

  # Buffered page builder for {DocumentBuilder} — the Ruby counterpart of
  # python.rs's `FluentPageBuilder`. The Rust `FluentPageBuilder<'a>` borrows
  # its `DocumentBuilder`, which a GC'd wrapper can't hold, so (like the
  # Python binding) operations are buffered — here as plain
  # `[op_name, *args]` arrays — and committed in one shot by {#done}, which
  # replays them through the native `DocumentBuilder#_apply_page`.
  #
  #   PdfDioxide::DocumentBuilder.new
  #     .title("Hello")
  #     .a4_page
  #       .font("Helvetica", 12.0)
  #       .at(72.0, 720.0).text("Hello!")
  #       .done
  #     .build
  class FluentPageBuilder
    PAGE_HEIGHTS = { a4: 842.0, letter: 792.0 }.freeze
    private_constant :PAGE_HEIGHTS

    def initialize(builder, page_spec)
      @builder = builder
      @page_spec = page_spec
      @ops = []
      @done = false
      @current_font = "Helvetica"
      @current_size = 12.0
      @last_y = nil
    end

    # -- cursor / text ------------------------------------------------------

    def font(name, size)
      @current_font = name
      @current_size = size
      push("font", name, size)
    end

    def at(x, y)
      @last_y = y
      push("at", x, y)
    end

    def text(text) = push("text", text)
    def heading(level, text) = push("heading", level, text)
    def paragraph(text) = push("paragraph", text)
    def space(points) = push("space", points)
    def horizontal_rule = push("horizontal_rule")
    def newline = push("newline")
    def inline(text) = push("inline", text)
    def inline_bold(text) = push("inline_bold", text)
    def inline_italic(text) = push("inline_italic", text)
    def inline_color(r, g, b, text) = push("inline_color", r, g, b, text)
    def columns(count, gap_pt, text) = push("columns", count, gap_pt, text)
    def footnote(ref_mark, note_text) = push("footnote", ref_mark, note_text)

    def text_in_rect(x, y, w, h, text, align = nil)
      push("text_in_rect", x, y, w, h, text, resolve_align(align))
    end

    # -- links / actions ----------------------------------------------------

    def link_url(url) = push("link_url", url)
    def link_page(page) = push("link_page", page)
    def link_named(dest) = push("link_named", dest)
    def link_javascript(script) = push("link_javascript", script)
    def on_open(script) = push("on_open", script)
    def on_close(script) = push("on_close", script)
    def field_keystroke(script) = push("field_keystroke", script)
    def field_format(script) = push("field_format", script)
    def field_validate(script) = push("field_validate", script)
    def field_calculate(script) = push("field_calculate", script)

    # -- annotations --------------------------------------------------------

    def highlight(color) = push("highlight", *rgb(color))
    def underline(color) = push("underline", *rgb(color))
    def strikeout(color) = push("strikeout", *rgb(color))
    def squiggly(color) = push("squiggly", *rgb(color))
    def sticky_note(text) = push("sticky_note", text)
    def sticky_note_at(x, y, text) = push("sticky_note_at", x, y, text)
    def watermark(text) = push("watermark", text)
    def watermark_confidential = push("watermark_confidential")
    def watermark_draft = push("watermark_draft")
    def stamp(name) = push("stamp", name)
    def freetext(x, y, w, h, text) = push("freetext", x, y, w, h, text)

    # -- form fields --------------------------------------------------------

    def text_field(name, x, y, w, h, default_value = nil)
      push("text_field", name, x, y, w, h, default_value)
    end

    def checkbox(name, x, y, w, h, checked)
      push("checkbox", name, x, y, w, h, checked)
    end

    def combo_box(name, x, y, w, h, options, selected = nil)
      push("combo_box", name, x, y, w, h, options, selected)
    end

    # `buttons` is an Array of `[label, x, y, w, h]`.
    def radio_group(name, buttons, selected = nil)
      push("radio_group", name, buttons, selected)
    end

    def push_button(name, x, y, w, h, caption)
      push("push_button", name, x, y, w, h, caption)
    end

    def signature_field(name, x, y, w, h)
      push("signature_field", name, x, y, w, h)
    end

    # -- graphics -----------------------------------------------------------

    def rect(x, y, w, h) = push("rect", x, y, w, h)

    def filled_rect(x, y, w, h, r, g, b) = push("filled_rect", x, y, w, h, r, g, b)

    def line(x1, y1, x2, y2) = push("line", x1, y1, x2, y2)

    def stroke_rect(x, y, w, h, width: 1.0, color: [0.0, 0.0, 0.0])
      push("stroke_rect", x, y, w, h, width, *rgb(color))
    end

    def stroke_rect_dashed(x, y, w, h, dash, width: 1.0, color: [0.0, 0.0, 0.0], phase: 0.0)
      push("stroke_rect_dashed", x, y, w, h, width, *rgb(color), dash, phase)
    end

    def stroke_line(x1, y1, x2, y2, width: 1.0, color: [0.0, 0.0, 0.0])
      push("stroke_line", x1, y1, x2, y2, width, *rgb(color))
    end

    def stroke_line_dashed(x1, y1, x2, y2, dash, width: 1.0, color: [0.0, 0.0, 0.0], phase: 0.0)
      push("stroke_line_dashed", x1, y1, x2, y2, width, *rgb(color), dash, phase)
    end

    # -- images / barcodes --------------------------------------------------

    # Barcodes render at record time so errors surface here, not in done.
    def barcode_1d(barcode_type, data, x, y, w, h)
      bytes = PdfDioxide._render_barcode_1d(barcode_type, data, w, h)
      push("image", bytes, x, y, w, h)
    end

    def barcode_qr(data, x, y, size)
      bytes = PdfDioxide._render_barcode_qr(data, size)
      push("image", bytes, x, y, size, size)
    end

    def image_with_alt(bytes, x, y, w, h, alt_text)
      push("image_with_alt", bytes, x, y, w, h, alt_text)
    end

    def image_artifact(bytes, x, y, w, h)
      push("image_artifact", bytes, x, y, w, h)
    end

    # -- measurement --------------------------------------------------------

    # Text width in points for the current font/size (base-14 metrics).
    def measure(text) = PdfDioxide._measure_text(text, @current_font, @current_size)

    # Client-side estimate of vertical space left above the 72pt bottom
    # margin (see python.rs for the caveats).
    def remaining_space
      page_height =
        case @page_spec[0]
        when "a4" then PAGE_HEIGHTS[:a4]
        when "letter" then PAGE_HEIGHTS[:letter]
        else @page_spec[2]
        end
      y = @last_y || (page_height - 72.0)
      [y - 72.0, 0.0].max
    end

    # -- tables -------------------------------------------------------------

    def table(table)
      raise ArgumentError, "expected PdfDioxide::Table" unless table.is_a?(PdfDioxide::Table)

      push("_table_object", table)
    end

    def streaming_table(columns, repeat_header: false, mode: "fixed", sample_rows: 50,
                        min_col_width_pt: 20.0, max_col_width_pt: 400.0,
                        max_rowspan: 1, batch_size: 256)
      raise ArgumentError, "streaming_table requires at least one Column" if columns.empty?
      raise ArgumentError, "batch_size must be >= 1" if batch_size < 1

      StreamingTable.new(
        self, columns,
        repeat_header: repeat_header, mode: mode, sample_rows: sample_rows,
        min_col_width_pt: min_col_width_pt, max_col_width_pt: max_col_width_pt,
        max_rowspan: max_rowspan, batch_size: batch_size
      )
    end

    # Start a fresh page of the same size mid-chain.
    def new_page_same_size = push("new_page_same_size")

    # Commit the buffered operations to the parent DocumentBuilder and
    # return it for further chaining. Single-use.
    def done
      raise PdfDioxide::Error, "FluentPageBuilder#done already called" if @done

      @done = true
      ops = @ops.map { |op| op[0] == "_table_object" ? lower_table(op[1]) : op }
      @builder.send(:_apply_page, @page_spec, ops)
    end

    # @api private — StreamingTable#finish pushes its buffered rows here.
    def _push_op(*op)
      push(*op)
      nil
    end

    def inspect = "#<PdfDioxide::FluentPageBuilder ops=#{@ops.size}#{@done ? " (done)" : ""}>"

    private

    def push(*op)
      raise PdfDioxide::Error, "FluentPageBuilder#done already called" if @done

      @ops << op
      self
    end

    def rgb(color)
      values = color.is_a?(PdfDioxide::Color) ? [color.r, color.g, color.b] : color.to_a
      raise ArgumentError, "color must be [r, g, b]" unless values.size == 3

      values
    end

    def resolve_align(align)
      case align
      when nil then 0
      when Integer then align
      when String, Symbol
        { "left" => 0, "l" => 0, "center" => 1, "centre" => 1, "c" => 1,
          "right" => 2, "r" => 2 }.fetch(align.to_s.downcase) do
          raise ArgumentError, "invalid align #{align.inspect}"
        end
      else
        raise ArgumentError, "invalid align #{align.inspect}"
      end
    end

    # Lower a Table value object into the raw ["table", ...] op. When
    # has_header, the Columns' own header strings become a synthetic first
    # row (matching python.rs).
    def lower_table(t)
      columns = t.instance_variable_get(:@__columns) || begin
        # Native RbTable doesn't expose members; carry them via marshaling
        # from Column objects passed at construction — see Table patch below.
        raise PdfDioxide::Error, "Table object missing column data"
      end
      rows = t.instance_variable_get(:@__rows)
      has_header = t.instance_variable_get(:@__has_header)
      widths = columns.map(&:width)
      aligns = columns.map(&:align)
      all_rows = has_header ? [columns.map(&:header)] + rows : rows
      ["table", widths, aligns, all_rows, has_header]
    end
  end

  # Streaming-table handle from {FluentPageBuilder#streaming_table}: push
  # rows batch-by-batch; {#finish} attaches the buffered rows to the page
  # (rendered by the Rust `StreamingTable` core at `done` time).
  class StreamingTable
    def initialize(page, columns, repeat_header:, mode:, sample_rows:,
                   min_col_width_pt:, max_col_width_pt:, max_rowspan:, batch_size:)
      @page = page
      @columns = columns
      @repeat_header = repeat_header
      @mode = mode
      @sample_rows = sample_rows
      @min_col_width_pt = min_col_width_pt
      @max_col_width_pt = max_col_width_pt
      @max_rowspan = max_rowspan
      @batch_size = batch_size
      @current_batch = []
      @completed_batches = []
      @finished = false
    end

    # Push a row of string cells (all rowspan = 1).
    def push_row(cells)
      check_row!(cells)
      @current_batch << cells.map { |c| [c, 1] }
      flush if @current_batch.size >= @batch_size
      nil
    end

    # Push a row of `[text, rowspan]` pairs.
    def push_row_span(cells)
      check_row!(cells)
      @current_batch << cells.map { |(text, span)| [text, span] }
      flush if @current_batch.size >= @batch_size
      nil
    end

    def column_count = @columns.size
    def pending_row_count = @current_batch.size
    def batch_count = @completed_batches.size

    def flush
      @completed_batches << @current_batch unless @current_batch.empty?
      @current_batch = []
      nil
    end

    # Close the table and return the parent FluentPageBuilder.
    def finish
      raise PdfDioxide::Error, "StreamingTable#finish already called" if @finished

      flush
      @finished = true
      @page._push_op(
        "streaming_table",
        @columns.map(&:header), @columns.map(&:width), @columns.map(&:align),
        @repeat_header, @completed_batches.flatten(1),
        @mode, @sample_rows, @min_col_width_pt, @max_col_width_pt, @max_rowspan
      )
      @page
    end

    def inspect = "#<PdfDioxide::StreamingTable columns=#{@columns.size} batches=#{@completed_batches.size}>"

    private

    def check_row!(cells)
      raise PdfDioxide::Error, "StreamingTable#finish already called" if @finished
      return if cells.size == @columns.size

      raise ArgumentError, "row has #{cells.size} cells, expected #{@columns.size}"
    end
  end

  # Page-builder entry points for the native DocumentBuilder (python.rs's
  # `a4_page` / `letter_page` / `page`).
  class DocumentBuilder
    def a4_page = FluentPageBuilder.new(self, ["a4"])
    def letter_page = FluentPageBuilder.new(self, ["letter"])
    def page(width, height) = FluentPageBuilder.new(self, ["custom", width, height])
  end

  # Keep the Column/Table members visible to FluentPageBuilder#table: the
  # native Table doesn't re-expose its rows, so remember them Ruby-side.
  class Table
    class << self
      alias _native_new new

      def new(columns, rows, has_header = false)
        instance = _native_new(columns, rows, has_header)
        instance.instance_variable_set(:@__columns, columns)
        instance.instance_variable_set(:@__rows, rows)
        instance.instance_variable_set(:@__has_header, has_header)
        instance
      end
    end
  end

  # Ruby-flavored counterparts of python.rs's `__len__` / `__getitem__` /
  # `__iter__` / `pages` / `within` / `__repr__` — plus block-form `open`
  # standing in for Python's `__enter__` / `__exit__` context manager.
  class PdfDocument
    include Enumerable

    class << self
      alias _native_open open

      # `PdfDocument.open(path)` returns the document; with a block, yields
      # it and returns the block's result (File.open-style; the native side
      # frees resources at GC).
      def open(path, password = nil)
        doc = password ? _native_open(path, password) : _native_open(path)
        return doc unless block_given?

        yield doc
      end
    end

    def length = page_count
    alias size length

    # `doc[0]`, `doc[-1]` — negative indices count from the end. Raises
    # IndexError out of range, like Python's `__getitem__`.
    def [](index)
      count = page_count
      idx = index.negative? ? count + index : index
      raise IndexError, "page index out of range" if idx.negative? || idx >= count

      Page.new(self, idx)
    end

    # Yields a {Page} per page. Without a block returns an Enumerator, so
    # `doc.map(&:text)`, `doc.first(3)`, ... all work via Enumerable.
    def each
      return enum_for(:each) unless block_given?

      page_count.times { |i| yield Page.new(self, i) }
      self
    end
    alias each_page each

    # Explicitly-named page iterator (python.rs `doc.pages`).
    def pages = enum_for(:each)

    # Focus extraction on a page region: `doc.within(0, [x, y, w, h])`.
    def within(page, bbox) = PdfPageRegion.new(self, page, bbox)

    def inspect = "#<PdfDioxide::PdfDocument version=#{version.join(".")}>"
  end
end
