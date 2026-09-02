//! pdf_dioxide-only extensions — the "Ext" surface.
//!
//! Nothing in this file exists in upstream's `src/python.rs` (nor in the C
//! ABI / WASM / JNI bindings). Upstream exposes only pre-digested results;
//! this module exposes the two layers underneath them so a Ruby application
//! can write its own structural checks:
//!
//! 1. the PDF object graph (`page_object` / `catalog_object` / `load_object`
//!    / `Stream#data`) and
//! 2. content streams as operator lists (`Ext.content_operators`).
//!
//! Every item is registered on a `PdfDioxide::Ext::*` module (never directly
//! on a parity class); `lib/pdf_dioxide/ext.rb` decides how it attaches. See
//! `.claude/rules/extensions.md` for the rules and the graduation process.

use magnus::{
    function, method,
    prelude::*,
    scan_args::{get_kwargs, scan_args},
    Error, IntoValue, RArray, RHash, RModule, RString, Ruby, Value,
};
use pdf_oxide::content::parser::{parse_content_stream, parse_content_stream_text_only};
use pdf_oxide::content::{Operator, TextElement};
use pdf_oxide::object::{Object, ObjectRef};
use pdf_oxide::PdfDocument;

use crate::{map_pdf_error, RbPdfDocument, UNSUPPORTED_ERROR};

// ---------------------------------------------------------------------------
// PDF object model -> Ruby
// ---------------------------------------------------------------------------
//
// Mapping (chosen for pattern-matching ergonomics on the Ruby side):
//   Null -> nil, Boolean -> true/false, Integer -> Integer, Real -> Float,
//   String -> binary String (PDF strings are byte strings),
//   Name -> Symbol (`:Form`), Array -> Array,
//   Dictionary -> Hash with String keys (`dict["Subtype"]`),
//   Stream -> PdfDioxide::Ext::Stream, Reference -> PdfDioxide::Ext::ObjectRef.

/// EXT: `PdfDioxide::Ext::ObjectRef` — an indirect reference (`5 0 R`).
/// Rust core: `pdf_oxide::object::ObjectRef`. Upstream: not yet filed.
#[magnus::wrap(class = "PdfDioxide::Ext::ObjectRef", free_immediately, size)]
#[derive(Clone)]
struct RbObjectRef(ObjectRef);

impl RbObjectRef {
    fn new(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(u32,), (Option<u16>,), (), (), (), ()>(args)?;
        let (id,) = args.required;
        let (gen,) = args.optional;
        let _ = ruby;
        Ok(Self(ObjectRef {
            id,
            gen: gen.unwrap_or(0),
        }))
    }
    fn id(&self) -> u32 {
        self.0.id
    }
    fn gen(&self) -> u16 {
        self.0.gen
    }
    fn to_s(&self) -> String {
        format!("{} {} R", self.0.id, self.0.gen)
    }
    fn inspect(&self) -> String {
        format!("#<PdfDioxide::Ext::ObjectRef {} {} R>", self.0.id, self.0.gen)
    }
}

/// EXT: `PdfDioxide::Ext::Stream` — a stream object: `#dict` (Hash),
/// `#data` (filters decoded), `#raw_data` (as stored in the file).
/// Rust core: `Object::Stream` + `Object::decode_stream_data`.
/// Encrypted documents: `#data` raises `PdfDioxide::UnsupportedError`
/// because stream decryption is `pub(crate)` upstream.
#[magnus::wrap(class = "PdfDioxide::Ext::Stream", free_immediately, size)]
struct RbStream {
    obj: Object,
    encrypted: bool,
}

impl RbStream {
    fn dict(ruby: &Ruby, rb_self: &Self) -> Result<RHash, Error> {
        let dict = rb_self.obj.as_dict().expect("RbStream always wraps Object::Stream");
        dict_to_ruby(ruby, dict, rb_self.encrypted)
    }
    fn data(ruby: &Ruby, rb_self: &Self) -> Result<RString, Error> {
        if rb_self.encrypted {
            return Err(encrypted_error(ruby));
        }
        rb_self
            .obj
            .decode_stream_data()
            .map(|b| ruby.str_from_slice(&b))
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn raw_data(ruby: &Ruby, rb_self: &Self) -> RString {
        match &rb_self.obj {
            Object::Stream { data, .. } => ruby.str_from_slice(data),
            _ => unreachable!("RbStream always wraps Object::Stream"),
        }
    }
    fn encrypted(&self) -> bool {
        self.encrypted
    }
    fn inspect(&self) -> String {
        let dict = self.obj.as_dict().expect("stream dict");
        let raw_len = match &self.obj {
            Object::Stream { data, .. } => data.len(),
            _ => 0,
        };
        let subtype = dict
            .get("Subtype")
            .and_then(|s| s.as_name())
            .map(|s| format!(" /{s}"))
            .unwrap_or_default();
        format!("#<PdfDioxide::Ext::Stream{subtype} raw_bytes={raw_len}>")
    }
}

fn encrypted_error(ruby: &Ruby) -> Error {
    Error::new(
        ruby.get_inner(&UNSUPPORTED_ERROR),
        "stream data of an encrypted PDF is not available: pdf_oxide's public \
         API does not expose stream decryption",
    )
}

fn dict_to_ruby(
    ruby: &Ruby,
    dict: &std::collections::HashMap<String, Object>,
    encrypted: bool,
) -> Result<RHash, Error> {
    let h = ruby.hash_new();
    for (k, v) in dict {
        h.aset(k.as_str(), object_to_ruby(ruby, v, encrypted)?)?;
    }
    Ok(h)
}

fn object_to_ruby(ruby: &Ruby, obj: &Object, encrypted: bool) -> Result<Value, Error> {
    Ok(match obj {
        Object::Null => ruby.qnil().as_value(),
        Object::Boolean(b) => b.into_value_with(ruby),
        Object::Integer(i) => i.into_value_with(ruby),
        Object::Real(f) => f.into_value_with(ruby),
        Object::String(bytes) => ruby.str_from_slice(bytes).as_value(),
        Object::Name(n) => ruby.to_symbol(n.as_str()).as_value(),
        Object::Array(items) => {
            let ary = ruby.ary_new();
            for item in items {
                ary.push(object_to_ruby(ruby, item, encrypted)?)?;
            }
            ary.as_value()
        },
        Object::Dictionary(d) => dict_to_ruby(ruby, d, encrypted)?.as_value(),
        Object::Stream { .. } => RbStream {
            obj: obj.clone(),
            encrypted,
        }
        .into_value_with(ruby),
        Object::Reference(r) => RbObjectRef(*r).into_value_with(ruby),
    })
}

// ---------------------------------------------------------------------------
// Content stream operators -> Ruby
// ---------------------------------------------------------------------------
//
// Each operator becomes `[mnemonic, *operands]` using the PDF spec mnemonics
// (ISO 32000-1 Table A.1): `["Tj", "bytes"]`, `["Do", "Fm1"]`,
// `["re", x, y, w, h]`, `["BI", dict, data]`, ... Operators the core does
// not model become `[name, *operands]` with operands converted like objects.

fn bytes_val(ruby: &Ruby, b: &[u8]) -> Value {
    ruby.str_from_slice(b).as_value()
}

fn text_array_to_ruby(ruby: &Ruby, elems: &[TextElement]) -> Result<Value, Error> {
    let ary = ruby.ary_new();
    for e in elems {
        match e {
            TextElement::String(s) => ary.push(bytes_val(ruby, s))?,
            TextElement::Offset(o) => ary.push(*o)?,
        }
    }
    Ok(ary.as_value())
}

fn operator_to_ruby(ruby: &Ruby, op: &Operator) -> Result<RArray, Error> {
    use Operator::*;
    let out = ruby.ary_new();
    macro_rules! op {
        ($name:expr $(, $arg:expr)*) => {{
            out.push($name)?;
            $( out.push($arg)?; )*
        }};
    }
    match op {
        Td { tx, ty } => op!("Td", *tx, *ty),
        TD { tx, ty } => op!("TD", *tx, *ty),
        Tm { a, b, c, d, e, f } => op!("Tm", *a, *b, *c, *d, *e, *f),
        TStar => op!("T*"),
        Tj { text } => op!("Tj", bytes_val(ruby, text)),
        TJ { array } => op!("TJ", text_array_to_ruby(ruby, array)?),
        Quote { text } => op!("'", bytes_val(ruby, text)),
        DoubleQuote {
            word_space,
            char_space,
            text,
        } => op!("\"", *word_space, *char_space, bytes_val(ruby, text)),
        Tc { char_space } => op!("Tc", *char_space),
        Tw { word_space } => op!("Tw", *word_space),
        Tz { scale } => op!("Tz", *scale),
        TL { leading } => op!("TL", *leading),
        Tf { font, size } => op!("Tf", ruby.to_symbol(font.as_str()), *size),
        Tr { render } => op!("Tr", *render as i64),
        Ts { rise } => op!("Ts", *rise),
        SaveState => op!("q"),
        RestoreState => op!("Q"),
        Cm { a, b, c, d, e, f } => op!("cm", *a, *b, *c, *d, *e, *f),
        SetFillRgb { r, g, b } => op!("rg", *r, *g, *b),
        SetStrokeRgb { r, g, b } => op!("RG", *r, *g, *b),
        SetFillGray { gray } => op!("g", *gray),
        SetStrokeGray { gray } => op!("G", *gray),
        SetFillCmyk { c, m, y, k } => op!("k", *c, *m, *y, *k),
        SetStrokeCmyk { c, m, y, k } => op!("K", *c, *m, *y, *k),
        SetFillColorSpace { name } => op!("cs", ruby.to_symbol(name.as_str())),
        SetStrokeColorSpace { name } => op!("CS", ruby.to_symbol(name.as_str())),
        SetFillColor { components } => op!("sc", components.clone()),
        SetStrokeColor { components } => op!("SC", components.clone()),
        SetFillColorN { components, name } => {
            out.push("scn")?;
            out.push(components.clone())?;
            if let Some(n) = name {
                out.push(ruby.to_symbol(n.as_str()))?;
            }
        },
        SetStrokeColorN { components, name } => {
            out.push("SCN")?;
            out.push(components.clone())?;
            if let Some(n) = name {
                out.push(ruby.to_symbol(n.as_str()))?;
            }
        },
        BeginText => op!("BT"),
        EndText => op!("ET"),
        Do { name } => op!("Do", ruby.to_symbol(name.as_str())),
        MoveTo { x, y } => op!("m", *x, *y),
        LineTo { x, y } => op!("l", *x, *y),
        CurveTo {
            x1,
            y1,
            x2,
            y2,
            x3,
            y3,
        } => op!("c", *x1, *y1, *x2, *y2, *x3, *y3),
        CurveToV { x2, y2, x3, y3 } => op!("v", *x2, *y2, *x3, *y3),
        CurveToY { x1, y1, x3, y3 } => op!("y", *x1, *y1, *x3, *y3),
        ClosePath => op!("h"),
        Rectangle {
            x,
            y,
            width,
            height,
        } => op!("re", *x, *y, *width, *height),
        Stroke => op!("S"),
        Fill => op!("f"),
        FillEvenOdd => op!("f*"),
        CloseFillStroke => op!("b"),
        FillStroke => op!("B"),
        FillStrokeEvenOdd => op!("B*"),
        CloseFillStrokeEvenOdd => op!("b*"),
        EndPath => op!("n"),
        ClipNonZero => op!("W"),
        ClipEvenOdd => op!("W*"),
        SetLineWidth { width } => op!("w", *width),
        SetDash { array, phase } => op!("d", array.clone(), *phase),
        SetLineCap { cap_style } => op!("J", *cap_style as i64),
        SetLineJoin { join_style } => op!("j", *join_style as i64),
        SetMiterLimit { limit } => op!("M", *limit),
        SetRenderingIntent { intent } => op!("ri", ruby.to_symbol(intent.as_str())),
        SetFlatness { tolerance } => op!("i", *tolerance),
        SetExtGState { dict_name } => op!("gs", ruby.to_symbol(dict_name.as_str())),
        PaintShading { name } => op!("sh", ruby.to_symbol(name.as_str())),
        InlineImage { dict, data } => {
            op!("BI", dict_to_ruby(ruby, dict, false)?, bytes_val(ruby, data))
        },
        BeginMarkedContent { tag } => op!("BMC", ruby.to_symbol(tag.as_str())),
        BeginMarkedContentDict { tag, properties } => op!(
            "BDC",
            ruby.to_symbol(tag.as_str()),
            object_to_ruby(ruby, properties, false)?
        ),
        EndMarkedContent => op!("EMC"),
        Other { name, operands } => {
            out.push(name.as_str())?;
            for o in operands.iter() {
                out.push(object_to_ruby(ruby, o, false)?)?;
            }
        },
    }
    Ok(out)
}

/// EXT: `PdfDioxide::Ext.content_operators(bytes, text_only: false)
/// #=> Array<Array>` — parse a (decoded) content stream into
/// `[mnemonic, *operands]` arrays. `text_only: true` uses the fast parser
/// that keeps only BT..ET blocks plus `Do`/`cm`/`q`/`Q` (drops path and
/// colour operators).
/// Rust core: `content::parser::{parse_content_stream, parse_content_stream_text_only}`.
/// Upstream: not yet filed.
fn content_operators(ruby: &Ruby, args: &[Value]) -> Result<RArray, Error> {
    let args = scan_args::<(RString,), (), (), (), RHash, ()>(args)?;
    let (bytes,) = args.required;
    let kw = get_kwargs::<_, (), (Option<bool>,), ()>(args.keywords, &[], &["text_only"])?;
    let text_only = kw.optional.0.unwrap_or(false);

    let data = unsafe { bytes.as_slice() }.to_vec();
    let ops = if text_only {
        parse_content_stream_text_only(&data)
    } else {
        parse_content_stream(&data)
    }
    .map_err(|e| map_pdf_error(ruby, e))?;

    let out = ruby.ary_new();
    for op in &ops {
        out.push(operator_to_ruby(ruby, op)?)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Document-level primitives (PdfDioxide::Ext::Document mixin)
// ---------------------------------------------------------------------------

fn with_doc<R>(rb_self: Value, f: impl FnOnce(&PdfDocument) -> Result<R, Error>) -> Result<R, Error> {
    use magnus::TryConvert;
    let this = <&RbPdfDocument as TryConvert>::try_convert(rb_self)?;
    this.with_doc(f)
}

fn object_ref_arg(value: Value) -> Result<ObjectRef, Error> {
    use magnus::TryConvert;
    Ok(<&RbObjectRef as TryConvert>::try_convert(value)?.0)
}

/// EXT: `doc.page_object(index) #=> Hash` — the page dictionary with
/// inheritable attributes (Resources, MediaBox, Rotate, ...) applied.
/// Rust core: `PdfDocument::get_page`. Upstream: not yet filed.
fn page_object(ruby: &Ruby, rb_self: Value, index: usize) -> Result<Value, Error> {
    with_doc(rb_self, |doc| {
        let page = doc.get_page(index).map_err(|e| map_pdf_error(ruby, e))?;
        object_to_ruby(ruby, &page, doc.is_encrypted())
    })
}

/// EXT: `doc.catalog_object #=> Hash` — the document catalog (`/Root`).
/// Rust core: `PdfDocument::catalog`. Upstream: not yet filed.
fn catalog_object(ruby: &Ruby, rb_self: Value) -> Result<Value, Error> {
    with_doc(rb_self, |doc| {
        let cat = doc.catalog().map_err(|e| map_pdf_error(ruby, e))?;
        object_to_ruby(ruby, &cat, doc.is_encrypted())
    })
}

/// EXT: `doc.trailer_object #=> Hash` — the trailer dictionary.
/// Rust core: `PdfDocument::trailer`. Upstream: not yet filed.
fn trailer_object(ruby: &Ruby, rb_self: Value) -> Result<Value, Error> {
    with_doc(rb_self, |doc| object_to_ruby(ruby, doc.trailer(), doc.is_encrypted()))
}

/// EXT: `doc.load_object(ref) #=> Object` — load an indirect object by
/// `PdfDioxide::Ext::ObjectRef` (any type; streams come back as
/// `Ext::Stream`). Rust core: `PdfDocument::load_object`. Upstream: not
/// yet filed.
fn load_object(ruby: &Ruby, rb_self: Value, obj_ref: Value) -> Result<Value, Error> {
    let r = object_ref_arg(obj_ref)?;
    with_doc(rb_self, |doc| {
        let obj = doc.load_object(r).map_err(|e| map_pdf_error(ruby, e))?;
        object_to_ruby(ruby, &obj, doc.is_encrypted())
    })
}

/// EXT: `doc.form_xobject?(ref) #=> true/false` — peek at an XObject's
/// /Subtype without loading image data (conservatively true when the
/// object cannot be peeked). Rust core: `PdfDocument::is_form_xobject`.
/// Upstream: not yet filed.
fn form_xobject(_ruby: &Ruby, rb_self: Value, obj_ref: Value) -> Result<bool, Error> {
    let r = object_ref_arg(obj_ref)?;
    with_doc(rb_self, |doc| Ok(doc.is_form_xobject(r)))
}

/// EXT: `doc.page_content(index) #=> String (binary)` — the page's
/// decoded content stream(s), concatenated in order.
/// Rust core: `PdfDocument::get_page_content_data`. Upstream: not yet filed.
fn page_content(ruby: &Ruby, rb_self: Value, index: usize) -> Result<RString, Error> {
    with_doc(rb_self, |doc| {
        doc.get_page_content_data(index)
            .map(|b| ruby.str_from_slice(&b))
            .map_err(|e| map_pdf_error(ruby, e))
    })
}

/// EXT: `doc.stream_data(ref) #=> String (binary)` — load an indirect
/// stream and decode its filters (shorthand for `load_object(ref).data`).
/// Rust core: `PdfDocument::load_object` + `Object::decode_stream_data`.
/// Upstream: not yet filed.
fn stream_data(ruby: &Ruby, rb_self: Value, obj_ref: Value) -> Result<RString, Error> {
    let r = object_ref_arg(obj_ref)?;
    with_doc(rb_self, |doc| {
        if doc.is_encrypted() {
            return Err(encrypted_error(ruby));
        }
        let obj = doc.load_object(r).map_err(|e| map_pdf_error(ruby, e))?;
        if !matches!(obj, Object::Stream { .. }) {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("{} {} R is not a stream", r.id, r.gen),
            ));
        }
        obj.decode_stream_data()
            .map(|b| ruby.str_from_slice(&b))
            .map_err(|e| map_pdf_error(ruby, e))
    })
}

/// EXT: `doc.experimental_render_page(page, **render_page kwargs)
/// #=> String (binary image)` — identical to the parity `render_page`
/// except `RenderOptions::resolve_form_resources` is enabled: pdf_dioxide's
/// opt-in fix for upstream #1309 (text inside a Form XObject whose
/// `/Resources` is an indirect reference was not drawn). Requires the
/// patched upstream build (`UPSTREAM_VERSION` ends in `+fix1309`).
/// Graduates into `render_page` once upstream merges the fix.
fn experimental_render_page(ruby: &Ruby, rb_self: Value, args: &[Value]) -> Result<RString, Error> {
    let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
    let (page,) = args.required;
    let mut options = crate::render_options_from_kwargs(ruby, args.keywords, Some(72))?;
    options.resolve_form_resources = true;
    with_doc(rb_self, |doc| {
        pdf_oxide::rendering::render_page(doc, page, &options)
            .map(|img| ruby.str_from_slice(&img.data))
            .map_err(|e| map_pdf_error(ruby, e))
    })
}

/// Register the Ext surface: `PdfDioxide::Ext`, its value classes and the
/// per-class mixin modules. Attachment to the parity classes happens in
/// Ruby (`require "pdf_dioxide/ext"`), never here.
pub(crate) fn init(ruby: &Ruby, module: RModule) -> Result<(), Error> {
    let ext = module.define_module("Ext")?;

    let object_ref = ext.define_class("ObjectRef", ruby.class_object())?;
    object_ref.define_singleton_method("new", function!(RbObjectRef::new, -1))?;
    object_ref.define_method("id", method!(RbObjectRef::id, 0))?;
    object_ref.define_method("gen", method!(RbObjectRef::gen, 0))?;
    object_ref.define_method("to_s", method!(RbObjectRef::to_s, 0))?;
    object_ref.define_method("inspect", method!(RbObjectRef::inspect, 0))?;

    let stream = ext.define_class("Stream", ruby.class_object())?;
    stream.define_method("dict", method!(RbStream::dict, 0))?;
    stream.define_method("data", method!(RbStream::data, 0))?;
    stream.define_method("raw_data", method!(RbStream::raw_data, 0))?;
    stream.define_method("encrypted?", method!(RbStream::encrypted, 0))?;
    stream.define_method("inspect", method!(RbStream::inspect, 0))?;

    ext.define_module_function("content_operators", function!(content_operators, -1))?;

    let document = ext.define_module("Document")?;
    document.define_method("page_object", method!(page_object, 1))?;
    document.define_method("catalog_object", method!(catalog_object, 0))?;
    document.define_method("trailer_object", method!(trailer_object, 0))?;
    document.define_method("load_object", method!(load_object, 1))?;
    document.define_method("form_xobject?", method!(form_xobject, 1))?;
    document.define_method("page_content", method!(page_content, 1))?;
    document.define_method("stream_data", method!(stream_data, 1))?;
    document.define_method(
        "experimental_render_page",
        method!(experimental_render_page, -1),
    )?;
    Ok(())
}
