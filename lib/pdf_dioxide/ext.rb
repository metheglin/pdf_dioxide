# frozen_string_literal: true

require "pdf_dioxide"

# pdf_dioxide-only extensions ("Ext"). NOT part of the python.rs parity
# surface — loading this file is the explicit opt-in that attaches them.
# Policy: .claude/rules/extensions.md
#
# What Ext provides is the layer upstream keeps closed in every binding:
# the PDF object graph and content streams as data, so an application can
# write its own structural checks. Example — "does any Form XObject on the
# page draw text?":
#
#   require "pdf_dioxide/ext"
#   page   = doc.page_object(0)
#   xobjs  = doc.resolve(doc.resolve(page["Resources"])["XObject"]) || {}
#   xobjs.any? do |_name, ref|
#     doc.form_xobject?(ref) &&
#       PdfDioxide::Ext.content_operators(doc.stream_data(ref), text_only: true)
#                       .any? { |op, *| %w[Tj TJ ' "].include?(op) }
#   end
module PdfDioxide
  module Ext
    # Registry — the single source of truth for what Ext provides.
    # kind:   :method | :override | :class | :function
    # attach: :include | :prepend | :module_function
    # status: :extension | :graduated | :removed
    FEATURES = [
      { name: :ObjectRef, kind: :class, attach: :module_function,
        rust_api: "object::ObjectRef", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :Stream, kind: :class, attach: :module_function,
        rust_api: "Object::Stream + Object::decode_stream_data", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :content_operators, kind: :function, attach: :module_function,
        rust_api: "content::parser::{parse_content_stream, parse_content_stream_text_only}",
        since: "0.1.1", upstream_issue: "not yet filed", status: :extension,
        graduated_in_upstream: nil },
      { name: :page_object, kind: :method, attach: :include,
        rust_api: "PdfDocument::get_page", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :catalog_object, kind: :method, attach: :include,
        rust_api: "PdfDocument::catalog", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :trailer_object, kind: :method, attach: :include,
        rust_api: "PdfDocument::trailer", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :load_object, kind: :method, attach: :include,
        rust_api: "PdfDocument::load_object", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :resolve, kind: :method, attach: :include,
        rust_api: "(pure Ruby over load_object)", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :"form_xobject?", kind: :method, attach: :include,
        rust_api: "PdfDocument::is_form_xobject", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :page_content, kind: :method, attach: :include,
        rust_api: "PdfDocument::get_page_content_data", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      { name: :stream_data, kind: :method, attach: :include,
        rust_api: "PdfDocument::load_object + Object::decode_stream_data", since: "0.1.1",
        upstream_issue: "not yet filed", status: :extension, graduated_in_upstream: nil },
      # GRADUATED in upstream 0.3.78: render_page resolves an indirect
      # /Resources itself, so the fork patch and this method are obsolete.
      # Deprecating shim; delete one minor release after 0.1.2.
      { name: :experimental_render_page, kind: :method, attach: :include,
        rust_api: "(shim) delegates to the parity PdfDocument#render_page",
        since: "0.1.1", upstream_issue: "https://github.com/yfedoseev/pdf_oxide/issues/1309",
        status: :graduated, graduated_in_upstream: "0.3.78" }
    ].freeze

    def self.features = FEATURES

    # One deprecation notice per process per graduated name.
    def self.warn_graduated(old_name, parity_name)
      @warned ||= {}
      return if @warned[old_name]

      @warned[old_name] = true
      warn "PdfDioxide::Ext##{old_name} has graduated into the parity API: " \
           "use PdfDioxide::PdfDocument##{parity_name} instead. " \
           "The shim will be removed in a later release."
    end

    # Value semantics so refs work as Hash keys / in Sets (visited tracking).
    class ObjectRef
      def ==(other) = other.is_a?(ObjectRef) && id == other.id && gen == other.gen
      alias eql? ==
      def hash = [id, gen].hash
    end

    # Native methods live on this module; Ruby adds the conveniences.
    module Document
      # Follow an indirect reference; anything else passes through unchanged.
      # `doc.resolve(page["Resources"])` works whether Resources is inline or
      # a reference.
      def resolve(obj) = obj.is_a?(ObjectRef) ? load_object(obj) : obj

      # Deprecated: upstream 0.3.78 fixed the Form XObject /Resources bug
      # (issue #1309) in `render_page` itself, so this is now a plain
      # delegation kept for one release.
      def experimental_render_page(page, **opts)
        Ext.warn_graduated(:experimental_render_page, :render_page)
        render_page(page, **opts)
      end
    end
  end

  # `include` (not `prepend`): all names are new, so nothing can shadow a
  # parity method.
  PdfDocument.include Ext::Document
end
