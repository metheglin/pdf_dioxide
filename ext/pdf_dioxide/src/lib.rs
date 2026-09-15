use std::cell::RefCell;
use std::collections::HashSet;

use magnus::{
    function, method,
    prelude::*,
    IntoValue,
    scan_args::{get_kwargs, scan_args},
    value::Lazy,
    Error, ExceptionClass, RHash, RModule, RString, Ruby, Value,
};
use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::editor::{
    DocumentEditor, EditableDocument, EncryptionAlgorithm, EncryptionConfig, Permissions,
    SaveOptions,
};
use pdf_oxide::layout::{
    RectFilterMode, SpatialCollectionFiltering, TextChar, TextLine, TextSpan, Word,
};
use pdf_oxide::redaction::{RedactionOptions, RedactionReport};
use pdf_oxide::rendering::RenderOptions;
use pdf_oxide::{error::Error as PdfError, PdfDocument, ReadingOrder};

// pdf_dioxide-only surface (see .claude/rules/extensions.md).
mod ext;

// ---------------------------------------------------------------------------
// Exception hierarchy
// ---------------------------------------------------------------------------

// Module and exception classes are defined lazily so the mapping code can
// reach them from any callback without threading handles around; `init`
// forces them all so `rescue PdfDioxide::ParseError` works before the first
// error is ever raised.
static PDF_OXIDE: Lazy<RModule> = Lazy::new(|ruby| ruby.define_module("PdfDioxide").unwrap());

/// `PdfDioxide::Error < StandardError` — base class and catch-all for pdf_oxide
/// errors that have no more specific mapping below.
static ERROR: Lazy<ExceptionClass> = Lazy::new(|ruby| {
    ruby.get_inner(&PDF_OXIDE)
        .define_error("Error", ruby.exception_standard_error())
        .unwrap()
});

/// `PdfDioxide::IoError` — the underlying file could not be read.
static IO_ERROR: Lazy<ExceptionClass> = Lazy::new(|ruby| {
    ruby.get_inner(&PDF_OXIDE)
        .define_error("IoError", ruby.get_inner(&ERROR))
        .unwrap()
});

/// `PdfDioxide::ParseError` — the bytes are not a well-formed PDF (bad header,
/// broken xref, truncated file, undecodable stream, ...).
static PARSE_ERROR: Lazy<ExceptionClass> = Lazy::new(|ruby| {
    ruby.get_inner(&PDF_OXIDE)
        .define_error("ParseError", ruby.get_inner(&ERROR))
        .unwrap()
});

/// `PdfDioxide::PasswordError` — the PDF is encrypted and needs (correct)
/// authentication before content can be extracted.
static PASSWORD_ERROR: Lazy<ExceptionClass> = Lazy::new(|ruby| {
    ruby.get_inner(&PDF_OXIDE)
        .define_error("PasswordError", ruby.get_inner(&ERROR))
        .unwrap()
});

/// `PdfDioxide::UnsupportedError` — valid PDF, but it uses a feature or filter
/// this build of pdf_oxide cannot handle.
static UNSUPPORTED_ERROR: Lazy<ExceptionClass> = Lazy::new(|ruby| {
    ruby.get_inner(&PDF_OXIDE)
        .define_error("UnsupportedError", ruby.get_inner(&ERROR))
        .unwrap()
});

/// Map a `pdf_oxide::error::Error` onto the `PdfDioxide::Error` hierarchy.
///
/// Grouping is by variant, not by message. Variants that are cfg-gated in
/// pdf_oxide (`Ml`, `Ocr`) or too niche for their own class (`Font`, `Image`,
/// `Barcode`, ...) fall through to the base `PdfDioxide::Error`.
fn map_pdf_error(ruby: &Ruby, e: PdfError) -> Error {
    let class = match &e {
        PdfError::Io(_) => ruby.get_inner(&IO_ERROR),
        PdfError::EncryptedPdf => ruby.get_inner(&PASSWORD_ERROR),
        PdfError::Unsupported(_)
        | PdfError::UnsupportedFilter(_)
        | PdfError::UnsupportedVersion(_) => ruby.get_inner(&UNSUPPORTED_ERROR),
        PdfError::InvalidHeader(_)
        | PdfError::ParseError { .. }
        | PdfError::ParseWarning { .. }
        | PdfError::InvalidXref
        | PdfError::ObjectNotFound(..)
        | PdfError::InvalidObjectType { .. }
        | PdfError::UnexpectedEof
        | PdfError::Utf8Error(_)
        | PdfError::InvalidPdf(_)
        | PdfError::Decode(_)
        | PdfError::CircularReference(_)
        | PdfError::RecursionLimitExceeded(_) => ruby.get_inner(&PARSE_ERROR),
        _ => ruby.get_inner(&ERROR),
    };
    Error::new(class, e.to_string())
}

// ---------------------------------------------------------------------------
// Conversion options (ConversionOptions <- Ruby keyword args)
// ---------------------------------------------------------------------------

/// Build `ConversionOptions` from Ruby keyword arguments, mirroring the
/// defaults of the PyO3 signatures (`preserve_layout=false`,
/// `detect_headings=true`, `include_images=false`, `embed_images=true`,
/// `include_form_fields=true`, `include_artifacts=true`; `extract_tables` is
/// always on, as in python.rs).
fn conversion_options(kw: RHash) -> Result<ConversionOptions, Error> {
    type Opts = (
        Option<bool>,   // preserve_layout
        Option<bool>,   // detect_headings
        Option<bool>,   // include_images
        Option<String>, // image_output_dir
        Option<bool>,   // embed_images
        Option<bool>,   // include_form_fields
        Option<bool>,   // include_artifacts
    );
    let kw = get_kwargs::<_, (), Opts, ()>(
        kw,
        &[],
        &[
            "preserve_layout",
            "detect_headings",
            "include_images",
            "image_output_dir",
            "embed_images",
            "include_form_fields",
            "include_artifacts",
        ],
    )?;
    let (
        preserve_layout,
        detect_headings,
        include_images,
        image_output_dir,
        embed_images,
        include_form_fields,
        include_artifacts,
    ) = kw.optional;
    Ok(ConversionOptions {
        preserve_layout: preserve_layout.unwrap_or(false),
        detect_headings: detect_headings.unwrap_or(true),
        extract_tables: true,
        include_images: include_images.unwrap_or(false),
        image_output_dir,
        embed_images: embed_images.unwrap_or(true),
        include_form_fields: include_form_fields.unwrap_or(true),
        include_artifacts: include_artifacts.unwrap_or(true),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Render options (RenderOptions <- Ruby keyword args)
// ---------------------------------------------------------------------------

/// Build `RenderOptions` from Ruby keyword arguments, mirroring the PyO3
/// `render_page` signature. `default_dpi: None` starts from
/// `RenderOptions::default()` (the `render_page_fit` shape, where DPI is
/// computed); `Some(dpi)` is the fallback when the caller passes no `dpi:`.
fn render_options_from_kwargs(
    ruby: &Ruby,
    kw: RHash,
    default_dpi: Option<u32>,
) -> Result<RenderOptions, Error> {
    type Opts = (
        Option<u32>,                    // dpi
        Option<String>,                 // format
        Option<(f32, f32, f32, f32)>,   // background
        Option<bool>,                   // transparent
        Option<bool>,                   // render_annotations
        Option<u8>,                     // jpeg_quality
        Option<Vec<String>>,            // excluded_layers
    );
    let kw = get_kwargs::<_, (), Opts, ()>(
        kw,
        &[],
        &[
            "dpi",
            "format",
            "background",
            "transparent",
            "render_annotations",
            "jpeg_quality",
            "excluded_layers",
        ],
    )?;
    let (dpi, format, background, transparent, render_annotations, jpeg_quality, excluded_layers) =
        kw.optional;

    let quality = match jpeg_quality {
        Some(q) => {
            if !(1..=100).contains(&q) {
                return Err(Error::new(
                    ruby.exception_arg_error(),
                    format!("jpeg_quality must be 1-100, got {q}"),
                ));
            }
            q
        },
        None => 85,
    };

    let mut options = match default_dpi {
        Some(fallback) => RenderOptions::with_dpi(dpi.unwrap_or(fallback)),
        None => RenderOptions::default(),
    };
    if let Some(fmt) = format {
        if fmt.eq_ignore_ascii_case("jpeg") || fmt.eq_ignore_ascii_case("jpg") {
            options = options.as_jpeg(quality);
        } else if fmt.eq_ignore_ascii_case("png") {
            // default — no change
        } else {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("format must be 'png' or 'jpeg', got {fmt:?}"),
            ));
        }
    }
    if let Some((r, g, b, a)) = background {
        options.background = Some([r, g, b, a]);
    }
    if transparent.unwrap_or(false) {
        options.background = None;
    }
    if let Some(flag) = render_annotations {
        options.render_annotations = flag;
    }
    if let Some(layers) = excluded_layers {
        options.excluded_layers = layers.into_iter().collect();
    }
    Ok(options)
}

/// RedactionReport -> Ruby Hash with the same keys as the PyO3 dict.
fn redaction_report_to_hash(ruby: &Ruby, report: &RedactionReport) -> Result<RHash, Error> {
    let h = ruby.hash_new();
    h.aset("regions", report.regions)?;
    h.aset("glyphs_removed", report.glyphs_removed)?;
    h.aset("images_modified", report.images_modified)?;
    h.aset("images_removed", report.images_removed)?;
    h.aset("paths_pruned", report.paths_pruned)?;
    h.aset("xobjects_specialized", report.xobjects_specialized)?;
    h.aset("annotations_removed", report.annotations_removed)?;
    h.aset("fonts_scrubbed", report.fonts_scrubbed)?;
    h.aset("bytes_removed", report.bytes_removed)?;
    Ok(h)
}

// ---------------------------------------------------------------------------
// Shared hash builders
// ---------------------------------------------------------------------------

/// PathContent -> Ruby Hash (same keys as the PyO3 `path_to_py_dict`).
fn path_to_hash(ruby: &Ruby, path: &pdf_oxide::elements::PathContent) -> Result<RHash, Error> {
    use pdf_oxide::elements::PathOperation;

    let h = ruby.hash_new();
    h.aset("bbox", (path.bbox.x, path.bbox.y, path.bbox.width, path.bbox.height))?;
    // Stroke-inflated extents — what the reader sees.
    let rendered = path.rendered_bbox();
    h.aset("rendered_bbox", (rendered.x, rendered.y, rendered.width, rendered.height))?;
    h.aset("stroke_width", path.stroke_width)?;
    h.aset("stroke_color", path.stroke_color.as_ref().map(|c| (c.r, c.g, c.b)))?;
    h.aset("fill_color", path.fill_color.as_ref().map(|c| (c.r, c.g, c.b)))?;
    h.aset("operations_count", path.operations.len())?;
    // Optional Content Group ("layer") name, nil outside any /OC region.
    h.aset("layer", path.layer.as_deref())?;

    let ops = ruby.ary_new();
    for op in &path.operations {
        let oh = ruby.hash_new();
        match op {
            PathOperation::MoveTo(x, y) => {
                oh.aset("op", "move_to")?;
                oh.aset("x", *x)?;
                oh.aset("y", *y)?;
            },
            PathOperation::LineTo(x, y) => {
                oh.aset("op", "line_to")?;
                oh.aset("x", *x)?;
                oh.aset("y", *y)?;
            },
            PathOperation::CurveTo(cx1, cy1, cx2, cy2, x, y) => {
                oh.aset("op", "curve_to")?;
                oh.aset("cx1", *cx1)?;
                oh.aset("cy1", *cy1)?;
                oh.aset("cx2", *cx2)?;
                oh.aset("cy2", *cy2)?;
                oh.aset("x", *x)?;
                oh.aset("y", *y)?;
            },
            PathOperation::Rectangle(x, y, w, hh) => {
                oh.aset("op", "rectangle")?;
                oh.aset("x", *x)?;
                oh.aset("y", *y)?;
                oh.aset("width", *w)?;
                oh.aset("height", *hh)?;
            },
            PathOperation::ClosePath => {
                oh.aset("op", "close_path")?;
            },
        }
        ops.push(oh)?;
    }
    h.aset("operations", ops)?;
    Ok(h)
}

/// Search results -> Ruby Array of Hashes.
fn search_results_to_ary(
    ruby: &Ruby,
    results: Vec<pdf_oxide::search::SearchResult>,
) -> Result<magnus::RArray, Error> {
    let out = ruby.ary_new();
    for r in results {
        let h = ruby.hash_new();
        h.aset("page", r.page)?;
        h.aset("text", r.text.as_str())?;
        h.aset("x", r.bbox.x)?;
        h.aset("y", r.bbox.y)?;
        h.aset("width", r.bbox.width)?;
        h.aset("height", r.bbox.height)?;
        out.push(h)?;
    }
    Ok(out)
}

/// Outline tree -> Ruby Array of Hashes (recursive `children`).
fn outline_items_to_ary(
    ruby: &Ruby,
    items: &[pdf_oxide::outline::OutlineItem],
) -> Result<magnus::RArray, Error> {
    let out = ruby.ary_new();
    for i in items {
        let h = ruby.hash_new();
        h.aset("title", i.title.as_str())?;
        match &i.dest {
            Some(pdf_oxide::outline::Destination::PageIndex(idx)) => h.aset("page", *idx)?,
            _ => h.aset("page", None::<usize>)?,
        }
        h.aset("children", outline_items_to_ary(ruby, &i.children)?)?;
        out.push(h)?;
    }
    Ok(out)
}

/// "1b" / "2a" / ... -> PdfALevel, ArgumentError on anything else.
fn parse_pdf_a_level(ruby: &Ruby, level: &str) -> Result<pdf_oxide::compliance::PdfALevel, Error> {
    use pdf_oxide::compliance::PdfALevel;
    Ok(match level {
        "1a" => PdfALevel::A1a,
        "1b" => PdfALevel::A1b,
        "2a" => PdfALevel::A2a,
        "2b" => PdfALevel::A2b,
        "2u" => PdfALevel::A2u,
        "3a" => PdfALevel::A3a,
        "3b" => PdfALevel::A3b,
        "3u" => PdfALevel::A3u,
        _ => {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("Unknown PDF/A level: '{level}'. Use 1a, 1b, 2a, 2b, 2u, 3a, 3b, 3u"),
            ))
        },
    })
}

// ---------------------------------------------------------------------------
// PdfDioxide::FormField
// ---------------------------------------------------------------------------

/// `PdfDioxide::FormField` — a form field discovered by `get_form_fields`.
#[magnus::wrap(class = "PdfDioxide::FormField", free_immediately, size)]
struct RbFormField(pdf_oxide::extractors::forms::FormField);

impl RbFormField {
    fn name(&self) -> String {
        self.0.full_name.clone()
    }
    fn field_type(&self) -> &'static str {
        use pdf_oxide::extractors::forms::FieldType;
        match &self.0.field_type {
            FieldType::Text => "text",
            FieldType::Button => "button",
            FieldType::Choice => "choice",
            FieldType::Signature => "signature",
            FieldType::Unknown(_) => "unknown",
        }
    }
    fn value(ruby: &Ruby, rb_self: &Self) -> Result<Value, Error> {
        use pdf_oxide::extractors::forms::FieldValue;
        Ok(match &rb_self.0.value {
            FieldValue::Text(s) => s.as_str().into_value_with(ruby),
            FieldValue::Name(s) => s.as_str().into_value_with(ruby),
            FieldValue::Boolean(b) => (*b).into_value_with(ruby),
            FieldValue::Array(v) => v.clone().into_value_with(ruby),
            FieldValue::None => ruby.qnil().as_value(),
        })
    }
    fn tooltip(&self) -> Option<String> {
        self.0.tooltip.clone()
    }
    fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        self.0.bounds.map(|b| (b[0], b[1], b[2], b[3]))
    }
    fn flags(&self) -> Option<u32> {
        self.0.flags
    }
    fn max_length(&self) -> Option<u32> {
        self.0.max_length
    }
    fn is_readonly(&self) -> bool {
        use pdf_oxide::extractors::forms::field_flags;
        self.0.flags.is_some_and(|f| f & field_flags::READ_ONLY != 0)
    }
    fn is_required(&self) -> bool {
        use pdf_oxide::extractors::forms::field_flags;
        self.0.flags.is_some_and(|f| f & field_flags::REQUIRED != 0)
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::FormField name={:?} type={:?}>",
            self.0.full_name,
            self.field_type()
        )
    }
}

// ---------------------------------------------------------------------------
// Extraction tuning classes (ExtractionProfile / LayoutParams)
// ---------------------------------------------------------------------------

/// `PdfDioxide::ExtractionProfile` — pre-tuned extraction knobs; construct via
/// the named class methods (`conservative`, `balanced`, ...).
#[magnus::wrap(class = "PdfDioxide::ExtractionProfile", free_immediately, size)]
#[derive(Clone)]
struct RbExtractionProfile(pdf_oxide::config::ExtractionProfile);

impl RbExtractionProfile {
    fn name(&self) -> &'static str {
        self.0.name
    }
    fn tj_offset_threshold(&self) -> f32 {
        self.0.tj_offset_threshold
    }
    fn word_margin_ratio(&self) -> f32 {
        self.0.word_margin_ratio
    }
    fn space_threshold_em_ratio(&self) -> f32 {
        self.0.space_threshold_em_ratio
    }
    fn space_char_multiplier(&self) -> f32 {
        self.0.space_char_multiplier
    }
    fn use_adaptive_threshold(&self) -> bool {
        self.0.use_adaptive_threshold
    }
    fn conservative() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::CONSERVATIVE)
    }
    fn aggressive() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::AGGRESSIVE)
    }
    fn balanced() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::BALANCED)
    }
    fn academic() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::ACADEMIC)
    }
    fn policy() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::POLICY)
    }
    fn form() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::FORM)
    }
    fn government() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::GOVERNMENT)
    }
    fn scanned_ocr() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::SCANNED_OCR)
    }
    fn adaptive() -> Self {
        Self(pdf_oxide::config::ExtractionProfile::ADAPTIVE)
    }
    fn available() -> Vec<&'static str> {
        pdf_oxide::config::ExtractionProfile::all_profiles().to_vec()
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::ExtractionProfile {:?} word_margin_ratio={} tj_offset_threshold={}>",
            self.0.name, self.0.word_margin_ratio, self.0.tj_offset_threshold
        )
    }
}

/// Parse an optional `profile:` keyword value into a core ExtractionProfile.
fn profile_from_kwarg(
    value: Option<Value>,
) -> Result<Option<pdf_oxide::config::ExtractionProfile>, Error> {
    use magnus::TryConvert;
    match value {
        Some(v) if !v.is_nil() => {
            let p = <&RbExtractionProfile as TryConvert>::try_convert(v)?;
            Ok(Some(p.0.clone()))
        },
        _ => Ok(None),
    }
}

/// `PdfDioxide::LayoutParams` — computed adaptive layout parameters for a page.
#[magnus::wrap(class = "PdfDioxide::LayoutParams", free_immediately, size)]
struct RbLayoutParams {
    word_gap_threshold: f32,
    line_gap_threshold: f32,
    median_char_width: f32,
    median_font_size: f32,
    median_line_spacing: f32,
    column_count: usize,
}

impl RbLayoutParams {
    fn word_gap_threshold(&self) -> f32 {
        self.word_gap_threshold
    }
    fn line_gap_threshold(&self) -> f32 {
        self.line_gap_threshold
    }
    fn median_char_width(&self) -> f32 {
        self.median_char_width
    }
    fn median_font_size(&self) -> f32 {
        self.median_font_size
    }
    fn median_line_spacing(&self) -> f32 {
        self.median_line_spacing
    }
    fn column_count(&self) -> usize {
        self.column_count
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::LayoutParams word_gap={:.2} line_gap={:.2} char_width={:.2} \
             font_size={:.2} line_spacing={:.2} columns={}>",
            self.word_gap_threshold,
            self.line_gap_threshold,
            self.median_char_width,
            self.median_font_size,
            self.median_line_spacing,
            self.column_count,
        )
    }
}

// ---------------------------------------------------------------------------
// Signature classes (Signature / Certificate / Timestamp / Dss /
// RevocationMaterial)
// ---------------------------------------------------------------------------

fn pades_level_to_str(level: pdf_oxide::signatures::PadesLevel) -> &'static str {
    use pdf_oxide::signatures::PadesLevel;
    match level {
        PadesLevel::BB => "B_B",
        PadesLevel::BT => "B_T",
        PadesLevel::BLt => "B_LT",
        PadesLevel::BLta => "B_LTA",
        _ => "UNKNOWN",
    }
}

/// `PdfDioxide::Signature` — an existing signature from `doc.signatures`.
/// `pades_level` is returned as a String (`"B_B"`, `"B_T"`, `"B_LT"`,
/// `"B_LTA"`) instead of Python's `PadesLevel` enum class.
#[magnus::wrap(class = "PdfDioxide::Signature", free_immediately, size)]
struct RbSignature(pdf_oxide::signatures::SignatureInfo);

impl RbSignature {
    fn signer_name(&self) -> Option<String> {
        self.0.signer_name.clone()
    }
    fn reason(&self) -> Option<String> {
        self.0.reason.clone()
    }
    fn location(&self) -> Option<String> {
        self.0.location.clone()
    }
    fn contact_info(&self) -> Option<String> {
        self.0.contact_info.clone()
    }
    fn signing_time(&self) -> Option<i64> {
        self.0
            .signing_time
            .as_deref()
            .and_then(pdf_oxide::signatures::parse_pdf_date_to_epoch)
    }
    fn covers_whole_document(&self) -> bool {
        self.0.covers_whole_document
    }
    fn pades_level(&self) -> &'static str {
        pades_level_to_str(pdf_oxide::signatures::classify_pades_level(&self.0, None))
    }
    fn verify(ruby: &Ruby, rb_self: &Self) -> Result<bool, Error> {
        use pdf_oxide::signatures::{verify_signer, SignerVerify};
        let Some(contents) = rb_self.0.contents() else {
            return Err(Error::new(
                ruby.exception_not_imp_error(),
                "Signature has no /Contents blob — nothing to verify",
            ));
        };
        match verify_signer(contents) {
            Ok(SignerVerify::Valid) => Ok(true),
            Ok(SignerVerify::Invalid) => Ok(false),
            Ok(SignerVerify::Unknown) => Err(Error::new(
                ruby.exception_not_imp_error(),
                "Signature#verify: signer uses RSA-PSS, ECDSA, an unknown digest OID, \
                 or the CMS blob lacks signed_attrs",
            )),
            Err(e) => Err(Error::new(
                ruby.exception_arg_error(),
                format!("Signature#verify: failed to parse /Contents as CMS: {e}"),
            )),
        }
    }
    fn verify_detached(ruby: &Ruby, rb_self: &Self, pdf_data: RString) -> Result<bool, Error> {
        use pdf_oxide::signatures::{verify_signer_detached, ByteRangeCalculator, SignerVerify};
        let Some(contents) = rb_self.0.contents() else {
            return Err(Error::new(
                ruby.exception_not_imp_error(),
                "Signature has no /Contents blob — nothing to verify",
            ));
        };
        let br = rb_self.0.byte_range();
        if br.len() != 4 {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "Signature has no /ByteRange — cannot extract signed bytes",
            ));
        }
        let byte_range: [i64; 4] = [br[0], br[1], br[2], br[3]];
        let data = unsafe { pdf_data.as_slice() }.to_vec();
        let signed_bytes = ByteRangeCalculator::extract_signed_bytes(&data, &byte_range)
            .map_err(|e| {
                Error::new(
                    ruby.exception_arg_error(),
                    format!("Failed to extract signed bytes: {e}"),
                )
            })?;
        match verify_signer_detached(contents, &signed_bytes) {
            Ok(SignerVerify::Valid) => Ok(true),
            Ok(SignerVerify::Invalid) => Ok(false),
            Ok(SignerVerify::Unknown) => Err(Error::new(
                ruby.exception_not_imp_error(),
                "Signature#verify_detached: signer uses RSA-PSS, ECDSA, an unknown \
                 digest, or the CMS blob lacks signed_attrs / messageDigest",
            )),
            Err(e) => Err(Error::new(
                ruby.exception_arg_error(),
                format!("Signature#verify_detached: {e}"),
            )),
        }
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::Signature signer_name={:?} reason={:?} location={:?}>",
            self.0.signer_name, self.0.reason, self.0.location
        )
    }
}

/// `PdfDioxide::Certificate` — signing credentials for PAdES signing.
#[magnus::wrap(class = "PdfDioxide::Certificate", free_immediately, size)]
struct RbCertificate(pdf_oxide::signatures::SigningCredentials);

impl RbCertificate {
    fn load(ruby: &Ruby, data: RString) -> Result<Self, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        if bytes.is_empty() {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "Certificate data must not be empty",
            ));
        }
        pdf_oxide::signatures::SigningCredentials::from_der(bytes)
            .map(Self)
            .map_err(|e| {
                Error::new(ruby.exception_arg_error(), format!("Invalid certificate: {e}"))
            })
    }
    fn load_pem(ruby: &Ruby, cert_pem: String, key_pem: String) -> Result<Self, Error> {
        pdf_oxide::signatures::SigningCredentials::from_pem(&cert_pem, &key_pem)
            .map(Self)
            .map_err(|e| {
                Error::new(
                    ruby.exception_arg_error(),
                    format!("Failed to load PEM credentials: {e}"),
                )
            })
    }
    fn load_pkcs12(ruby: &Ruby, data: RString, password: String) -> Result<Self, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        if bytes.is_empty() {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "PKCS#12 data must not be empty",
            ));
        }
        pdf_oxide::signatures::SigningCredentials::from_pkcs12(&bytes, &password)
            .map(Self)
            .map_err(|e| {
                Error::new(ruby.exception_arg_error(), format!("Failed to load PKCS#12: {e}"))
            })
    }
    fn subject(ruby: &Ruby, rb_self: &Self) -> Result<String, Error> {
        rb_self
            .0
            .subject()
            .map_err(|e| Error::new(ruby.exception_arg_error(), format!("{e}")))
    }
    fn issuer(ruby: &Ruby, rb_self: &Self) -> Result<String, Error> {
        rb_self
            .0
            .issuer()
            .map_err(|e| Error::new(ruby.exception_arg_error(), format!("{e}")))
    }
    fn serial(ruby: &Ruby, rb_self: &Self) -> Result<String, Error> {
        rb_self
            .0
            .serial()
            .map_err(|e| Error::new(ruby.exception_arg_error(), format!("{e}")))
    }
    fn validity(ruby: &Ruby, rb_self: &Self) -> Result<(i64, i64), Error> {
        rb_self
            .0
            .validity()
            .map_err(|e| Error::new(ruby.exception_arg_error(), format!("{e}")))
    }
    fn is_valid(ruby: &Ruby, rb_self: &Self) -> Result<bool, Error> {
        rb_self
            .0
            .is_valid()
            .map_err(|e| Error::new(ruby.exception_arg_error(), format!("{e}")))
    }
    fn inspect(&self) -> String {
        let subject = self.0.subject().unwrap_or_else(|_| "<unreadable>".into());
        let serial = self.0.serial().unwrap_or_else(|_| "<unreadable>".into());
        format!("#<PdfDioxide::Certificate subject={subject:?} serial={serial:?}>")
    }
}

/// `PdfDioxide::Timestamp` — a parsed RFC 3161 timestamp token.
#[magnus::wrap(class = "PdfDioxide::Timestamp", free_immediately, size)]
struct RbTimestamp(pdf_oxide::signatures::Timestamp);

impl RbTimestamp {
    fn parse(ruby: &Ruby, data: RString) -> Result<Self, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        if bytes.is_empty() {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "Timestamp data must not be empty",
            ));
        }
        pdf_oxide::signatures::Timestamp::from_der(&bytes)
            .map(Self)
            .map_err(|e| Error::new(ruby.exception_arg_error(), format!("Invalid timestamp: {e}")))
    }
    fn time(&self) -> i64 {
        self.0.time()
    }
    fn serial(&self) -> String {
        self.0.serial()
    }
    fn policy_oid(&self) -> String {
        self.0.policy_oid()
    }
    fn tsa_name(&self) -> String {
        self.0.tsa_name()
    }
    fn hash_algorithm(&self) -> i32 {
        self.0.hash_algorithm() as i32
    }
    fn message_imprint(ruby: &Ruby, rb_self: &Self) -> RString {
        ruby.str_from_slice(rb_self.0.message_imprint_ref())
    }
    fn verify(ruby: &Ruby, rb_self: &Self) -> Result<bool, Error> {
        rb_self
            .0
            .verify()
            .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::Timestamp time={} serial={:?} policy_oid={:?}>",
            self.0.time(),
            self.0.serial(),
            self.0.policy_oid()
        )
    }
}

/// `PdfDioxide::Dss` — the document's Document Security Store.
#[magnus::wrap(class = "PdfDioxide::Dss", free_immediately, size)]
struct RbDss(pdf_oxide::signatures::DocumentSecurityStore);

impl RbDss {
    fn certs(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for d in &rb_self.0.certificates {
            out.push(ruby.str_from_slice(d))?;
        }
        Ok(out)
    }
    fn crls(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for d in &rb_self.0.crls {
            out.push(ruby.str_from_slice(d))?;
        }
        Ok(out)
    }
    fn ocsps(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for d in &rb_self.0.ocsp_responses {
            out.push(ruby.str_from_slice(d))?;
        }
        Ok(out)
    }
    fn vri(&self) -> Vec<String> {
        self.0.vri.iter().map(|v| v.signature_digest.clone()).collect()
    }
}

/// `PdfDioxide::RevocationMaterial` — offline validation material for B-LT.
#[magnus::wrap(class = "PdfDioxide::RevocationMaterial", free_immediately, size)]
#[derive(Clone, Default)]
struct RbRevocationMaterial {
    certs: Vec<Vec<u8>>,
    crls: Vec<Vec<u8>>,
    ocsps: Vec<Vec<u8>>,
}

impl RbRevocationMaterial {
    /// `RevocationMaterial.new(certs: [...], crls: [...], ocsps: [...])` —
    /// each entry a DER blob as a binary String.
    fn new(_ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        use magnus::TryConvert;
        type Opts = (
            Option<magnus::RArray>,
            Option<magnus::RArray>,
            Option<magnus::RArray>,
        );
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let kw = get_kwargs::<_, (), Opts, ()>(args.keywords, &[], &["certs", "crls", "ocsps"])?;
        let (certs, crls, ocsps) = kw.optional;
        let to_bytes = |v: Option<magnus::RArray>| -> Result<Vec<Vec<u8>>, Error> {
            let mut out = Vec::new();
            if let Some(ary) = v {
                for item in ary.each() {
                    let s = <RString as TryConvert>::try_convert(item?)?;
                    out.push(unsafe { s.as_slice() }.to_vec());
                }
            }
            Ok(out)
        };
        Ok(Self {
            certs: to_bytes(certs)?,
            crls: to_bytes(crls)?,
            ocsps: to_bytes(ocsps)?,
        })
    }
}

/// `PdfDioxide.sign_pdf_bytes_pades(pdf_data, cert, level, tsa_url: nil,
/// reason: nil, location: nil, revocation: nil) #=> String (signed PDF)`
///
/// `level` is `"B_B"` / `"B_T"` / `"B_LT"`. This build has no RFC 3161
/// client (`tsa-client` feature), so `tsa_url` is accepted but unused —
/// matching python.rs's non-tsa-client branch.
fn sign_pdf_bytes_pades(ruby: &Ruby, args: &[Value]) -> Result<RString, Error> {
    use magnus::TryConvert;
    use pdf_oxide::signatures::{
        sign_pdf_bytes_pades as core_sign, PadesLevel, RevocationMaterial, SignOptions,
    };

    type Opts = (
        Option<String>, // tsa_url (unused without the tsa-client feature)
        Option<String>, // reason
        Option<String>, // location
        Option<Value>,  // revocation
    );
    let args = scan_args::<(RString, Value, String), (), (), (), RHash, ()>(args)?;
    let (pdf_data, cert_val, level) = args.required;
    let kw = get_kwargs::<_, (), Opts, ()>(
        args.keywords,
        &[],
        &["tsa_url", "reason", "location", "revocation"],
    )?;
    let (_tsa_url, reason, location, revocation) = kw.optional;

    let cert = <&RbCertificate as TryConvert>::try_convert(cert_val)?;
    let level = match level.as_str() {
        "B_B" => PadesLevel::BB,
        "B_T" => PadesLevel::BT,
        "B_LT" => PadesLevel::BLt,
        "B_LTA" => PadesLevel::BLta,
        other => {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("Unknown PAdES level '{other}'. Use B_B, B_T, B_LT"),
            ))
        },
    };
    let material = match revocation {
        Some(v) if !v.is_nil() => {
            let r = <&RbRevocationMaterial as TryConvert>::try_convert(v)?;
            let mut m = RevocationMaterial::default();
            m.certificates = r.certs.clone();
            m.crls = r.crls.clone();
            m.ocsp_responses = r.ocsps.clone();
            m
        },
        _ => RevocationMaterial::default(),
    };
    let opts = SignOptions {
        reason,
        location,
        ..Default::default()
    };

    let data = unsafe { pdf_data.as_slice() }.to_vec();
    let signed = core_sign(&data, &cert.0, opts, level, None, &material).map_err(|e| {
        Error::new(ruby.exception_arg_error(), format!("sign_pdf_bytes_pades failed: {e}"))
    })?;
    Ok(ruby.str_from_slice(&signed))
}

/// `PdfDioxide.has_document_timestamp(pdf_data) #=> true/false`
fn has_document_timestamp(pdf_data: RString) -> bool {
    let data = unsafe { pdf_data.as_slice() }.to_vec();
    pdf_oxide::signatures::has_document_timestamp(&data)
}

/// `PdfDioxide.sign_pdf_bytes(pdf_data, cert, reason: nil, location: nil)
/// #=> String (signed PDF)` — plain (non-PAdES) CMS signature.
fn sign_pdf_bytes(ruby: &Ruby, args: &[Value]) -> Result<RString, Error> {
    use magnus::TryConvert;
    use pdf_oxide::signatures::{sign_pdf_bytes as core_sign, SignOptions};

    let args = scan_args::<(RString, Value), (), (), (), RHash, ()>(args)?;
    let (pdf_data, cert_val) = args.required;
    let kw = get_kwargs::<_, (), (Option<String>, Option<String>), ()>(
        args.keywords,
        &[],
        &["reason", "location"],
    )?;
    let (reason, location) = kw.optional;

    let cert = <&RbCertificate as TryConvert>::try_convert(cert_val)?;
    let opts = SignOptions {
        reason,
        location,
        ..Default::default()
    };
    let data = unsafe { pdf_data.as_slice() }.to_vec();
    core_sign(&data, &cert.0, opts)
        .map(|b| ruby.str_from_slice(&b))
        .map_err(|e| {
            Error::new(ruby.exception_arg_error(), format!("sign_pdf_bytes failed: {e}"))
        })
}

// ---------------------------------------------------------------------------
// Bookmark splitting
// ---------------------------------------------------------------------------

fn split_opts_from_kwargs(
    kw: RHash,
) -> Result<pdf_oxide::split_bookmarks::SplitByBookmarksOptions, Error> {
    type Opts = (Option<String>, Option<bool>, Option<u32>, Option<bool>);
    let kw = get_kwargs::<_, (), Opts, ()>(
        kw,
        &[],
        &["title_prefix", "ignore_case", "level", "include_front_matter"],
    )?;
    let (title_prefix, ignore_case, level, include_front_matter) = kw.optional;
    Ok(pdf_oxide::split_bookmarks::SplitByBookmarksOptions {
        title_prefix,
        ignore_case: ignore_case.unwrap_or(false),
        level: pdf_oxide::split_bookmarks::BookmarkLevel::from_u32(level.unwrap_or(1)),
        include_front_matter: include_front_matter.unwrap_or(true),
        ..Default::default()
    })
}

fn segment_to_hash(
    ruby: &Ruby,
    seg: &pdf_oxide::split_bookmarks::BookmarkSegment,
) -> Result<RHash, Error> {
    let h = ruby.hash_new();
    h.aset("index", seg.index)?;
    h.aset("start_page", seg.start_page)?;
    h.aset("end_page", seg.end_page)?;
    h.aset("title", seg.title.clone())?;
    h.aset("file_stem", seg.file_stem.as_str())?;
    h.aset("page_label", seg.page_label.clone())?;
    Ok(h)
}

/// `PdfDioxide.plan_split_by_bookmarks(src_bytes, title_prefix: nil,
/// ignore_case: false, level: 1, include_front_matter: true)
/// #=> Array<Hash>` — plan only, no PDFs produced.
fn plan_split_by_bookmarks(ruby: &Ruby, args: &[Value]) -> Result<magnus::RArray, Error> {
    let args = scan_args::<(RString,), (), (), (), RHash, ()>(args)?;
    let (src,) = args.required;
    let opts = split_opts_from_kwargs(args.keywords)?;

    let data = unsafe { src.as_slice() }.to_vec();
    let doc = PdfDocument::from_bytes(data).map_err(|e| map_pdf_error(ruby, e))?;
    let segs = pdf_oxide::split_bookmarks::plan_split_by_bookmarks(&doc, &opts)
        .map_err(|e| map_pdf_error(ruby, e))?;
    let out = ruby.ary_new();
    for s in &segs {
        out.push(segment_to_hash(ruby, s)?)?;
    }
    Ok(out)
}

/// `PdfDioxide.split_by_bookmarks(src_bytes, **opts) #=> Array<[Hash, String]>`
/// — each segment's metadata paired with its PDF bytes.
fn split_by_bookmarks(ruby: &Ruby, args: &[Value]) -> Result<magnus::RArray, Error> {
    let args = scan_args::<(RString,), (), (), (), RHash, ()>(args)?;
    let (src,) = args.required;
    let opts = split_opts_from_kwargs(args.keywords)?;

    let data = unsafe { src.as_slice() }.to_vec();
    let parts = pdf_oxide::split_bookmarks::split_by_bookmarks_to_bytes(&data, &opts)
        .map_err(|e| map_pdf_error(ruby, e))?;
    let out = ruby.ary_new();
    for (seg, blob) in &parts {
        let pair = ruby.ary_new();
        pair.push(segment_to_hash(ruby, seg)?)?;
        pair.push(ruby.str_from_slice(blob))?;
        out.push(pair)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// OCR model provisioning (works without the `ocr` feature: downloads are
// disabled but the manifest and cache-dir paths still function)
// ---------------------------------------------------------------------------

/// `PdfDioxide.prefetch_models(["en", "ja"]) #=> String (cache dir)`
fn prefetch_models(ruby: &Ruby, languages: Vec<String>) -> Result<String, Error> {
    use pdf_oxide::extractors::auto::{AutoExtractor, OcrLanguage};
    let langs: Vec<OcrLanguage> = languages
        .iter()
        .filter_map(|s| OcrLanguage::from_code(s.trim()))
        .collect();
    let want = if langs.is_empty() {
        vec![OcrLanguage::English]
    } else {
        langs
    };
    AutoExtractor::prefetch_models(&want)
        .map(|dir| dir.to_string_lossy().into_owned())
        .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
}

/// `PdfDioxide.model_manifest #=> String (JSON)`
fn model_manifest() -> String {
    pdf_oxide::extractors::auto::AutoExtractor::model_manifest()
}

/// `PdfDioxide.prefetch_available #=> true/false` — whether this build can
/// actually download OCR models (`ocr` feature).
fn prefetch_available() -> bool {
    pdf_oxide::extractors::auto::AutoExtractor::prefetch_available()
}

// ---------------------------------------------------------------------------
// Crypto governance
// ---------------------------------------------------------------------------

/// `PdfDioxide.crypto_active_provider #=> String` (non-initializing).
fn crypto_active_provider() -> String {
    use pdf_oxide::crypto::CryptoProvider;
    if pdf_oxide::crypto::is_set() {
        pdf_oxide::crypto::active().name().to_string()
    } else {
        format!("{} (default, lazy)", pdf_oxide::crypto::RustCryptoProvider.name())
    }
}

/// `PdfDioxide.crypto_available_providers #=> Array<String>`
fn crypto_available_providers() -> Vec<String> {
    // This build has no `fips` feature (mutually exclusive with the
    // default `legacy-crypto`), so only the permissive provider exists.
    vec!["rust-crypto".to_string()]
}

/// `PdfDioxide.crypto_use_fips` — always raises here: the gem is built with
/// `legacy-crypto` (default), which is mutually exclusive with `fips`.
fn crypto_use_fips(ruby: &Ruby) -> Result<(), Error> {
    Err(Error::new(
        ruby.get_inner(&ERROR),
        "FIPS provider not compiled in; build the extension with --features fips",
    ))
}

/// `PdfDioxide.crypto_set_policy("compat;deny:rc4@write")` — set-once.
fn crypto_set_policy(ruby: &Ruby, spec: String) -> Result<(), Error> {
    let policy: pdf_oxide::crypto::SecurityPolicy = spec
        .parse()
        .map_err(|e: pdf_oxide::crypto::PolicyParseError| {
            Error::new(ruby.get_inner(&ERROR), e.to_string())
        })?;
    pdf_oxide::crypto::set_policy(policy)
        .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
}

/// `PdfDioxide.crypto_policy #=> String` — the active policy grammar string.
fn crypto_policy() -> String {
    pdf_oxide::crypto::active_policy().to_string()
}

/// `PdfDioxide.crypto_inventory #=> Array<String>` — algorithms exercised so
/// far this process.
fn crypto_inventory() -> Vec<String> {
    pdf_oxide::crypto::inventory()
        .into_iter()
        .map(|a| a.token().to_string())
        .collect()
}

/// `PdfDioxide.crypto_cbom #=> String (JSON)` — CBOM-adjacent report.
fn crypto_cbom() -> String {
    pdf_oxide::crypto::cbom_json()
}

// ---------------------------------------------------------------------------
// Module-level utility functions
// ---------------------------------------------------------------------------

/// `PdfDioxide.set_max_ops_per_stream(limit) #=> previous limit | nil`
fn set_max_ops_per_stream(limit: Option<usize>) -> Option<usize> {
    pdf_oxide::content::parser::set_max_ops_per_stream(limit)
}

/// `PdfDioxide.set_preserve_unmapped_glyphs(flag) #=> previous flag`
fn set_preserve_unmapped_glyphs(preserve: bool) -> bool {
    pdf_oxide::extractors::text::set_preserve_unmapped_glyphs(preserve)
}

/// `PdfDioxide.set_log_level("warn")` — gate for pdf_oxide's Rust-side `log`
/// output. (No logger is installed by the gem; this sets the global filter
/// used by whatever logger the host process installs.)
fn set_log_level(ruby: &Ruby, level: String) -> Result<(), Error> {
    use log::LevelFilter;
    let filter = match level.to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        other => {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!(
                    "invalid log level '{other}': expected off, error, warn, info, debug, or trace"
                ),
            ))
        },
    };
    log::set_max_level(filter);
    Ok(())
}

/// `PdfDioxide.get_log_level #=> String`
fn get_log_level() -> &'static str {
    match log::max_level() {
        log::LevelFilter::Off => "off",
        log::LevelFilter::Error => "error",
        log::LevelFilter::Warn => "warn",
        log::LevelFilter::Info => "info",
        log::LevelFilter::Debug => "debug",
        log::LevelFilter::Trace => "trace",
    }
}

/// `PdfDioxide.disable_logging` — convenience for `set_log_level("off")`.
fn disable_logging() {
    log::set_max_level(log::LevelFilter::Off);
}

/// `PdfDioxide.generate_barcode_svg(barcode_type, data) #=> String (SVG)`
/// `barcode_type`: 0=Code128, 1=Code39, 2=EAN13, 3=EAN8, 4=UPCA, 5=ITF,
/// 6=Code93, 7=Codabar.
fn generate_barcode_svg(ruby: &Ruby, barcode_type: i32, data: String) -> Result<String, Error> {
    use pdf_oxide::writer::{BarcodeGenerator, BarcodeOptions, BarcodeType};
    let bt = match barcode_type {
        0 => BarcodeType::Code128,
        1 => BarcodeType::Code39,
        2 => BarcodeType::Ean13,
        3 => BarcodeType::Ean8,
        4 => BarcodeType::UpcA,
        5 => BarcodeType::Itf,
        6 => BarcodeType::Code93,
        7 => BarcodeType::Codabar,
        _ => {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("unknown barcode_type {barcode_type}; valid values are 0-7"),
            ))
        },
    };
    BarcodeGenerator::generate_1d_svg(bt, &data, &BarcodeOptions::default())
        .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
}

/// `PdfDioxide.generate_qr_svg(data, error_correction, size) #=> String (SVG)`
/// `error_correction`: 0=Low, 1=Medium, 2=Quartile, 3=High.
fn generate_qr_svg(
    ruby: &Ruby,
    data: String,
    error_correction: i32,
    size: u32,
) -> Result<String, Error> {
    use pdf_oxide::writer::{BarcodeGenerator, QrCodeOptions, QrErrorCorrection};
    let ec = match error_correction {
        0 => QrErrorCorrection::Low,
        2 => QrErrorCorrection::Quartile,
        3 => QrErrorCorrection::High,
        _ => QrErrorCorrection::Medium,
    };
    let opts = QrCodeOptions::new().size(size).error_correction(ec);
    BarcodeGenerator::generate_qr_svg(&data, &opts)
        .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
}

// ---------------------------------------------------------------------------
// Writer value classes (Color / BlendMode / ExtGState / gradients / line
// styles / patterns / artifacts / page template)
// ---------------------------------------------------------------------------

/// `PdfDioxide::Color` — RGB color in 0.0..=1.0 components.
#[magnus::wrap(class = "PdfDioxide::Color", free_immediately, size)]
#[derive(Clone)]
struct RbColor(pdf_oxide::layout::Color);

impl RbColor {
    fn new(r: f32, g: f32, b: f32) -> Self {
        Self(pdf_oxide::layout::Color::new(r, g, b))
    }
    fn black() -> Self {
        Self(pdf_oxide::layout::Color::black())
    }
    fn white() -> Self {
        Self(pdf_oxide::layout::Color::white())
    }
    fn red() -> Self {
        Self::new(1.0, 0.0, 0.0)
    }
    fn green() -> Self {
        Self::new(0.0, 1.0, 0.0)
    }
    fn blue() -> Self {
        Self::new(0.0, 0.0, 1.0)
    }
    fn from_hex(ruby: &Ruby, hex: String) -> Result<Self, Error> {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return Err(Error::new(ruby.exception_arg_error(), "Invalid hex color length"));
        }
        let parse = |s: &str| -> Result<f32, Error> {
            u8::from_str_radix(s, 16)
                .map(|v| v as f32 / 255.0)
                .map_err(|_| Error::new(ruby.exception_arg_error(), "Invalid hex color"))
        };
        Ok(Self(pdf_oxide::layout::Color::new(
            parse(&hex[0..2])?,
            parse(&hex[2..4])?,
            parse(&hex[4..6])?,
        )))
    }
    fn r(&self) -> f32 {
        self.0.r
    }
    fn g(&self) -> f32 {
        self.0.g
    }
    fn b(&self) -> f32 {
        self.0.b
    }
}

/// `PdfDioxide::BlendMode` — construct via lowercase class methods
/// (`BlendMode.multiply`, ...); Python's SCREAMING_CASE statics map to
/// Ruby-idiomatic lowercase.
#[magnus::wrap(class = "PdfDioxide::BlendMode", free_immediately, size)]
#[derive(Clone)]
struct RbBlendMode(pdf_oxide::writer::BlendMode);

impl RbBlendMode {
    fn make(mode: pdf_oxide::writer::BlendMode) -> Self {
        Self(mode)
    }
}

/// `PdfDioxide::ExtGState` — immutable builder for transparency state; every
/// method returns a new instance.
#[magnus::wrap(class = "PdfDioxide::ExtGState", free_immediately, size)]
#[derive(Clone)]
struct RbExtGState {
    fill_alpha: Option<f32>,
    stroke_alpha: Option<f32>,
    blend_mode: Option<pdf_oxide::writer::BlendMode>,
}

impl RbExtGState {
    fn new() -> Self {
        Self {
            fill_alpha: None,
            stroke_alpha: None,
            blend_mode: None,
        }
    }
    fn alpha(&self, a: f32) -> Self {
        let v = Some(a.clamp(0.0, 1.0));
        Self {
            fill_alpha: v,
            stroke_alpha: v,
            blend_mode: self.blend_mode,
        }
    }
    fn fill_alpha(&self, a: f32) -> Self {
        Self {
            fill_alpha: Some(a.clamp(0.0, 1.0)),
            ..self.clone()
        }
    }
    fn stroke_alpha(&self, a: f32) -> Self {
        Self {
            stroke_alpha: Some(a.clamp(0.0, 1.0)),
            ..self.clone()
        }
    }
    fn blend_mode(&self, mode: &RbBlendMode) -> Self {
        Self {
            blend_mode: Some(mode.0),
            ..self.clone()
        }
    }
    fn semi_transparent() -> Self {
        Self {
            fill_alpha: Some(0.5),
            stroke_alpha: Some(0.5),
            blend_mode: None,
        }
    }
}

/// `PdfDioxide::LinearGradient` — immutable builder; methods return new
/// instances.
#[magnus::wrap(class = "PdfDioxide::LinearGradient", free_immediately, size)]
#[derive(Clone)]
struct RbLinearGradient {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    stops: Vec<(f32, pdf_oxide::layout::Color)>,
}

impl RbLinearGradient {
    fn new() -> Self {
        Self {
            x1: 0.0,
            y1: 0.0,
            x2: 100.0,
            y2: 100.0,
            stops: Vec::new(),
        }
    }
    fn start(&self, x: f32, y: f32) -> Self {
        Self {
            x1: x,
            y1: y,
            ..self.clone()
        }
    }
    fn end(&self, x: f32, y: f32) -> Self {
        Self {
            x2: x,
            y2: y,
            ..self.clone()
        }
    }
    fn add_stop(&self, offset: f32, color: &RbColor) -> Self {
        let mut out = self.clone();
        out.stops.push((offset, color.0));
        out
    }
    fn horizontal(width: f32, start: &RbColor, end: &RbColor) -> Self {
        Self {
            x1: 0.0,
            y1: 0.0,
            x2: width,
            y2: 0.0,
            stops: vec![(0.0, start.0), (1.0, end.0)],
        }
    }
    fn vertical(height: f32, start: &RbColor, end: &RbColor) -> Self {
        Self {
            x1: 0.0,
            y1: 0.0,
            x2: 0.0,
            y2: height,
            stops: vec![(0.0, start.0), (1.0, end.0)],
        }
    }
}

/// `PdfDioxide::RadialGradient` — immutable builder; methods return new
/// instances.
#[magnus::wrap(class = "PdfDioxide::RadialGradient", free_immediately, size)]
#[derive(Clone)]
struct RbRadialGradient {
    x1: f32,
    y1: f32,
    r1: f32,
    x2: f32,
    y2: f32,
    r2: f32,
    stops: Vec<(f32, pdf_oxide::layout::Color)>,
}

impl RbRadialGradient {
    fn new() -> Self {
        Self {
            x1: 50.0,
            y1: 50.0,
            r1: 0.0,
            x2: 50.0,
            y2: 50.0,
            r2: 50.0,
            stops: Vec::new(),
        }
    }
    fn inner_circle(&self, x: f32, y: f32, r: f32) -> Self {
        Self {
            x1: x,
            y1: y,
            r1: r,
            ..self.clone()
        }
    }
    fn outer_circle(&self, x: f32, y: f32, r: f32) -> Self {
        Self {
            x2: x,
            y2: y,
            r2: r,
            ..self.clone()
        }
    }
    fn add_stop(&self, offset: f32, color: &RbColor) -> Self {
        let mut out = self.clone();
        out.stops.push((offset, color.0));
        out
    }
    fn centered(x: f32, y: f32, radius: f32) -> Self {
        Self {
            x1: x,
            y1: y,
            r1: 0.0,
            x2: x,
            y2: y,
            r2: radius,
            stops: Vec::new(),
        }
    }
}

/// `PdfDioxide::LineCap` — `butt` / `round` / `square`.
#[magnus::wrap(class = "PdfDioxide::LineCap", free_immediately, size)]
#[derive(Clone)]
struct RbLineCap(pdf_oxide::writer::LineCap);

/// `PdfDioxide::LineJoin` — `miter` / `round` / `bevel`.
#[magnus::wrap(class = "PdfDioxide::LineJoin", free_immediately, size)]
#[derive(Clone)]
struct RbLineJoin(pdf_oxide::writer::LineJoin);

impl RbLineCap {
    /// Tells a value read back from the editor's settings apart in a REPL
    /// (upstream's `LineCap.__repr__`).
    fn inspect(&self) -> String {
        format!("#<PdfDioxide::LineCap {:?}>", self.0)
    }
}

impl RbLineJoin {
    /// See `RbLineCap::inspect` (upstream's `LineJoin.__repr__`).
    fn inspect(&self) -> String {
        format!("#<PdfDioxide::LineJoin {:?}>", self.0)
    }
}

/// `PdfDioxide::PatternPresets` — static generators returning raw pattern
/// content-stream bytes as binary Strings.
#[magnus::wrap(class = "PdfDioxide::PatternPresets", free_immediately, size)]
#[derive(Clone)]
struct RbPatternPresets;

impl RbPatternPresets {
    fn horizontal_stripes(
        ruby: &Ruby,
        width: f32,
        height: f32,
        stripe_height: f32,
        color: &RbColor,
    ) -> RString {
        ruby.str_from_slice(&pdf_oxide::writer::PatternPresets::horizontal_stripes(
            width,
            height,
            stripe_height,
            color.0,
        ))
    }
    fn vertical_stripes(
        ruby: &Ruby,
        width: f32,
        height: f32,
        stripe_width: f32,
        color: &RbColor,
    ) -> RString {
        ruby.str_from_slice(&pdf_oxide::writer::PatternPresets::vertical_stripes(
            width,
            height,
            stripe_width,
            color.0,
        ))
    }
    fn checkerboard(ruby: &Ruby, size: f32, color1: &RbColor, color2: &RbColor) -> RString {
        ruby.str_from_slice(&pdf_oxide::writer::PatternPresets::checkerboard(
            size, color1.0, color2.0,
        ))
    }
    fn dots(ruby: &Ruby, spacing: f32, radius: f32, color: &RbColor) -> RString {
        ruby.str_from_slice(&pdf_oxide::writer::PatternPresets::dots(spacing, radius, color.0))
    }
    fn diagonal_lines(ruby: &Ruby, size: f32, line_width: f32, color: &RbColor) -> RString {
        ruby.str_from_slice(&pdf_oxide::writer::PatternPresets::diagonal_lines(
            size, line_width, color.0,
        ))
    }
    fn crosshatch(ruby: &Ruby, size: f32, line_width: f32, color: &RbColor) -> RString {
        ruby.str_from_slice(&pdf_oxide::writer::PatternPresets::crosshatch(
            size, line_width, color.0,
        ))
    }
}

/// `PdfDioxide::ArtifactStyle` — fluent (mutates and returns self).
#[magnus::wrap(class = "PdfDioxide::ArtifactStyle", free_immediately, size)]
struct RbArtifactStyle(RefCell<pdf_oxide::writer::ArtifactStyle>);

impl RbArtifactStyle {
    fn new() -> Self {
        Self(RefCell::new(pdf_oxide::writer::ArtifactStyle::default()))
    }
    fn font(rb_self: Value, name: String, size: f32) -> Result<Value, Error> {
        use magnus::TryConvert;
        let this = <&RbArtifactStyle as TryConvert>::try_convert(rb_self)?;
        let updated = this.0.borrow().clone().font(&name, size);
        *this.0.borrow_mut() = updated;
        Ok(rb_self)
    }
    fn bold(rb_self: Value) -> Result<Value, Error> {
        use magnus::TryConvert;
        let this = <&RbArtifactStyle as TryConvert>::try_convert(rb_self)?;
        let updated = this.0.borrow().clone().bold();
        *this.0.borrow_mut() = updated;
        Ok(rb_self)
    }
}

/// `PdfDioxide::Artifact` — running header/footer content; fluent.
#[magnus::wrap(class = "PdfDioxide::Artifact", free_immediately, size)]
struct RbArtifact(RefCell<pdf_oxide::writer::Artifact>);

impl RbArtifact {
    fn new() -> Self {
        Self(RefCell::new(pdf_oxide::writer::Artifact::new()))
    }
    fn center(t: String) -> Self {
        Self(RefCell::new(pdf_oxide::writer::Artifact::center(&t)))
    }
    fn with_left(rb_self: Value, t: String) -> Result<Value, Error> {
        use magnus::TryConvert;
        let this = <&RbArtifact as TryConvert>::try_convert(rb_self)?;
        let updated = this.0.borrow().clone().with_left(&t);
        *this.0.borrow_mut() = updated;
        Ok(rb_self)
    }
}

/// `PdfDioxide::Header` — an Artifact placed as a page header.
#[magnus::wrap(class = "PdfDioxide::Header", free_immediately, size)]
struct RbHeader(pdf_oxide::writer::Artifact);

impl RbHeader {
    fn new() -> Self {
        Self(pdf_oxide::writer::Artifact::new())
    }
    fn center(t: String) -> Self {
        Self(pdf_oxide::writer::Artifact::center(&t))
    }
}

/// `PdfDioxide::Footer` — an Artifact placed as a page footer.
#[magnus::wrap(class = "PdfDioxide::Footer", free_immediately, size)]
struct RbFooter(pdf_oxide::writer::Artifact);

impl RbFooter {
    fn new() -> Self {
        Self(pdf_oxide::writer::Artifact::new())
    }
    fn center(t: String) -> Self {
        Self(pdf_oxide::writer::Artifact::center(&t))
    }
}

/// `PdfDioxide::PageTemplate` — header/footer template for `Pdf` conversion
/// entry points; fluent.
#[magnus::wrap(class = "PdfDioxide::PageTemplate", free_immediately, size)]
struct RbPageTemplate(RefCell<pdf_oxide::writer::PageTemplate>);

impl RbPageTemplate {
    fn new() -> Self {
        Self(RefCell::new(pdf_oxide::writer::PageTemplate::new()))
    }
    fn artifact_from(value: Value) -> Result<pdf_oxide::writer::Artifact, Error> {
        use magnus::TryConvert;
        if let Ok(h) = <&RbHeader as TryConvert>::try_convert(value) {
            Ok(h.0.clone())
        } else if let Ok(f) = <&RbFooter as TryConvert>::try_convert(value) {
            Ok(f.0.clone())
        } else {
            let a = <&RbArtifact as TryConvert>::try_convert(value)?;
            let out = a.0.borrow().clone();
            Ok(out)
        }
    }
    fn header(rb_self: Value, h: Value) -> Result<Value, Error> {
        use magnus::TryConvert;
        let this = <&RbPageTemplate as TryConvert>::try_convert(rb_self)?;
        let artifact = Self::artifact_from(h)?;
        let updated = this.0.borrow().clone().header(artifact);
        *this.0.borrow_mut() = updated;
        Ok(rb_self)
    }
    fn footer(rb_self: Value, f: Value) -> Result<Value, Error> {
        use magnus::TryConvert;
        let this = <&RbPageTemplate as TryConvert>::try_convert(rb_self)?;
        let artifact = Self::artifact_from(f)?;
        let updated = this.0.borrow().clone().footer(artifact);
        *this.0.borrow_mut() = updated;
        Ok(rb_self)
    }
}

// ---------------------------------------------------------------------------
// Table building blocks (Column / Table)
// ---------------------------------------------------------------------------

/// Accept `"left"`/`"center"`/`"right"` (String or Symbol), an Integer 0..=2,
/// or nil (left) for an alignment argument.
fn align_from_value(ruby: &Ruby, value: Option<Value>) -> Result<i32, Error> {
    use magnus::TryConvert;
    let Some(v) = value else { return Ok(0) };
    if v.is_nil() {
        return Ok(0);
    }
    let name = if let Ok(s) = <String as TryConvert>::try_convert(v) {
        Some(s)
    } else {
        <magnus::Symbol as TryConvert>::try_convert(v)
            .ok()
            .map(|sym| sym.name().map(|n| n.to_string()))
            .transpose()?
    };
    if let Some(name) = name {
        return match name.to_ascii_lowercase().as_str() {
            "left" => Ok(0),
            "center" | "centre" => Ok(1),
            "right" => Ok(2),
            _ => Err(Error::new(
                ruby.exception_arg_error(),
                "align must be 'left'/'center'/'right' or 0..2",
            )),
        };
    }
    if let Ok(i) = <i32 as TryConvert>::try_convert(v) {
        if (0..=2).contains(&i) {
            return Ok(i);
        }
    }
    Err(Error::new(
        ruby.exception_arg_error(),
        "align must be 'left'/'center'/'right' or 0..2",
    ))
}

/// `PdfDioxide::Column` — column descriptor for `Table`.
#[magnus::wrap(class = "PdfDioxide::Column", free_immediately, size)]
#[derive(Clone)]
struct RbColumn {
    header: String,
    width: f32,
    align: i32,
}

impl RbColumn {
    /// `Column.new(header, width = 100.0, align = :left)`
    fn new(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(String,), (Option<f32>, Option<Value>), (), (), (), ()>(args)?;
        let (header,) = args.required;
        let (width, align) = args.optional;
        Ok(Self {
            header,
            width: width.unwrap_or(100.0),
            align: align_from_value(ruby, align)?,
        })
    }
    fn header(&self) -> String {
        self.header.clone()
    }
    fn width(&self) -> f32 {
        self.width
    }
    fn align(&self) -> i32 {
        self.align
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::Column header={:?} width={} align={}>",
            self.header, self.width, self.align
        )
    }
}

/// `PdfDioxide::Table` — buffered table value object for the page builder.
#[magnus::wrap(class = "PdfDioxide::Table", free_immediately, size)]
#[derive(Clone)]
struct RbTable {
    columns: Vec<RbColumn>,
    rows: Vec<Vec<String>>,
    has_header: bool,
}

impl RbTable {
    /// `Table.new(columns, rows, has_header = false)`
    fn new(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        use magnus::TryConvert;
        let args =
            scan_args::<(magnus::RArray, Vec<Vec<String>>), (Option<bool>,), (), (), (), ()>(args)?;
        let (columns_ary, rows) = args.required;
        let has_header = args.optional.0.unwrap_or(false);

        let mut columns = Vec::new();
        for item in columns_ary.each() {
            let col = <&RbColumn as TryConvert>::try_convert(item?)?;
            columns.push(col.clone());
        }
        if columns.is_empty() {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "Table requires at least one Column",
            ));
        }
        for (i, row) in rows.iter().enumerate() {
            if row.len() != columns.len() {
                return Err(Error::new(
                    ruby.exception_arg_error(),
                    format!("Table row {i} has {} cells, expected {}", row.len(), columns.len()),
                ));
            }
        }
        Ok(Self {
            columns,
            rows,
            has_header,
        })
    }
    fn inspect(&self) -> String {
        format!(
            "#<PdfDioxide::Table columns={} rows={} has_header={}>",
            self.columns.len(),
            self.rows.len(),
            self.has_header
        )
    }
}

// ---------------------------------------------------------------------------
// PDF creation (Pdf / OfficeConverter / EmbeddedFont / DocumentBuilder)
// ---------------------------------------------------------------------------

/// `PdfDioxide::Pdf` — a finished PDF byte buffer produced by the conversion
/// entry points (`from_markdown`, `from_html`, `from_image`, ...).
#[magnus::wrap(class = "PdfDioxide::Pdf", free_immediately, size)]
struct RbPdf(Vec<u8>);

impl RbPdf {
    fn builder_with_meta(
        title: Option<String>,
        author: Option<String>,
    ) -> pdf_oxide::api::PdfBuilder {
        let mut b = pdf_oxide::api::PdfBuilder::new();
        if let Some(t) = title {
            b = b.title(t);
        }
        if let Some(a) = author {
            b = b.author(a);
        }
        b
    }

    fn meta_kwargs(kw: RHash) -> Result<(Option<String>, Option<String>), Error> {
        let kw = get_kwargs::<_, (), (Option<String>, Option<String>), ()>(
            kw,
            &[],
            &["title", "author"],
        )?;
        Ok(kw.optional)
    }

    /// `Pdf.from_markdown(content, title: nil, author: nil)`
    fn from_markdown(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(String,), (), (), (), RHash, ()>(args)?;
        let (content,) = args.required;
        let (title, author) = Self::meta_kwargs(args.keywords)?;
        Self::builder_with_meta(title, author)
            .from_markdown(&content)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_html(content, title: nil, author: nil)`
    fn from_html(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(String,), (), (), (), RHash, ()>(args)?;
        let (content,) = args.required;
        let (title, author) = Self::meta_kwargs(args.keywords)?;
        Self::builder_with_meta(title, author)
            .from_html(&content)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_text(content, title: nil, author: nil)`
    fn from_text(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(String,), (), (), (), RHash, ()>(args)?;
        let (content,) = args.required;
        let (title, author) = Self::meta_kwargs(args.keywords)?;
        Self::builder_with_meta(title, author)
            .from_text(&content)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_markdown_with_template(content, template, title: nil,
    /// author: nil)`
    fn from_markdown_with_template(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        use magnus::TryConvert;
        let args = scan_args::<(String, Value), (), (), (), RHash, ()>(args)?;
        let (content, template_val) = args.required;
        let (title, author) = Self::meta_kwargs(args.keywords)?;
        let template = <&RbPageTemplate as TryConvert>::try_convert(template_val)?;
        let template_inner = template.0.borrow().clone();
        Self::builder_with_meta(title, author)
            .template(template_inner)
            .from_markdown(&content)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_html_css(html, css, font_bytes)` — single embedded font.
    fn from_html_css(ruby: &Ruby, html: String, css: String, font: RString) -> Result<Self, Error> {
        let bytes = unsafe { font.as_slice() }.to_vec();
        pdf_oxide::api::Pdf::from_html_css(&html, &css, bytes)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_html_css_with_fonts(html, css, [["Family", font_bytes], ...])`
    fn from_html_css_with_fonts(
        ruby: &Ruby,
        html: String,
        css: String,
        fonts: magnus::RArray,
    ) -> Result<Self, Error> {
        use magnus::TryConvert;
        let mut font_vec: Vec<(String, Vec<u8>)> = Vec::new();
        for item in fonts.each() {
            let pair = <magnus::RArray as TryConvert>::try_convert(item?)?;
            let name = <String as TryConvert>::try_convert(pair.entry(0)?)?;
            let data = <RString as TryConvert>::try_convert(pair.entry(1)?)?;
            font_vec.push((name, unsafe { data.as_slice() }.to_vec()));
        }
        if font_vec.is_empty() {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "at least one font must be provided",
            ));
        }
        pdf_oxide::api::Pdf::from_html_css_with_fonts(&html, &css, font_vec)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_image(path)`
    fn from_image(ruby: &Ruby, path: String) -> Result<Self, Error> {
        pdf_oxide::api::Pdf::from_image(&path)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_images(paths)`
    fn from_images(ruby: &Ruby, paths: Vec<String>) -> Result<Self, Error> {
        pdf_oxide::api::Pdf::from_images(&paths)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_image_bytes(data)`
    fn from_image_bytes(ruby: &Ruby, data: RString) -> Result<Self, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        pdf_oxide::api::Pdf::from_image_bytes(&bytes)
            .map(|pdf| Self(pdf.into_bytes()))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `Pdf.from_bytes(data)` — open an existing PDF for re-serialization.
    fn from_bytes(ruby: &Ruby, data: RString) -> Result<Self, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        let mut pdf = pdf_oxide::api::Pdf::from_bytes(bytes).map_err(|e| map_pdf_error(ruby, e))?;
        let out = pdf.save_to_bytes().map_err(|e| map_pdf_error(ruby, e))?;
        Ok(Self(out))
    }

    /// `Pdf.merge(paths)` — merge several PDF files into one.
    fn merge(ruby: &Ruby, paths: Vec<String>) -> Result<Self, Error> {
        pdf_oxide::api::merge_pdfs(&paths)
            .map(Self)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `pdf.save(path)`
    fn save(ruby: &Ruby, rb_self: &Self, path: String) -> Result<(), Error> {
        std::fs::write(&path, &rb_self.0)
            .map_err(|e| Error::new(ruby.get_inner(&IO_ERROR), e.to_string()))
    }

    /// `pdf.to_bytes #=> String (binary)`
    fn to_bytes(ruby: &Ruby, rb_self: &Self) -> RString {
        ruby.str_from_slice(&rb_self.0)
    }

    fn length(&self) -> usize {
        self.0.len()
    }

    fn inspect(&self) -> String {
        format!("#<PdfDioxide::Pdf {} bytes>", self.0.len())
    }
}

/// `PdfDioxide::OfficeConverter` — Office documents -> `PdfDioxide::Pdf`.
#[magnus::wrap(class = "PdfDioxide::OfficeConverter", free_immediately, size)]
struct RbOfficeConverter;

impl RbOfficeConverter {
    fn new() -> Self {
        Self
    }
    fn from_docx(ruby: &Ruby, path: String) -> Result<RbPdf, Error> {
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert_docx(&path)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn from_docx_bytes(ruby: &Ruby, data: RString) -> Result<RbPdf, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert_docx_bytes(&bytes)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn from_xlsx(ruby: &Ruby, path: String) -> Result<RbPdf, Error> {
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert_xlsx(&path)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn from_xlsx_bytes(ruby: &Ruby, data: RString) -> Result<RbPdf, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert_xlsx_bytes(&bytes)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn from_pptx(ruby: &Ruby, path: String) -> Result<RbPdf, Error> {
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert_pptx(&path)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn from_pptx_bytes(ruby: &Ruby, data: RString) -> Result<RbPdf, Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert_pptx_bytes(&bytes)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn convert(ruby: &Ruby, path: String) -> Result<RbPdf, Error> {
        pdf_oxide::converters::office::OfficeConverter::new()
            .convert(&path)
            .map(RbPdf)
            .map_err(|e| map_pdf_error(ruby, e))
    }
}

/// `PdfDioxide::EmbeddedFont` — one-shot TTF/OTF handle for
/// `DocumentBuilder#register_embedded_font` (consumed on registration).
#[magnus::wrap(class = "PdfDioxide::EmbeddedFont", free_immediately, size)]
struct RbEmbeddedFont(RefCell<Option<pdf_oxide::writer::EmbeddedFont>>);

impl RbEmbeddedFont {
    fn from_file(ruby: &Ruby, path: String) -> Result<Self, Error> {
        pdf_oxide::writer::EmbeddedFont::from_file(&path)
            .map(|f| Self(RefCell::new(Some(f))))
            .map_err(|e| {
                Error::new(ruby.get_inner(&IO_ERROR), format!("failed to load font: {e}"))
            })
    }
    /// `EmbeddedFont.from_bytes(data, name = nil)`
    fn from_bytes(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(RString,), (Option<String>,), (), (), (), ()>(args)?;
        let (data,) = args.required;
        let (name,) = args.optional;
        let bytes = unsafe { data.as_slice() }.to_vec();
        pdf_oxide::writer::EmbeddedFont::from_data(name, bytes)
            .map(|f| Self(RefCell::new(Some(f))))
            .map_err(|e| {
                Error::new(ruby.exception_arg_error(), format!("failed to parse font: {e}"))
            })
    }
    fn name(&self) -> String {
        self.0
            .borrow()
            .as_ref()
            .map(|f| f.name.clone())
            .unwrap_or_default()
    }
    fn inspect(&self) -> String {
        match self.0.borrow().as_ref() {
            Some(f) => format!("#<PdfDioxide::EmbeddedFont {:?}>", f.name),
            None => "#<PdfDioxide::EmbeddedFont (consumed)>".to_string(),
        }
    }
}

/// `PdfDioxide::DocumentBuilder` — fluent PDF-creation API. Methods mutate in
/// place and return self; `build`/`save`/`save_encrypted`/
/// `to_bytes_encrypted` consume the builder (subsequent calls raise
/// `PdfDioxide::Error`).
#[magnus::wrap(class = "PdfDioxide::DocumentBuilder", free_immediately, size)]
struct RbDocumentBuilder(RefCell<Option<pdf_oxide::writer::DocumentBuilder>>);

impl RbDocumentBuilder {
    fn new() -> Self {
        Self(RefCell::new(Some(pdf_oxide::writer::DocumentBuilder::new())))
    }

    fn take_inner(
        &self,
        ruby: &Ruby,
        ctx: &str,
    ) -> Result<pdf_oxide::writer::DocumentBuilder, Error> {
        self.0.borrow_mut().take().ok_or_else(|| {
            Error::new(
                ruby.get_inner(&ERROR),
                format!("DocumentBuilder already consumed ({ctx})"),
            )
        })
    }

    fn with_inner(
        &self,
        ruby: &Ruby,
        ctx: &str,
        f: impl FnOnce(pdf_oxide::writer::DocumentBuilder) -> pdf_oxide::writer::DocumentBuilder,
    ) -> Result<(), Error> {
        let taken = self.take_inner(ruby, ctx)?;
        *self.0.borrow_mut() = Some(f(taken));
        Ok(())
    }

    fn fluent(
        ruby: &Ruby,
        rb_self: Value,
        ctx: &str,
        f: impl FnOnce(pdf_oxide::writer::DocumentBuilder) -> pdf_oxide::writer::DocumentBuilder,
    ) -> Result<Value, Error> {
        use magnus::TryConvert;
        let this = <&RbDocumentBuilder as TryConvert>::try_convert(rb_self)?;
        this.with_inner(ruby, ctx, f)?;
        Ok(rb_self)
    }

    fn title(ruby: &Ruby, rb_self: Value, title: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "title", |b| b.title(title))
    }
    fn author(ruby: &Ruby, rb_self: Value, author: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "author", |b| b.author(author))
    }
    fn subject(ruby: &Ruby, rb_self: Value, subject: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "subject", |b| b.subject(subject))
    }
    fn keywords(ruby: &Ruby, rb_self: Value, keywords: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "keywords", |b| b.keywords(keywords))
    }
    fn creator(ruby: &Ruby, rb_self: Value, creator: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "creator", |b| b.creator(creator))
    }
    fn on_open(ruby: &Ruby, rb_self: Value, script: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "on_open", |b| b.on_open(script))
    }
    fn tagged_pdf_ua1(ruby: &Ruby, rb_self: Value) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "tagged_pdf_ua1", |b| b.tagged_pdf_ua1())
    }
    fn language(ruby: &Ruby, rb_self: Value, lang: String) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "language", |b| b.language(lang))
    }
    fn role_map(
        ruby: &Ruby,
        rb_self: Value,
        custom: String,
        standard: String,
    ) -> Result<Value, Error> {
        Self::fluent(ruby, rb_self, "role_map", |b| b.role_map(custom, standard))
    }
    fn register_embedded_font(
        ruby: &Ruby,
        rb_self: Value,
        name: String,
        font: Value,
    ) -> Result<Value, Error> {
        use magnus::TryConvert;
        let font = <&RbEmbeddedFont as TryConvert>::try_convert(font)?;
        let embedded = font.0.borrow_mut().take().ok_or_else(|| {
            Error::new(ruby.get_inner(&ERROR), "EmbeddedFont already consumed")
        })?;
        Self::fluent(ruby, rb_self, "register_embedded_font", |b| {
            b.register_embedded_font(name, embedded)
        })
    }

    /// `builder.build #=> String (binary PDF)` — consumes the builder.
    fn build(ruby: &Ruby, rb_self: &Self) -> Result<RString, Error> {
        let inner = rb_self.take_inner(ruby, "build")?;
        let bytes = inner
            .build()
            .map_err(|e| Error::new(ruby.get_inner(&ERROR), format!("build failed: {e}")))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    /// `builder.save(path)` — consumes the builder.
    fn save(ruby: &Ruby, rb_self: &Self, path: String) -> Result<(), Error> {
        let inner = rb_self.take_inner(ruby, "save")?;
        inner
            .save(&path)
            .map_err(|e| Error::new(ruby.get_inner(&IO_ERROR), format!("save failed: {e}")))
    }

    /// `builder.save_encrypted(path, user_password, owner_password)` —
    /// AES-256; consumes the builder.
    fn save_encrypted(
        ruby: &Ruby,
        rb_self: &Self,
        path: String,
        user_password: String,
        owner_password: String,
    ) -> Result<(), Error> {
        let inner = rb_self.take_inner(ruby, "save_encrypted")?;
        inner
            .save_encrypted(&path, &user_password, &owner_password)
            .map_err(|e| {
                Error::new(ruby.get_inner(&IO_ERROR), format!("save_encrypted failed: {e}"))
            })
    }

    /// `builder.to_bytes_encrypted(user_password, owner_password)
    /// #=> String (binary)` — consumes the builder.
    fn to_bytes_encrypted(
        ruby: &Ruby,
        rb_self: &Self,
        user_password: String,
        owner_password: String,
    ) -> Result<RString, Error> {
        let inner = rb_self.take_inner(ruby, "to_bytes_encrypted")?;
        let bytes = inner
            .to_bytes_encrypted(&user_password, &owner_password)
            .map_err(|e| {
                Error::new(ruby.get_inner(&ERROR), format!("to_bytes_encrypted failed: {e}"))
            })?;
        Ok(ruby.str_from_slice(&bytes))
    }
}

// ---------------------------------------------------------------------------
// FluentPageBuilder support (replay engine + record-time helpers)
// ---------------------------------------------------------------------------
//
// Architectural note (mirrors python.rs): the Rust `FluentPageBuilder<'a>`
// borrows the `DocumentBuilder`, which a GC'd wrapper object cannot hold.
// python.rs buffers ops in a Rust-side Vec; the Ruby port buffers them in
// PURE RUBY (lib/pdf_oxidized_ruby.rb's FluentPageBuilder) as
// `[op_name, *args]` arrays and commits them here in one shot.

fn parse_stamp_type(name: &str) -> pdf_oxide::writer::StampType {
    use pdf_oxide::writer::StampType;
    match name {
        "Approved" => StampType::Approved,
        "Experimental" => StampType::Experimental,
        "NotApproved" => StampType::NotApproved,
        "AsIs" => StampType::AsIs,
        "Expired" => StampType::Expired,
        "NotForPublicRelease" => StampType::NotForPublicRelease,
        "Confidential" => StampType::Confidential,
        "Final" => StampType::Final,
        "Sold" => StampType::Sold,
        "Departmental" => StampType::Departmental,
        "ForComment" => StampType::ForComment,
        "TopSecret" => StampType::TopSecret,
        "Draft" => StampType::Draft,
        "ForPublicRelease" => StampType::ForPublicRelease,
        other => StampType::Custom(other.to_string()),
    }
}

fn cell_align(i: i32) -> pdf_oxide::writer::CellAlign {
    match i {
        1 => pdf_oxide::writer::CellAlign::Center,
        2 => pdf_oxide::writer::CellAlign::Right,
        _ => pdf_oxide::writer::CellAlign::Left,
    }
}

fn text_align(i: i32) -> pdf_oxide::writer::TextAlign {
    match i {
        1 => pdf_oxide::writer::TextAlign::Center,
        2 => pdf_oxide::writer::TextAlign::Right,
        _ => pdf_oxide::writer::TextAlign::Left,
    }
}

/// Positional-argument accessor for a buffered `[op_name, *args]` Array.
struct OpArgs {
    name: String,
    args: Vec<Value>,
}

impl OpArgs {
    fn parse(ruby: &Ruby, op: Value) -> Result<Self, Error> {
        use magnus::TryConvert;
        let ary = <magnus::RArray as TryConvert>::try_convert(op)?;
        let mut parts = Vec::with_capacity(ary.len());
        for item in ary.each() {
            parts.push(item?);
        }
        if parts.is_empty() {
            return Err(Error::new(ruby.exception_arg_error(), "empty page op"));
        }
        let name = <String as TryConvert>::try_convert(parts.remove(0))?;
        Ok(Self { name, args: parts })
    }

    fn get<T: magnus::TryConvert>(&self, ruby: &Ruby, i: usize) -> Result<T, Error> {
        let v = self.args.get(i).copied().ok_or_else(|| {
            Error::new(
                ruby.exception_arg_error(),
                format!("page op '{}' missing argument {}", self.name, i),
            )
        })?;
        T::try_convert(v)
    }

    fn bytes(&self, ruby: &Ruby, i: usize) -> Result<Vec<u8>, Error> {
        let s: RString = self.get(ruby, i)?;
        Ok(unsafe { s.as_slice() }.to_vec())
    }
}

impl RbDocumentBuilder {
    /// Internal: commit a buffered page. `page_spec` is `["a4"]`,
    /// `["letter"]` or `["custom", w, h]`; `ops` is the Ruby-side op buffer.
    /// Called by `PdfDioxide::FluentPageBuilder#done` — not public API.
    fn apply_page(
        ruby: &Ruby,
        rb_self: Value,
        page_spec: magnus::RArray,
        ops: magnus::RArray,
    ) -> Result<Value, Error> {
        use magnus::TryConvert;
        use pdf_oxide::writer::{LineStyle, PageSize};

        let this = <&RbDocumentBuilder as TryConvert>::try_convert(rb_self)?;
        let mut slot = this.0.borrow_mut();
        let inner = slot.as_mut().ok_or_else(|| {
            Error::new(ruby.get_inner(&ERROR), "DocumentBuilder already consumed")
        })?;

        let mut spec: Vec<Value> = Vec::with_capacity(page_spec.len());
        for item in page_spec.each() {
            spec.push(item?);
        }
        let kind = <String as TryConvert>::try_convert(spec[0])?;
        let page_size = match kind.as_str() {
            "a4" => PageSize::A4,
            "letter" => PageSize::Letter,
            "custom" => PageSize::Custom(
                <f32 as TryConvert>::try_convert(spec[1])?,
                <f32 as TryConvert>::try_convert(spec[2])?,
            ),
            other => {
                return Err(Error::new(
                    ruby.exception_arg_error(),
                    format!("unknown page spec '{other}'"),
                ))
            },
        };

        let mut page = inner.page(page_size);
        for op in ops.each() {
            let op = OpArgs::parse(ruby, op?)?;
            let o = &op;
            page = match op.name.as_str() {
                "font" => page.font(&o.get::<String>(ruby, 0)?, o.get(ruby, 1)?),
                "at" => page.at(o.get(ruby, 0)?, o.get(ruby, 1)?),
                "text" => page.text(&o.get::<String>(ruby, 0)?),
                "heading" => page.heading(o.get(ruby, 0)?, &o.get::<String>(ruby, 1)?),
                "paragraph" => page.paragraph(&o.get::<String>(ruby, 0)?),
                "space" => page.space(o.get(ruby, 0)?),
                "horizontal_rule" => page.horizontal_rule(),
                "link_url" => page.link_url(&o.get::<String>(ruby, 0)?),
                "link_page" => page.link_page(o.get(ruby, 0)?),
                "link_named" => page.link_named(&o.get::<String>(ruby, 0)?),
                "link_javascript" => page.link_javascript(&o.get::<String>(ruby, 0)?),
                "on_open" => page.on_open(&o.get::<String>(ruby, 0)?),
                "on_close" => page.on_close(&o.get::<String>(ruby, 0)?),
                "field_keystroke" => page.field_keystroke(&o.get::<String>(ruby, 0)?),
                "field_format" => page.field_format(&o.get::<String>(ruby, 0)?),
                "field_validate" => page.field_validate(&o.get::<String>(ruby, 0)?),
                "field_calculate" => page.field_calculate(&o.get::<String>(ruby, 0)?),
                "highlight" => {
                    page.highlight((o.get(ruby, 0)?, o.get(ruby, 1)?, o.get(ruby, 2)?))
                },
                "underline" => {
                    page.underline((o.get(ruby, 0)?, o.get(ruby, 1)?, o.get(ruby, 2)?))
                },
                "strikeout" => {
                    page.strikeout((o.get(ruby, 0)?, o.get(ruby, 1)?, o.get(ruby, 2)?))
                },
                "squiggly" => {
                    page.squiggly((o.get(ruby, 0)?, o.get(ruby, 1)?, o.get(ruby, 2)?))
                },
                "sticky_note" => page.sticky_note(&o.get::<String>(ruby, 0)?),
                "sticky_note_at" => page.sticky_note_at(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    &o.get::<String>(ruby, 2)?,
                ),
                "watermark" => page.watermark(&o.get::<String>(ruby, 0)?),
                "watermark_confidential" => page.watermark_confidential(),
                "watermark_draft" => page.watermark_draft(),
                "stamp" => page.stamp(parse_stamp_type(&o.get::<String>(ruby, 0)?)),
                "freetext" => page.freetext(
                    pdf_oxide::geometry::Rect::new(
                        o.get(ruby, 0)?,
                        o.get(ruby, 1)?,
                        o.get(ruby, 2)?,
                        o.get(ruby, 3)?,
                    ),
                    &o.get::<String>(ruby, 4)?,
                ),
                "text_field" => page.text_field(
                    o.get::<String>(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    o.get(ruby, 4)?,
                    o.get::<Option<String>>(ruby, 5)?,
                ),
                "checkbox" => page.checkbox(
                    o.get::<String>(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    o.get(ruby, 4)?,
                    o.get(ruby, 5)?,
                ),
                "combo_box" => page.combo_box(
                    o.get::<String>(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    o.get(ruby, 4)?,
                    o.get::<Vec<String>>(ruby, 5)?,
                    o.get::<Option<String>>(ruby, 6)?,
                ),
                "radio_group" => page.radio_group(
                    o.get::<String>(ruby, 0)?,
                    o.get::<Vec<(String, f32, f32, f32, f32)>>(ruby, 1)?,
                    o.get::<Option<String>>(ruby, 2)?,
                ),
                "push_button" => page.push_button(
                    o.get::<String>(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    o.get(ruby, 4)?,
                    o.get::<String>(ruby, 5)?,
                ),
                "signature_field" => page.signature_field(
                    o.get::<String>(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    o.get(ruby, 4)?,
                ),
                "footnote" => {
                    page.footnote(&o.get::<String>(ruby, 0)?, &o.get::<String>(ruby, 1)?)
                },
                "columns" => page.columns(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    &o.get::<String>(ruby, 2)?,
                ),
                "inline" => page.inline(&o.get::<String>(ruby, 0)?),
                "inline_bold" => page.inline_bold(&o.get::<String>(ruby, 0)?),
                "inline_italic" => page.inline_italic(&o.get::<String>(ruby, 0)?),
                "inline_color" => page.inline_color(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    &o.get::<String>(ruby, 3)?,
                ),
                "newline" => page.newline(),
                "rect" => page.rect(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                ),
                "filled_rect" => page.filled_rect(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    o.get(ruby, 4)?,
                    o.get(ruby, 5)?,
                    o.get(ruby, 6)?,
                ),
                "line" => page.line(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                ),
                "stroke_rect" => page.stroke_rect(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    LineStyle::new(
                        o.get(ruby, 4)?,
                        o.get(ruby, 5)?,
                        o.get(ruby, 6)?,
                        o.get(ruby, 7)?,
                    ),
                ),
                "stroke_rect_dashed" => {
                    let style = LineStyle::new(
                        o.get(ruby, 4)?,
                        o.get(ruby, 5)?,
                        o.get(ruby, 6)?,
                        o.get(ruby, 7)?,
                    )
                    .with_dash(&o.get::<Vec<f32>>(ruby, 8)?, o.get(ruby, 9)?);
                    page.stroke_rect(
                        o.get(ruby, 0)?,
                        o.get(ruby, 1)?,
                        o.get(ruby, 2)?,
                        o.get(ruby, 3)?,
                        style,
                    )
                },
                "stroke_line" => page.stroke_line(
                    o.get(ruby, 0)?,
                    o.get(ruby, 1)?,
                    o.get(ruby, 2)?,
                    o.get(ruby, 3)?,
                    LineStyle::new(
                        o.get(ruby, 4)?,
                        o.get(ruby, 5)?,
                        o.get(ruby, 6)?,
                        o.get(ruby, 7)?,
                    ),
                ),
                "stroke_line_dashed" => {
                    let style = LineStyle::new(
                        o.get(ruby, 4)?,
                        o.get(ruby, 5)?,
                        o.get(ruby, 6)?,
                        o.get(ruby, 7)?,
                    )
                    .with_dash(&o.get::<Vec<f32>>(ruby, 8)?, o.get(ruby, 9)?);
                    page.stroke_line(
                        o.get(ruby, 0)?,
                        o.get(ruby, 1)?,
                        o.get(ruby, 2)?,
                        o.get(ruby, 3)?,
                        style,
                    )
                },
                "text_in_rect" => page.text_in_rect(
                    pdf_oxide::geometry::Rect::new(
                        o.get(ruby, 0)?,
                        o.get(ruby, 1)?,
                        o.get(ruby, 2)?,
                        o.get(ruby, 3)?,
                    ),
                    &o.get::<String>(ruby, 4)?,
                    text_align(o.get(ruby, 5)?),
                ),
                "new_page_same_size" => page.new_page_same_size(),
                "image" => page
                    .image_from_bytes(
                        &o.bytes(ruby, 0)?,
                        pdf_oxide::geometry::Rect::new(
                            o.get(ruby, 1)?,
                            o.get(ruby, 2)?,
                            o.get(ruby, 3)?,
                            o.get(ruby, 4)?,
                        ),
                    )
                    .map_err(|e| map_pdf_error(ruby, e))?,
                "image_with_alt" => page
                    .image_from_bytes_with_alt(
                        &o.bytes(ruby, 0)?,
                        pdf_oxide::geometry::Rect::new(
                            o.get(ruby, 1)?,
                            o.get(ruby, 2)?,
                            o.get(ruby, 3)?,
                            o.get(ruby, 4)?,
                        ),
                        &o.get::<String>(ruby, 5)?,
                    )
                    .map_err(|e| map_pdf_error(ruby, e))?,
                "image_artifact" => page
                    .image_from_bytes_as_artifact(
                        &o.bytes(ruby, 0)?,
                        pdf_oxide::geometry::Rect::new(
                            o.get(ruby, 1)?,
                            o.get(ruby, 2)?,
                            o.get(ruby, 3)?,
                            o.get(ruby, 4)?,
                        ),
                    )
                    .map_err(|e| map_pdf_error(ruby, e))?,
                "table" => {
                    let widths: Vec<f32> = o.get(ruby, 0)?;
                    let aligns: Vec<i32> = o.get(ruby, 1)?;
                    let rows: Vec<Vec<String>> = o.get(ruby, 2)?;
                    let has_header: bool = o.get(ruby, 3)?;
                    let cells: Vec<Vec<pdf_oxide::writer::TableCell>> = rows
                        .into_iter()
                        .map(|row| {
                            row.into_iter()
                                .map(pdf_oxide::writer::TableCell::text)
                                .collect()
                        })
                        .collect();
                    let mut tbl = pdf_oxide::writer::Table::new(cells);
                    tbl = tbl.with_column_widths(
                        widths
                            .iter()
                            .map(|&w| pdf_oxide::writer::ColumnWidth::Fixed(w))
                            .collect(),
                    );
                    tbl.column_aligns = aligns.iter().map(|&a| cell_align(a)).collect();
                    if has_header {
                        tbl = tbl.with_header_row();
                    }
                    page.table(tbl)
                },
                "streaming_table" => {
                    let headers: Vec<String> = o.get(ruby, 0)?;
                    let widths: Vec<f32> = o.get(ruby, 1)?;
                    let aligns: Vec<i32> = o.get(ruby, 2)?;
                    let repeat_header: bool = o.get(ruby, 3)?;
                    let rows: Vec<Vec<(String, usize)>> = o.get(ruby, 4)?;
                    let mode: String = o.get(ruby, 5)?;
                    let sample_rows: usize = o.get(ruby, 6)?;
                    let min_w: f32 = o.get(ruby, 7)?;
                    let max_w: f32 = o.get(ruby, 8)?;
                    let max_rowspan: usize = o.get(ruby, 9)?;

                    let mut cfg = pdf_oxide::writer::StreamingTableConfig::new()
                        .repeat_header(repeat_header)
                        .max_rowspan(max_rowspan);
                    cfg = match mode.as_str() {
                        "sample" => cfg.mode_sample(sample_rows, min_w, max_w),
                        "auto_all" => cfg.mode_auto_all(),
                        _ => cfg.mode_fixed(),
                    };
                    for i in 0..headers.len() {
                        let col = pdf_oxide::writer::StreamingColumn::new(headers[i].clone())
                            .width_pt(widths[i])
                            .align(cell_align(aligns[i]));
                        cfg = cfg.column(col);
                    }
                    let mut st = page.streaming_table(cfg);
                    for row in rows {
                        let _ = st.push_row(|r| {
                            for (text, span) in row {
                                if span > 1 {
                                    r.span_cell(text, span);
                                } else {
                                    r.cell(text);
                                }
                            }
                        });
                    }
                    st.finish()
                },
                other => {
                    return Err(Error::new(
                        ruby.exception_arg_error(),
                        format!("unknown page op '{other}'"),
                    ))
                },
            };
        }
        page.done();
        drop(slot);
        Ok(rb_self)
    }
}

/// Record-time 1-D barcode render for `FluentPageBuilder#barcode_1d`
/// (errors surface at the Ruby call site, not during `done`).
fn render_barcode_1d(
    ruby: &Ruby,
    barcode_type: i32,
    data: String,
    w: f32,
    h: f32,
) -> Result<RString, Error> {
    use pdf_oxide::writer::{BarcodeGenerator, BarcodeOptions, BarcodeType};
    let bt = match barcode_type {
        0 => BarcodeType::Code128,
        1 => BarcodeType::Code39,
        2 => BarcodeType::Ean13,
        3 => BarcodeType::Ean8,
        4 => BarcodeType::UpcA,
        5 => BarcodeType::Itf,
        6 => BarcodeType::Code93,
        7 => BarcodeType::Codabar,
        _ => {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("unknown barcode_type {barcode_type}; valid values are 0-7"),
            ))
        },
    };
    let opts = BarcodeOptions::new().width(w as u32).height(h as u32);
    BarcodeGenerator::generate_1d(bt, &data, &opts)
        .map(|b| ruby.str_from_slice(&b))
        .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
}

/// Record-time QR render for `FluentPageBuilder#barcode_qr`.
fn render_barcode_qr(ruby: &Ruby, data: String, size: f32) -> Result<RString, Error> {
    use pdf_oxide::writer::{BarcodeGenerator, QrCodeOptions};
    let opts = QrCodeOptions::new().size(size as u32);
    BarcodeGenerator::generate_qr(&data, &opts)
        .map(|b| ruby.str_from_slice(&b))
        .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
}

/// Base-14 text-width measurement for `FluentPageBuilder#measure`.
fn measure_text(text: String, font: String, size: f32) -> f32 {
    pdf_oxide::writer::FontManager::new().text_width(&text, &font, size)
}

// ---------------------------------------------------------------------------
// Editor DOM classes (PdfPage / PdfTextId / PdfText / PdfImage /
// PdfAnnotation / PdfElement)
// ---------------------------------------------------------------------------

/// `PdfDioxide::PdfTextId` — opaque handle to a text element on a `PdfPage`.
#[magnus::wrap(class = "PdfDioxide::PdfTextId", free_immediately, size)]
#[derive(Clone)]
struct RbPdfTextId(pdf_oxide::editor::ElementId);

impl RbPdfTextId {
    fn inspect(&self) -> String {
        format!("#<PdfDioxide::PdfTextId {:?}>", self.0)
    }
}

/// `PdfDioxide::PdfText` — a text element in the editor DOM.
#[magnus::wrap(class = "PdfDioxide::PdfText", free_immediately, size)]
struct RbPdfText(pdf_oxide::editor::PdfText);

impl RbPdfText {
    fn id(&self) -> RbPdfTextId {
        RbPdfTextId(self.0.id())
    }
    fn value(&self) -> String {
        self.0.text().to_string()
    }
    fn bbox(&self) -> (f32, f32, f32, f32) {
        let r = self.0.bbox();
        (r.x, r.y, r.width, r.height)
    }
    fn font_name(&self) -> String {
        self.0.font_name().to_string()
    }
    fn font_size(&self) -> f32 {
        self.0.font_size()
    }
    fn is_bold(&self) -> bool {
        self.0.is_bold()
    }
    fn is_italic(&self) -> bool {
        self.0.is_italic()
    }
    fn contains(&self, n: String) -> bool {
        self.0.contains(&n)
    }
    fn starts_with(&self, p: String) -> bool {
        self.0.starts_with(&p)
    }
    fn ends_with(&self, s: String) -> bool {
        self.0.ends_with(&s)
    }
    fn inspect(&self) -> String {
        format!("#<PdfDioxide::PdfText {:?}>", self.0.text())
    }
}

/// `PdfDioxide::PdfImage` — an image element in the editor DOM.
#[magnus::wrap(class = "PdfDioxide::PdfImage", free_immediately, size)]
struct RbPdfImage(pdf_oxide::editor::PdfImage);

impl RbPdfImage {
    fn bbox(&self) -> (f32, f32, f32, f32) {
        let r = self.0.bbox();
        (r.x, r.y, r.width, r.height)
    }
    fn width(&self) -> u32 {
        self.0.dimensions().0
    }
    fn height(&self) -> u32 {
        self.0.dimensions().1
    }
    fn aspect_ratio(&self) -> f32 {
        self.0.aspect_ratio()
    }
    fn inspect(&self) -> String {
        let (w, h) = self.0.dimensions();
        format!("#<PdfDioxide::PdfImage {w}x{h}>")
    }
}

/// `PdfDioxide::PdfAnnotation` — an annotation on a `PdfPage`.
#[magnus::wrap(class = "PdfDioxide::PdfAnnotation", free_immediately, size)]
struct RbPdfAnnotation(pdf_oxide::editor::AnnotationWrapper);

impl RbPdfAnnotation {
    fn subtype(&self) -> String {
        format!("{:?}", self.0.subtype())
    }
    fn rect(&self) -> (f32, f32, f32, f32) {
        let r = self.0.rect();
        (r.x, r.y, r.width, r.height)
    }
    fn contents(&self) -> Option<String> {
        self.0.contents().map(|s| s.to_string())
    }
    fn color(&self) -> Option<(f32, f32, f32)> {
        self.0.color()
    }
    fn is_modified(&self) -> bool {
        self.0.is_modified()
    }
    fn is_new(&self) -> bool {
        self.0.is_new()
    }
    fn inspect(&self) -> String {
        format!("#<PdfDioxide::PdfAnnotation subtype={:?}>", self.0.subtype())
    }
}

/// `PdfDioxide::PdfElement` — a typed element in the editor DOM.
#[magnus::wrap(class = "PdfDioxide::PdfElement", free_immediately, size)]
struct RbPdfElement(pdf_oxide::editor::PdfElement);

impl RbPdfElement {
    fn is_text(&self) -> bool {
        self.0.is_text()
    }
    fn is_image(&self) -> bool {
        self.0.is_image()
    }
    fn is_path(&self) -> bool {
        self.0.is_path()
    }
    fn is_table(&self) -> bool {
        self.0.is_table()
    }
    fn is_structure(&self) -> bool {
        self.0.is_structure()
    }
    fn as_text(&self) -> Option<RbPdfText> {
        if let pdf_oxide::editor::PdfElement::Text(t) = &self.0 {
            Some(RbPdfText(t.clone()))
        } else {
            None
        }
    }
    fn as_image(&self) -> Option<RbPdfImage> {
        if let pdf_oxide::editor::PdfElement::Image(i) = &self.0 {
            Some(RbPdfImage(i.clone()))
        } else {
            None
        }
    }
    fn bbox(&self) -> (f32, f32, f32, f32) {
        let r = self.0.bbox();
        (r.x, r.y, r.width, r.height)
    }
    fn inspect(&self) -> String {
        "#<PdfDioxide::PdfElement>".to_string()
    }
}

/// `PdfDioxide::PdfPage` — an editable page snapshot from `doc.page(i)`; write
/// modifications back with `doc.save_page(page)`.
#[magnus::wrap(class = "PdfDioxide::PdfPage", free_immediately, size)]
struct RbPdfPage(RefCell<pdf_oxide::editor::PdfPage>);

impl RbPdfPage {
    fn index(&self) -> usize {
        self.0.borrow().page_index
    }
    fn width(&self) -> f32 {
        self.0.borrow().width
    }
    fn height(&self) -> f32 {
        self.0.borrow().height
    }
    fn children(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for e in rb_self.0.borrow().children() {
            out.push(RbPdfElement(e))?;
        }
        Ok(out)
    }
    fn find_text_containing(
        ruby: &Ruby,
        rb_self: &Self,
        needle: String,
    ) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for t in rb_self.0.borrow().find_text_containing(&needle) {
            out.push(RbPdfText(t))?;
        }
        Ok(out)
    }
    fn find_images(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for i in rb_self.0.borrow().find_images() {
            out.push(RbPdfImage(i))?;
        }
        Ok(out)
    }
    fn set_text(
        ruby: &Ruby,
        rb_self: &Self,
        text_id: &RbPdfTextId,
        new_text: String,
    ) -> Result<(), Error> {
        rb_self
            .0
            .borrow_mut()
            .set_text(text_id.0, &new_text)
            .map_err(|e| map_pdf_error(ruby, e))
    }
    fn annotations(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for a in rb_self.0.borrow().annotations().iter() {
            out.push(RbPdfAnnotation(a.clone()))?;
        }
        Ok(out)
    }
    fn add_link(
        &self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        url: String,
    ) -> String {
        use pdf_oxide::writer::LinkAnnotation;
        let l = LinkAnnotation::uri(pdf_oxide::geometry::Rect::new(x, y, width, height), &url);
        format!("{:?}", self.0.borrow_mut().add_annotation(l))
    }
    fn add_highlight(
        &self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: (f32, f32, f32),
    ) -> String {
        use pdf_oxide::writer::TextMarkupAnnotation;
        use pdf_oxide::TextMarkupType;
        let l = TextMarkupAnnotation::from_rect(
            TextMarkupType::Highlight,
            pdf_oxide::geometry::Rect::new(x, y, width, height),
        )
        .with_color(color.0, color.1, color.2);
        format!("{:?}", self.0.borrow_mut().add_annotation(l))
    }
    fn add_note(&self, x: f32, y: f32, text: String) -> String {
        use pdf_oxide::writer::TextAnnotation;
        let l = TextAnnotation::new(pdf_oxide::geometry::Rect::new(x, y, 24.0, 24.0), &text);
        format!("{:?}", self.0.borrow_mut().add_annotation(l))
    }
    fn remove_annotation(&self, index: usize) -> bool {
        self.0.borrow_mut().remove_annotation(index).is_some()
    }
    /// `page.add_text(text, x, y, font_size = 12.0) #=> PdfTextId`
    fn add_text(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RbPdfTextId, Error> {
        use pdf_oxide::elements::{FontSpec, TextContent, TextStyle};
        let _ = ruby;
        let args = scan_args::<(String, f32, f32), (Option<f32>,), (), (), (), ()>(args)?;
        let (text, x, y) = args.required;
        let font_size = args.optional.0.unwrap_or(12.0);

        let c = TextContent {
            text: text.clone(),
            bbox: pdf_oxide::geometry::Rect::new(
                x,
                y,
                text.len() as f32 * font_size * 0.6,
                font_size,
            ),
            font: FontSpec {
                name: "Helvetica".to_string(),
                size: font_size,
            },
            style: TextStyle::default(),
            reading_order: None,
            artifact_type: None,
            origin: None,
            rotation_degrees: None,
            matrix: None,
        };
        Ok(RbPdfTextId(rb_self.0.borrow_mut().add_text(c)))
    }
    fn remove_element(&self, id: &RbPdfTextId) -> bool {
        self.0.borrow_mut().remove_element(id.0)
    }
    fn inspect(&self) -> String {
        let p = self.0.borrow();
        format!(
            "#<PdfDioxide::PdfPage index={} width={:.1} height={:.1}>",
            p.page_index, p.width, p.height
        )
    }
}

// ---------------------------------------------------------------------------
// Text object classes (TextChar / TextSpan / TextWord / TextLine)
// ---------------------------------------------------------------------------

fn bbox_tuple(bbox: &pdf_oxide::geometry::Rect) -> (f32, f32, f32, f32) {
    (bbox.x, bbox.y, bbox.width, bbox.height)
}

/// `PdfDioxide::TextChar` — a single positioned character.
#[magnus::wrap(class = "PdfDioxide::TextChar", free_immediately, size)]
struct RbTextChar(TextChar);

impl RbTextChar {
    fn char(&self) -> char {
        self.0.char
    }
    fn bbox(&self) -> (f32, f32, f32, f32) {
        bbox_tuple(&self.0.bbox)
    }
    fn font_name(&self) -> String {
        self.0.font_name.clone()
    }
    fn font_size(&self) -> f32 {
        self.0.font_size
    }
    fn font_weight(&self) -> String {
        format!("{:?}", self.0.font_weight)
    }
    fn is_italic(&self) -> bool {
        self.0.is_italic
    }
    fn is_monospace(&self) -> bool {
        self.0.is_monospace
    }
    fn color(&self) -> (f32, f32, f32) {
        (self.0.color.r, self.0.color.g, self.0.color.b)
    }
    fn rotation_degrees(&self) -> f32 {
        self.0.rotation_degrees
    }
    fn origin_x(&self) -> f32 {
        self.0.origin_x
    }
    fn origin_y(&self) -> f32 {
        self.0.origin_y
    }
    fn advance_width(&self) -> f32 {
        self.0.advance_width
    }
    fn mcid(&self) -> Option<u32> {
        self.0.mcid
    }
}

/// `PdfDioxide::TextSpan` — a run of text drawn by one Tj/TJ operator.
#[magnus::wrap(class = "PdfDioxide::TextSpan", free_immediately, size)]
struct RbTextSpan(TextSpan);

impl RbTextSpan {
    fn text(&self) -> String {
        self.0.text.clone()
    }
    fn bbox(&self) -> (f32, f32, f32, f32) {
        bbox_tuple(&self.0.bbox)
    }
    /// `bbox` with any text-matrix rotation resolved into an axis-aligned
    /// page-space hull; identical to `bbox` for upright runs.
    fn page_bbox(&self) -> (f32, f32, f32, f32) {
        bbox_tuple(&self.0.page_bbox())
    }
    fn font_name(&self) -> String {
        self.0.font_name.clone()
    }
    fn font_size(&self) -> f32 {
        self.0.font_size
    }
    fn is_bold(&self) -> bool {
        self.0.font_weight as u16 >= 700
    }
    fn is_italic(&self) -> bool {
        self.0.is_italic
    }
    fn is_monospace(&self) -> bool {
        self.0.is_monospace
    }
    fn char_widths(&self) -> Vec<f32> {
        self.0.char_widths.clone()
    }
    fn color(&self) -> (f32, f32, f32) {
        (self.0.color.r, self.0.color.g, self.0.color.b)
    }
    /// Content-stream emission order — adjacent values mean the spans were
    /// drawn consecutively, not merely placed near each other.
    fn sequence(&self) -> usize {
        self.0.sequence
    }
    /// Which ISO 32000-1 §9.10.2 mapping tier produced the text
    /// ("to_unicode", "encoding", ... or "fallback"); nil when the font
    /// could not be resolved.
    fn provenance(&self) -> Option<&'static str> {
        self.0.provenance.map(|p| p.as_str())
    }
}

/// `PdfDioxide::TextWord` — a whitespace-delimited word.
#[magnus::wrap(class = "PdfDioxide::TextWord", free_immediately, size)]
struct RbTextWord(Word);

impl RbTextWord {
    fn text(&self) -> String {
        self.0.text.clone()
    }
    fn bbox(&self) -> (f32, f32, f32, f32) {
        bbox_tuple(&self.0.bbox)
    }
    fn font_name(&self) -> String {
        self.0.dominant_font.clone()
    }
    fn font_size(&self) -> f32 {
        self.0.avg_font_size
    }
    fn is_bold(&self) -> bool {
        self.0.is_bold
    }
    fn is_italic(&self) -> bool {
        self.0.is_italic
    }
    fn chars(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for c in &rb_self.0.chars {
            out.push(RbTextChar(c.clone()))?;
        }
        Ok(out)
    }
    fn sequence(&self) -> usize {
        self.0.sequence
    }
    fn rotation_degrees(&self) -> f32 {
        self.0.rotation_degrees
    }
}

/// `PdfDioxide::TextLine` — a horizontal line of words.
#[magnus::wrap(class = "PdfDioxide::TextLine", free_immediately, size)]
struct RbTextLine(TextLine);

impl RbTextLine {
    fn text(&self) -> String {
        self.0.text.clone()
    }
    fn bbox(&self) -> (f32, f32, f32, f32) {
        bbox_tuple(&self.0.bbox)
    }
    fn words(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for w in &rb_self.0.words {
            out.push(RbTextWord(w.clone()))?;
        }
        Ok(out)
    }
    fn chars(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let out = ruby.ary_new();
        for w in &rb_self.0.words {
            for c in &w.chars {
                out.push(RbTextChar(c.clone()))?;
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// PdfDioxide::PdfDocument
// ---------------------------------------------------------------------------

/// Mirror of PyPdfDocument: the parsed document plus the lazily-created
/// `DocumentEditor` used by the mutation APIs (rotation, boxes, metadata,
/// redaction, save). The editor re-reads the document from `path` /
/// `raw_bytes`, exactly like python.rs's `ensure_editor`.
struct Inner {
    doc: PdfDocument,
    path: Option<String>,
    raw_bytes: Option<Vec<u8>>,
    editor: Option<DocumentEditor>,
}

impl Inner {
    /// Ensure the editor is initialized for DOM access.
    fn ensure_editor(&mut self, ruby: &Ruby) -> Result<&mut DocumentEditor, Error> {
        if self.editor.is_none() {
            let editor = if let Some(ref path) = self.path {
                DocumentEditor::open(path)
            } else if let Some(ref bytes) = self.raw_bytes {
                DocumentEditor::from_bytes(bytes.clone())
            } else {
                return Err(Error::new(
                    ruby.get_inner(&ERROR),
                    "No document source available",
                ));
            };
            self.editor = Some(editor.map_err(|e| map_pdf_error(ruby, e))?);
        }
        Ok(self.editor.as_mut().expect("editor initialized above"))
    }
}

/// `PdfDocument` wrapped as a Ruby object.
///
/// Magnus hands wrapped methods a `&self`, so the `RefCell` is what gives us
/// interior mutability for the pdf_oxide methods that want `&mut self`.
#[magnus::wrap(class = "PdfDioxide::PdfDocument", free_immediately, size)]
struct RbPdfDocument(RefCell<Inner>);

impl RbPdfDocument {
    /// Run `f` against the parsed document. Used by the Ext surface, which
    /// must not reach into `Inner` directly.
    fn with_doc<R>(&self, f: impl FnOnce(&PdfDocument) -> R) -> R {
        f(&self.0.borrow().doc)
    }

    fn from_doc(doc: PdfDocument, path: Option<String>, raw_bytes: Option<Vec<u8>>) -> Self {
        RbPdfDocument(RefCell::new(Inner {
            doc,
            path,
            raw_bytes,
            editor: None,
        }))
    }

    /// Authenticate `doc` with `password`, raising `PdfDioxide::PasswordError`
    /// when the password is wrong (python.rs raises RuntimeError here; the
    /// dedicated class is the Ruby-side improvement).
    fn check_password(ruby: &Ruby, doc: &PdfDocument, password: &str) -> Result<(), Error> {
        let ok = doc
            .authenticate(password.as_bytes())
            .map_err(|e| map_pdf_error(ruby, e))?;
        if ok {
            Ok(())
        } else {
            Err(Error::new(
                ruby.get_inner(&PASSWORD_ERROR),
                "Authentication failed: wrong password",
            ))
        }
    }

    // -- constructors -------------------------------------------------------

    /// `PdfDioxide::PdfDocument.open(path, password = nil)`
    fn open(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(String,), (Option<String>,), (), (), (), ()>(args)?;
        let (path,) = args.required;
        let (password,) = args.optional;

        let doc = PdfDocument::open(&path).map_err(|e| map_pdf_error(ruby, e))?;
        if let Some(pw) = password {
            Self::check_password(ruby, &doc, &pw)?;
        }
        Ok(Self::from_doc(doc, Some(path), None))
    }

    /// `PdfDioxide::PdfDocument.from_bytes(data, password = nil)` — `data` is a
    /// binary Ruby String.
    fn from_bytes(ruby: &Ruby, args: &[Value]) -> Result<Self, Error> {
        let args = scan_args::<(RString,), (Option<String>,), (), (), (), ()>(args)?;
        let (data,) = args.required;
        let (password,) = args.optional;

        // Safety: the slice is copied to an owned Vec before any Ruby code
        // can run again, so the underlying String cannot move or be GC'd
        // while we hold it.
        let bytes = unsafe { data.as_slice() }.to_vec();
        let doc = PdfDocument::from_bytes(bytes.clone()).map_err(|e| map_pdf_error(ruby, e))?;
        if let Some(pw) = password {
            Self::check_password(ruby, &doc, &pw)?;
        }
        Ok(Self::from_doc(doc, None, Some(bytes)))
    }

    // -- basics -------------------------------------------------------------

    /// `doc.version #=> [major, minor]`
    fn version(_ruby: &Ruby, rb_self: &Self) -> Result<(u8, u8), Error> {
        Ok(rb_self.0.borrow().doc.version())
    }

    /// `doc.authenticate(password) #=> true/false`
    fn authenticate(ruby: &Ruby, rb_self: &Self, password: String) -> Result<bool, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .authenticate(password.as_bytes())
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.page_count #=> Integer`
    fn page_count(ruby: &Ruby, rb_self: &Self) -> Result<usize, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .page_count()
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.has_structure_tree #=> true/false` (Tagged PDF)
    fn has_structure_tree(_ruby: &Ruby, rb_self: &Self) -> Result<bool, Error> {
        Ok(rb_self.0.borrow().doc.structure_tree().ok().flatten().is_some())
    }

    /// `doc.get_layers #=> Array<String>`
    fn get_layers(ruby: &Ruby, rb_self: &Self) -> Result<Vec<String>, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .get_layers()
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.get_page_inks(page) #=> Array<String>`
    fn get_page_inks(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<Vec<String>, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .get_page_inks(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.get_page_inks_deep(page) #=> Array<String>`
    fn get_page_inks_deep(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<Vec<String>, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .get_page_inks_deep(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- text extraction ----------------------------------------------------

    /// `doc.extract_text(page, region: nil, exclude_layers: nil,
    /// exclude_inks: nil, extract_tables: true, include_artifacts: true)
    /// #=> String` — page index is 0-based; `region` is `[x, y, w, h]`.
    ///
    /// Note: pdf_oxide deliberately degrades to an empty string (not an
    /// error) when a page cannot be decrypted, matching pdftotext/PyMuPDF.
    /// Use `page_count` to detect an unauthenticated encrypted document —
    /// it raises `PdfDioxide::PasswordError`.
    fn extract_text(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        type Opts = (
            Option<(f32, f32, f32, f32)>, // region
            Option<Vec<String>>,          // exclude_layers
            Option<Vec<String>>,          // exclude_inks
            Option<bool>,                 // extract_tables
            Option<bool>,                 // include_artifacts
        );
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), Opts, ()>(
            args.keywords,
            &[],
            &[
                "region",
                "exclude_layers",
                "exclude_inks",
                "extract_tables",
                "include_artifacts",
            ],
        )?;
        let (region, exclude_layers, exclude_inks, extract_tables, include_artifacts) =
            kw.optional;

        let has_filters = exclude_layers.is_some() || exclude_inks.is_some();
        let layers: HashSet<String> = exclude_layers.unwrap_or_default().into_iter().collect();
        let inks: HashSet<String> = exclude_inks.unwrap_or_default().into_iter().collect();

        let inner = rb_self.0.borrow();
        if let Some((x, y, w, h)) = region {
            let rect = pdf_oxide::geometry::Rect::new(x, y, w, h);
            let mode = pdf_oxide::layout::RectFilterMode::Intersects;
            if has_filters {
                inner
                    .doc
                    .extract_text_filtered_in_rect(page, layers, inks, rect, mode)
                    .map_err(|e| map_pdf_error(ruby, e))
            } else {
                inner
                    .doc
                    .extract_text_in_rect(page, rect, mode)
                    .map_err(|e| map_pdf_error(ruby, e))
            }
        } else if has_filters {
            inner
                .doc
                .extract_text_filtered(page, layers, inks)
                .map_err(|e| map_pdf_error(ruby, e))
        } else {
            let options = ConversionOptions {
                extract_tables: extract_tables.unwrap_or(true),
                include_artifacts: include_artifacts.unwrap_or(true),
                ..Default::default()
            };
            inner
                .doc
                .extract_text_with_options(page, &options)
                .map_err(|e| map_pdf_error(ruby, e))
        }
    }

    /// `doc.extract_text_auto(page) #=> String` — picks the extraction mode
    /// (plain / markdown-stripped) that scores best for the page.
    fn extract_text_auto(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<String, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .extract_text_auto(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.has_text_layer(page) #=> true/false` — false for image-only /
    /// genuinely-empty pages; callers route those to OCR.
    fn has_text_layer(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<bool, Error> {
        rb_self
            .0
            .borrow()
            .doc
            .has_text_layer(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.permissions #=> Hash | nil` — the /P permission flags (advisory,
    /// PDF spec §7.6.3.2); nil for unencrypted PDFs.
    fn permissions(ruby: &Ruby, rb_self: &Self) -> Result<Option<RHash>, Error> {
        match rb_self.0.borrow().doc.permissions() {
            None => Ok(None),
            Some(p) => {
                let h = ruby.hash_new();
                h.aset("print_low_res", p.print_low_res)?;
                h.aset("modify", p.modify)?;
                h.aset("copy", p.copy)?;
                h.aset("annotate", p.annotate)?;
                h.aset("fill_forms", p.fill_forms)?;
                h.aset("accessibility", p.accessibility)?;
                h.aset("assemble", p.assemble)?;
                h.aset("print_high_res", p.print_high_res)?;
                h.aset("raw_p", p.raw_p)?;
                Ok(Some(h))
            },
        }
    }

    /// `doc.structured_warnings #=> Array<Hash>` — accumulated structured
    /// parse/extraction warnings (`category`, `page`, `message`,
    /// `spec_section`).
    fn structured_warnings(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let inner = rb_self.0.borrow();
        let out = ruby.ary_new();
        for w in inner.doc.structured_warnings() {
            let h = ruby.hash_new();
            h.aset("category", w.category.as_str())?;
            h.aset("page", w.page)?;
            h.aset("message", w.message.as_str())?;
            h.aset("spec_section", w.spec_section)?;
            out.push(h)?;
        }
        Ok(out)
    }

    // -- rendering ----------------------------------------------------------

    /// `doc.render_page(page, dpi: 72, format: "png", background: nil,
    /// transparent: false, render_annotations: nil, jpeg_quality: nil,
    /// excluded_layers: nil) #=> String (binary image bytes)`
    fn render_page(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RString, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        // Default 72 DPI mirrors the Python binding's back-compat default
        // (the Rust-level default is 150).
        let options = render_options_from_kwargs(ruby, args.keywords, Some(72))?;

        let inner = rb_self.0.borrow();
        pdf_oxide::rendering::render_page(&inner.doc, page, &options)
            .map(|img| ruby.str_from_slice(&img.data))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.render_page_fit(page, width, height, **opts) #=> String (binary)`
    /// — renders at the largest DPI whose output fits in `width`x`height`.
    fn render_page_fit(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RString, Error> {
        let args = scan_args::<(usize, u32, u32), (), (), (), RHash, ()>(args)?;
        let (page, width, height) = args.required;
        if width == 0 || height == 0 {
            return Err(Error::new(
                ruby.exception_arg_error(),
                "width and height must be > 0",
            ));
        }
        let options = render_options_from_kwargs(ruby, args.keywords, None)?;

        let inner = rb_self.0.borrow();
        pdf_oxide::rendering::render_page_fit(&inner.doc, page, width, height, &options)
            .map(|img| ruby.str_from_slice(&img.data))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.render_pixmap(page, dpi: 150) #=> Hash` with `"data"` (raw RGBA
    /// bytes), `"width"`, `"height"`. The Python binding wraps this in a
    /// `RenderedPixmap` helper class defined in Python; Ruby returns the
    /// plain Hash.
    fn render_pixmap(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<u32>,), ()>(args.keywords, &[], &["dpi"])?;
        let (dpi,) = kw.optional;

        let options = RenderOptions::with_dpi(dpi.unwrap_or(150)).as_raw();
        let inner = rb_self.0.borrow();
        let img = pdf_oxide::rendering::render_page(&inner.doc, page, &options)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let h = ruby.hash_new();
        h.aset("data", ruby.str_from_slice(&img.data))?;
        h.aset("width", img.width)?;
        h.aset("height", img.height)?;
        Ok(h)
    }

    fn separation_plate_to_hash(
        ruby: &Ruby,
        plate: &pdf_oxide::rendering::SeparationPlate,
    ) -> Result<RHash, Error> {
        let h = ruby.hash_new();
        h.aset("ink_name", plate.ink_name.as_str())?;
        h.aset("data", ruby.str_from_slice(&plate.data))?;
        h.aset("width", plate.width)?;
        h.aset("height", plate.height)?;
        Ok(h)
    }

    /// `doc.render_separations(page, dpi: 150) #=> Array<Hash>` — one
    /// grayscale plate per ink (`"ink_name"`, `"data"`, `"width"`,
    /// `"height"`; 0 = no ink, 255 = full tint).
    fn render_separations(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<u32>,), ()>(args.keywords, &[], &["dpi"])?;
        let (dpi,) = kw.optional;

        let inner = rb_self.0.borrow();
        let plates =
            pdf_oxide::rendering::render_separations(&inner.doc, page, dpi.unwrap_or(150))
                .map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for p in &plates {
            out.push(Self::separation_plate_to_hash(ruby, p)?)?;
        }
        Ok(out)
    }

    /// `doc.render_separation(page, ink_name, dpi: 150) #=> Hash` — a single
    /// ink plate (all zeros if the ink is absent from the page).
    fn render_separation(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        let args = scan_args::<(usize, String), (), (), (), RHash, ()>(args)?;
        let (page, ink_name) = args.required;
        let kw = get_kwargs::<_, (), (Option<u32>,), ()>(args.keywords, &[], &["dpi"])?;
        let (dpi,) = kw.optional;

        let inner = rb_self.0.borrow();
        let plate = pdf_oxide::rendering::render_separation(
            &inner.doc,
            page,
            &ink_name,
            dpi.unwrap_or(150),
        )
        .map_err(|e| map_pdf_error(ruby, e))?;
        Self::separation_plate_to_hash(ruby, &plate)
    }

    // -- structured text extraction ----------------------------------------

    /// `doc.extract_chars(page, region: nil, exclude_layers: nil,
    /// exclude_inks: nil) #=> Array<PdfDioxide::TextChar>`
    fn extract_chars(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        type Opts = (
            Option<(f32, f32, f32, f32)>, // region
            Option<Vec<String>>,          // exclude_layers
            Option<Vec<String>>,          // exclude_inks
        );
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), Opts, ()>(
            args.keywords,
            &[],
            &["region", "exclude_layers", "exclude_inks"],
        )?;
        let (region, exclude_layers, exclude_inks) = kw.optional;

        let has_filters = exclude_layers.is_some() || exclude_inks.is_some();
        let layers: HashSet<String> = exclude_layers.unwrap_or_default().into_iter().collect();
        let inks: HashSet<String> = exclude_inks.unwrap_or_default().into_iter().collect();

        let inner = rb_self.0.borrow();
        let chars = if has_filters {
            let chars = inner
                .doc
                .extract_chars_filtered(page, layers, inks)
                .map_err(|e| map_pdf_error(ruby, e))?;
            if let Some((x, y, w, h)) = region {
                chars.filter_by_rect(
                    &pdf_oxide::geometry::Rect::new(x, y, w, h),
                    RectFilterMode::Intersects,
                )
            } else {
                chars
            }
        } else if let Some((x, y, w, h)) = region {
            inner
                .doc
                .extract_chars_in_rect(
                    page,
                    pdf_oxide::geometry::Rect::new(x, y, w, h),
                    RectFilterMode::Intersects,
                )
                .map_err(|e| map_pdf_error(ruby, e))?
        } else {
            inner
                .doc
                .extract_chars(page)
                .map_err(|e| map_pdf_error(ruby, e))?
        };

        let out = ruby.ary_new();
        for ch in chars {
            out.push(RbTextChar(ch))?;
        }
        Ok(out)
    }

    /// `doc.extract_words(page, include_artifacts: true, region: nil,
    /// word_gap_threshold: nil, profile: nil) #=> Array<PdfDioxide::TextWord>`
    fn extract_words(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        type Opts = (
            Option<bool>,                 // include_artifacts
            Option<(f32, f32, f32, f32)>, // region
            Option<f32>,                  // word_gap_threshold
            Option<Value>,                // profile
        );
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), Opts, ()>(
            args.keywords,
            &[],
            &["include_artifacts", "region", "word_gap_threshold", "profile"],
        )?;
        let (include_artifacts, region, word_gap_threshold, profile) = kw.optional;
        let profile = profile_from_kwarg(profile)?;

        let inner = rb_self.0.borrow();
        let words = if include_artifacts.unwrap_or(true) {
            inner
                .doc
                .extract_words_with_thresholds(page, word_gap_threshold, profile)
                .map_err(|e| map_pdf_error(ruby, e))?
        } else {
            inner
                .doc
                .extract_words_with_thresholds_no_artifacts(page, word_gap_threshold, profile)
                .map_err(|e| map_pdf_error(ruby, e))?
        };

        let filtered = if let Some((x, y, w, h)) = region {
            words.filter_by_rect(
                &pdf_oxide::geometry::Rect::new(x, y, w, h),
                RectFilterMode::Intersects,
            )
        } else {
            words
        };

        let out = ruby.ary_new();
        for w in filtered {
            out.push(RbTextWord(w))?;
        }
        Ok(out)
    }

    /// `doc.extract_text_lines(page, include_artifacts: true, region: nil,
    /// word_gap_threshold: nil, line_gap_threshold: nil, profile: nil)
    /// #=> Array<PdfDioxide::TextLine>`
    fn extract_text_lines(
        ruby: &Ruby,
        rb_self: &Self,
        args: &[Value],
    ) -> Result<magnus::RArray, Error> {
        type Opts = (
            Option<bool>,                 // include_artifacts
            Option<(f32, f32, f32, f32)>, // region
            Option<f32>,                  // word_gap_threshold
            Option<f32>,                  // line_gap_threshold
            Option<Value>,                // profile
        );
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), Opts, ()>(
            args.keywords,
            &[],
            &[
                "include_artifacts",
                "region",
                "word_gap_threshold",
                "line_gap_threshold",
                "profile",
            ],
        )?;
        let (include_artifacts, region, word_gap_threshold, line_gap_threshold, profile) =
            kw.optional;
        let profile = profile_from_kwarg(profile)?;

        let inner = rb_self.0.borrow();
        let lines = if include_artifacts.unwrap_or(true) {
            inner
                .doc
                .extract_text_lines_with_thresholds(
                    page,
                    word_gap_threshold,
                    line_gap_threshold,
                    profile.clone(),
                )
                .map_err(|e| map_pdf_error(ruby, e))?
        } else {
            inner
                .doc
                .extract_text_lines_with_thresholds_no_artifacts(
                    page,
                    word_gap_threshold,
                    line_gap_threshold,
                    profile,
                )
                .map_err(|e| map_pdf_error(ruby, e))?
        };

        let filtered = if let Some((x, y, w, h)) = region {
            lines.filter_by_rect(
                &pdf_oxide::geometry::Rect::new(x, y, w, h),
                RectFilterMode::Intersects,
            )
        } else {
            lines
        };

        let out = ruby.ary_new();
        for l in filtered {
            out.push(RbTextLine(l))?;
        }
        Ok(out)
    }

    /// `doc.extract_spans(page, region: nil, reading_order: nil)
    /// #=> Array<PdfDioxide::TextSpan>` — `reading_order:` is one of
    /// `"top_to_bottom"` (default), `"column_aware"`, `"structure"`.
    fn extract_spans(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        type Opts = (
            Option<(f32, f32, f32, f32)>, // region
            Option<String>,               // reading_order
        );
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), Opts, ()>(args.keywords, &[], &["region", "reading_order"])?;
        let (region, reading_order) = kw.optional;

        let order = match reading_order.as_deref() {
            Some("column_aware") => ReadingOrder::ColumnAware,
            Some("structure") => ReadingOrder::Structure,
            Some("top_to_bottom") | None => ReadingOrder::TopToBottom,
            Some(other) => {
                return Err(Error::new(
                    ruby.exception_arg_error(),
                    format!(
                        "Unknown reading_order '{other}'. Expected 'top_to_bottom', \
                         'column_aware', or 'structure'."
                    ),
                ));
            },
        };

        let inner = rb_self.0.borrow();
        let spans = if let Some((x, y, w, h)) = region {
            inner.doc.extract_spans_in_rect(
                page,
                pdf_oxide::geometry::Rect::new(x, y, w, h),
                RectFilterMode::Intersects,
            )
        } else {
            inner.doc.extract_spans_with_reading_order(page, order)
        }
        .map_err(|e| map_pdf_error(ruby, e))?;

        let out = ruby.ary_new();
        for s in spans {
            out.push(RbTextSpan(s))?;
        }
        Ok(out)
    }

    // -- search -------------------------------------------------------------

    fn search_options_from_kwargs(
        kw: RHash,
        page: Option<usize>,
    ) -> Result<pdf_oxide::search::SearchOptions, Error> {
        type Opts = (Option<bool>, Option<bool>, Option<bool>, Option<usize>);
        let kw = get_kwargs::<_, (), Opts, ()>(
            kw,
            &[],
            &["case_insensitive", "literal", "whole_word", "max_results"],
        )?;
        let (case_insensitive, literal, whole_word, max_results) = kw.optional;
        let mut opts = pdf_oxide::search::SearchOptions::new()
            .with_case_insensitive(case_insensitive.unwrap_or(false))
            .with_literal(literal.unwrap_or(false))
            .with_whole_word(whole_word.unwrap_or(false))
            .with_max_results(max_results.unwrap_or(0));
        if let Some(p) = page {
            opts = opts.with_page_range(p, p);
        }
        Ok(opts)
    }

    /// `doc.search(pattern, case_insensitive: false, literal: false,
    /// whole_word: false, max_results: 0) #=> Array<Hash>`
    fn search(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(String,), (), (), (), RHash, ()>(args)?;
        let (pattern,) = args.required;
        let opts = Self::search_options_from_kwargs(args.keywords, None)?;

        let inner = rb_self.0.borrow();
        let results = pdf_oxide::search::TextSearcher::search(&inner.doc, &pattern, &opts)
            .map_err(|e| map_pdf_error(ruby, e))?;
        search_results_to_ary(ruby, results)
    }

    /// `doc.search_page(page, pattern, **opts) #=> Array<Hash>`
    fn search_page(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(usize, String), (), (), (), RHash, ()>(args)?;
        let (page, pattern) = args.required;
        let opts = Self::search_options_from_kwargs(args.keywords, Some(page))?;

        let inner = rb_self.0.borrow();
        let results = pdf_oxide::search::TextSearcher::search(&inner.doc, &pattern, &opts)
            .map_err(|e| map_pdf_error(ruby, e))?;
        search_results_to_ary(ruby, results)
    }

    /// `doc.prepare_search` — build the search index for every page up front.
    fn prepare_search(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        rb_self
            .0
            .borrow()
            .doc
            .prepare_search()
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.clear_search_index` — drop the cached search index.
    fn clear_search_index(_ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        rb_self.0.borrow().doc.clear_search_index();
        Ok(())
    }

    // -- images / tables / vector content -----------------------------------

    /// `doc.extract_images(page, region: nil) #=> Array<Hash>` — metadata only.
    fn extract_images(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<(f32, f32, f32, f32)>,), ()>(
            args.keywords,
            &[],
            &["region"],
        )?;
        let (region,) = kw.optional;

        let inner = rb_self.0.borrow();
        let images = if let Some(r) = region {
            inner
                .doc
                .extract_images_in_rect(page, pdf_oxide::geometry::Rect::new(r.0, r.1, r.2, r.3))
        } else {
            inner.doc.extract_images(page)
        }
        .map_err(|e| map_pdf_error(ruby, e))?;

        let out = ruby.ary_new();
        for img in &images {
            let h = ruby.hash_new();
            h.aset("width", img.width())?;
            h.aset("height", img.height())?;
            h.aset("color_space", format!("{:?}", img.color_space()))?;
            h.aset("bits_per_component", img.bits_per_component())?;
            h.aset("bbox", img.bbox().map(|b| (b.x, b.y, b.width, b.height)))?;
            h.aset("rotation", img.rotation_degrees())?;
            h.aset("matrix", img.matrix().to_vec())?;
            out.push(h)?;
        }
        Ok(out)
    }

    /// `doc.extract_image_bytes(page) #=> Array<Hash>` — each with PNG
    /// `"data"` plus `"width"`/`"height"`/`"format"`.
    fn extract_image_bytes(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<magnus::RArray, Error> {
        let inner = rb_self.0.borrow();
        let images = inner
            .doc
            .extract_images(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for img in &images {
            let png = img.to_png_bytes().map_err(|e| map_pdf_error(ruby, e))?;
            let h = ruby.hash_new();
            h.aset("width", img.width())?;
            h.aset("height", img.height())?;
            h.aset("format", "png")?;
            h.aset("data", ruby.str_from_slice(&png))?;
            out.push(h)?;
        }
        Ok(out)
    }

    /// `doc.extract_tables(page, region: nil, table_settings: nil)
    /// #=> Array<Hash>` — `table_settings` keys: `horizontal_strategy` /
    /// `vertical_strategy` ("lines"/"text"/"both"), `column_tolerance`,
    /// `row_tolerance`, `min_table_cells`.
    fn extract_tables(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        use pdf_oxide::structure::spatial_table_detector::{TableDetectionConfig, TableStrategy};

        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<(f32, f32, f32, f32)>, Option<RHash>), ()>(
            args.keywords,
            &[],
            &["region", "table_settings"],
        )?;
        let (region, table_settings) = kw.optional;

        let parse_strategy = |s: &str| -> Result<TableStrategy, Error> {
            match s {
                "lines" => Ok(TableStrategy::Lines),
                "text" => Ok(TableStrategy::Text),
                "both" => Ok(TableStrategy::Both),
                _ => Err(Error::new(ruby.exception_arg_error(), "Invalid strategy")),
            }
        };
        let mut config = TableDetectionConfig::default();
        if let Some(settings) = table_settings {
            if let Some(s) = settings.lookup::<_, Option<String>>(magnus::Symbol::new("horizontal_strategy"))? {
                config.horizontal_strategy = parse_strategy(&s)?;
            }
            if let Some(s) = settings.lookup::<_, Option<String>>(magnus::Symbol::new("vertical_strategy"))? {
                config.vertical_strategy = parse_strategy(&s)?;
            }
            if let Some(v) = settings.lookup::<_, Option<f32>>(magnus::Symbol::new("column_tolerance"))? {
                config.column_tolerance = v;
            }
            if let Some(v) = settings.lookup::<_, Option<f32>>(magnus::Symbol::new("row_tolerance"))? {
                config.row_tolerance = v;
            }
            if let Some(v) = settings.lookup::<_, Option<usize>>(magnus::Symbol::new("min_table_cells"))? {
                config.min_table_cells = v;
            }
        }

        let inner = rb_self.0.borrow();
        let tables = if let Some(r) = region {
            inner.doc.extract_tables_in_rect_with_config(
                page,
                pdf_oxide::geometry::Rect::new(r.0, r.1, r.2, r.3),
                config,
            )
        } else {
            inner.doc.extract_tables_with_config(page, config)
        }
        .map_err(|e| map_pdf_error(ruby, e))?;

        let out = ruby.ary_new();
        for t in &tables {
            let h = ruby.hash_new();
            h.aset("col_count", t.col_count)?;
            h.aset("row_count", t.rows.len())?;
            h.aset("bbox", t.bbox.map(|b| (b.x, b.y, b.width, b.height)))?;
            h.aset("has_header", t.has_header)?;
            let rows = ruby.ary_new();
            for r in &t.rows {
                let rh = ruby.hash_new();
                rh.aset("is_header", r.is_header)?;
                let cells = ruby.ary_new();
                for c in &r.cells {
                    let ch = ruby.hash_new();
                    ch.aset("text", c.text.as_str())?;
                    if let Some(b) = c.bbox {
                        ch.aset("bbox", (b.x, b.y, b.width, b.height))?;
                    }
                    cells.push(ch)?;
                }
                rh.aset("cells", cells)?;
                rows.push(rh)?;
            }
            h.aset("rows", rows)?;
            out.push(h)?;
        }
        Ok(out)
    }

    /// `doc.extract_paths(page, region: nil) #=> Array<Hash>` — vector paths
    /// (the feature the official Ruby FFI gem never exposed).
    fn extract_paths(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<(f32, f32, f32, f32)>,), ()>(
            args.keywords,
            &[],
            &["region"],
        )?;
        let (region,) = kw.optional;

        let inner = rb_self.0.borrow();
        let paths = if let Some(r) = region {
            inner
                .doc
                .extract_paths_in_rect(page, pdf_oxide::geometry::Rect::new(r.0, r.1, r.2, r.3))
        } else {
            inner.doc.extract_paths(page)
        }
        .map_err(|e| map_pdf_error(ruby, e))?;

        let out = ruby.ary_new();
        for p in &paths {
            out.push(path_to_hash(ruby, p)?)?;
        }
        Ok(out)
    }

    /// `doc.extract_rects(page, region: nil) #=> Array<Hash>`
    fn extract_rects(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<(f32, f32, f32, f32)>,), ()>(
            args.keywords,
            &[],
            &["region"],
        )?;
        let (region,) = kw.optional;

        let inner = rb_self.0.borrow();
        let paths = if let Some(r) = region {
            inner
                .doc
                .extract_rects_in_rect(page, pdf_oxide::geometry::Rect::new(r.0, r.1, r.2, r.3))
        } else {
            inner.doc.extract_rects(page)
        }
        .map_err(|e| map_pdf_error(ruby, e))?;

        let out = ruby.ary_new();
        for p in &paths {
            out.push(path_to_hash(ruby, p)?)?;
        }
        Ok(out)
    }

    /// `doc.extract_lines(page, region: nil) #=> Array<Hash>`
    fn extract_lines(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<magnus::RArray, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<(f32, f32, f32, f32)>,), ()>(
            args.keywords,
            &[],
            &["region"],
        )?;
        let (region,) = kw.optional;

        let inner = rb_self.0.borrow();
        let paths = if let Some(r) = region {
            inner
                .doc
                .extract_lines_in_rect(page, pdf_oxide::geometry::Rect::new(r.0, r.1, r.2, r.3))
        } else {
            inner.doc.extract_lines(page)
        }
        .map_err(|e| map_pdf_error(ruby, e))?;

        let out = ruby.ary_new();
        for p in &paths {
            out.push(path_to_hash(ruby, p)?)?;
        }
        Ok(out)
    }

    // -- page-level structured output ---------------------------------------

    /// `doc.extract_page_text(page, reading_order: nil) #=> Hash` with
    /// `"spans"` (TextSpan array), `"chars"` (TextChar array),
    /// `"page_width"`, `"page_height"`.
    fn extract_page_text(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let kw = get_kwargs::<_, (), (Option<String>,), ()>(args.keywords, &[], &["reading_order"])?;
        let (reading_order,) = kw.optional;

        let order = match reading_order.as_deref() {
            Some("column_aware") => ReadingOrder::ColumnAware,
            Some("structure") => ReadingOrder::Structure,
            Some("top_to_bottom") | None => ReadingOrder::TopToBottom,
            Some(other) => {
                return Err(Error::new(
                    ruby.exception_arg_error(),
                    format!(
                        "Unknown reading_order '{other}'. Expected 'top_to_bottom', \
                         'column_aware', or 'structure'."
                    ),
                ));
            },
        };

        let inner = rb_self.0.borrow();
        let page_text = inner
            .doc
            .extract_page_text_with_options(page, order)
            .map_err(|e| map_pdf_error(ruby, e))?;

        let spans = ruby.ary_new();
        for s in page_text.spans {
            spans.push(RbTextSpan(s))?;
        }
        let chars = ruby.ary_new();
        for c in page_text.chars {
            chars.push(RbTextChar(c))?;
        }
        let h = ruby.hash_new();
        h.aset("spans", spans)?;
        h.aset("chars", chars)?;
        h.aset("page_width", page_text.page_width)?;
        h.aset("page_height", page_text.page_height)?;
        Ok(h)
    }

    /// `doc.get_outline #=> Array<Hash> | nil` — bookmark tree (`"title"`,
    /// `"page"`, `"children"`).
    fn get_outline(ruby: &Ruby, rb_self: &Self) -> Result<Option<magnus::RArray>, Error> {
        let inner = rb_self.0.borrow();
        let outline = inner.doc.get_outline().map_err(|e| map_pdf_error(ruby, e))?;
        match outline {
            Some(items) => Ok(Some(outline_items_to_ary(ruby, &items)?)),
            None => Ok(None),
        }
    }

    /// `doc.extract_structured(page) #=> String` — JSON `StructuredPage`
    /// envelope (parse with `JSON.parse`).
    fn extract_structured(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<String, Error> {
        let inner = rb_self.0.borrow();
        let structured = inner
            .doc
            .extract_structured(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        serde_json::to_string(&structured)
            .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
    }

    /// `doc.get_annotations(page) #=> Array<Hash>`
    fn get_annotations(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<magnus::RArray, Error> {
        let inner = rb_self.0.borrow();
        let annos = inner
            .doc
            .get_annotations(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for a in &annos {
            let h = ruby.hash_new();
            if let Some(ref s) = a.subtype {
                h.aset("subtype", s.as_str())?;
            }
            if let Some(ref c) = a.contents {
                h.aset("contents", c.as_str())?;
            }
            if let Some(r) = a.rect {
                h.aset("rect", (r[0], r[1], r[2], r[3]))?;
            }
            if let Some(ref au) = a.author {
                h.aset("author", au.as_str())?;
            }
            if let Some(ref d) = a.creation_date {
                h.aset("creation_date", d.as_str())?;
            }
            if let Some(ref d) = a.modification_date {
                h.aset("modification_date", d.as_str())?;
            }
            if let Some(ref s) = a.subject {
                h.aset("subject", s.as_str())?;
            }
            if let Some(ref c) = a.color {
                if c.len() >= 3 {
                    h.aset("color", (c[0], c[1], c[2]))?;
                }
            }
            if let Some(o) = a.opacity {
                h.aset("opacity", o)?;
            }
            if let Some(ref f) = a.field_type {
                h.aset("field_type", format!("{f:?}"))?;
            }
            if let Some(ref n) = a.field_name {
                h.aset("field_name", n.as_str())?;
            }
            if let Some(ref v) = a.field_value {
                h.aset("field_value", v.as_str())?;
            }
            if let Some(pdf_oxide::annotations::LinkAction::Uri(ref u)) = a.action {
                h.aset("action_uri", u.as_str())?;
            }
            out.push(h)?;
        }
        Ok(out)
    }

    /// `doc.classify_page(page) #=> String` — JSON `PageClassification`.
    fn classify_page(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<String, Error> {
        let inner = rb_self.0.borrow();
        let c = inner
            .doc
            .classify_page(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        serde_json::to_string(&c).map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
    }

    /// `doc.classify_document #=> String` — JSON `DocumentClassification`.
    fn classify_document(ruby: &Ruby, rb_self: &Self) -> Result<String, Error> {
        let inner = rb_self.0.borrow();
        let c = inner
            .doc
            .classify_document()
            .map_err(|e| map_pdf_error(ruby, e))?;
        serde_json::to_string(&c).map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
    }

    /// `doc.extract_page_auto(page, options_json = nil) #=> String` — JSON
    /// `PageExtraction` envelope.
    fn extract_page_auto(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        use pdf_oxide::extractors::auto::{AutoExtractOptions, AutoExtractor};

        let args = scan_args::<(usize,), (Option<String>,), (), (), (), ()>(args)?;
        let (page,) = args.required;
        let (options_json,) = args.optional;

        let opts = match options_json.as_deref() {
            Some(s) if !s.trim().is_empty() => serde_json::from_str(s).map_err(|e| {
                Error::new(ruby.exception_arg_error(), format!("invalid options_json: {e}"))
            })?,
            _ => AutoExtractOptions::default(),
        };
        let inner = rb_self.0.borrow();
        let pe = AutoExtractor::with(opts)
            .extract_page(&inner.doc, page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        serde_json::to_string(&pe).map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))
    }

    // -- forms ---------------------------------------------------------------

    /// `doc.get_form_fields #=> Array<PdfDioxide::FormField>`
    fn get_form_fields(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        use pdf_oxide::extractors::forms::FormExtractor;
        let inner = rb_self.0.borrow();
        let fields =
            FormExtractor::extract_fields(&inner.doc).map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for f in fields {
            out.push(RbFormField(f))?;
        }
        Ok(out)
    }

    /// `doc.get_form_field_value(name) #=> String | bool | Array | nil`
    fn get_form_field_value(ruby: &Ruby, rb_self: &Self, name: String) -> Result<Value, Error> {
        use pdf_oxide::editor::form_fields::FormFieldValue;
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let value = editor
            .get_form_field_value(&name)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(match value {
            Some(FormFieldValue::Text(s)) => s.into_value_with(ruby),
            Some(FormFieldValue::Choice(s)) => s.into_value_with(ruby),
            Some(FormFieldValue::Boolean(b)) => b.into_value_with(ruby),
            Some(FormFieldValue::MultiChoice(v)) => v.into_value_with(ruby),
            Some(FormFieldValue::None) | None => ruby.qnil().as_value(),
        })
    }

    /// `doc.set_form_field_value(name, value)` — value may be a String,
    /// true/false, an Array of Strings, or nil.
    fn set_form_field_value(
        ruby: &Ruby,
        rb_self: &Self,
        name: String,
        value: Value,
    ) -> Result<(), Error> {
        use magnus::TryConvert;
        use pdf_oxide::editor::form_fields::FormFieldValue;

        let field_value = if value.is_nil() {
            FormFieldValue::None
        } else if let Ok(b) = <bool as TryConvert>::try_convert(value) {
            FormFieldValue::Boolean(b)
        } else if let Ok(s) = <String as TryConvert>::try_convert(value) {
            FormFieldValue::Text(s)
        } else if let Ok(v) = <Vec<String> as TryConvert>::try_convert(value) {
            FormFieldValue::MultiChoice(v)
        } else {
            return Err(Error::new(ruby.exception_arg_error(), "Invalid value."));
        };

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .set_form_field_value(&name, field_value)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.has_xfa #=> true/false`
    fn has_xfa(ruby: &Ruby, rb_self: &Self) -> Result<bool, Error> {
        use pdf_oxide::xfa::XfaExtractor;
        let mut inner = rb_self.0.borrow_mut();
        XfaExtractor::has_xfa(&mut inner.doc).map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.export_form_data(path, format = "fdf")` — format "fdf" or "xfdf".
    fn export_form_data(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<(), Error> {
        let args = scan_args::<(String,), (Option<String>,), (), (), (), ()>(args)?;
        let (path,) = args.required;
        let format = args.optional.0.unwrap_or_else(|| "fdf".to_string());

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        match format.as_str() {
            "fdf" => editor
                .export_form_data_fdf(&path)
                .map_err(|e| map_pdf_error(ruby, e)),
            "xfdf" => editor
                .export_form_data_xfdf(&path)
                .map_err(|e| map_pdf_error(ruby, e)),
            _ => Err(Error::new(ruby.exception_arg_error(), "Unknown format.")),
        }
    }

    /// `doc.flatten_forms`
    fn flatten_forms(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.flatten_forms().map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.flatten_forms_on_page(page)`
    fn flatten_forms_on_page(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .flatten_forms_on_page(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.flatten_warnings #=> Array<String>` — widgets without /AP that
    /// flattened to blank rectangles in the last save.
    fn flatten_warnings(_ruby: &Ruby, rb_self: &Self) -> Result<Vec<String>, Error> {
        Ok(rb_self
            .0
            .borrow()
            .editor
            .as_ref()
            .map(|e| e.flatten_warnings().to_vec())
            .unwrap_or_default())
    }

    // -- document assembly ---------------------------------------------------

    /// `doc.merge_from(path_or_bytes) #=> Integer` — appends another PDF's
    /// pages; accepts a path String or binary PDF bytes.
    fn merge_from(ruby: &Ruby, rb_self: &Self, source: Value) -> Result<usize, Error> {
        use magnus::TryConvert;

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        if let Ok(s) = <RString as TryConvert>::try_convert(source) {
            // A String that looks like a PDF is bytes; anything else a path.
            let bytes = unsafe { s.as_slice() }.to_vec();
            if bytes.starts_with(b"%PDF-") {
                editor
                    .merge_from_bytes(&bytes)
                    .map_err(|e| map_pdf_error(ruby, e))
            } else {
                let path = String::from_utf8(bytes).map_err(|_| {
                    Error::new(ruby.exception_arg_error(), "Invalid source.")
                })?;
                editor.merge_from(&path).map_err(|e| map_pdf_error(ruby, e))
            }
        } else {
            Err(Error::new(ruby.exception_arg_error(), "Invalid source."))
        }
    }

    /// `doc.embed_file(name, data)` — attach a file (binary String).
    fn embed_file(ruby: &Ruby, rb_self: &Self, name: String, data: RString) -> Result<(), Error> {
        let bytes = unsafe { data.as_slice() }.to_vec();
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .embed_file(&name, bytes)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.page_labels #=> Array<Hash>` (`"start_page"`, `"style"`,
    /// `"prefix"`, `"start_value"`)
    fn page_labels(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        use pdf_oxide::extractors::page_labels::PageLabelExtractor;
        let inner = rb_self.0.borrow();
        let labels = PageLabelExtractor::extract(&inner.doc).map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for l in &labels {
            let h = ruby.hash_new();
            h.aset("start_page", l.start_page)?;
            h.aset("style", format!("{:?}", l.style))?;
            h.aset("prefix", l.prefix.as_deref())?;
            h.aset("start_value", l.start_value)?;
            out.push(h)?;
        }
        Ok(out)
    }

    /// `doc.xmp_metadata #=> Hash | nil`
    fn xmp_metadata(ruby: &Ruby, rb_self: &Self) -> Result<Option<RHash>, Error> {
        use pdf_oxide::extractors::xmp::XmpExtractor;
        let inner = rb_self.0.borrow();
        let meta = XmpExtractor::extract(&inner.doc).map_err(|e| map_pdf_error(ruby, e))?;
        match meta {
            Some(xmp) => {
                let h = ruby.hash_new();
                if let Some(ref t) = xmp.dc_title {
                    h.aset("dc_title", t.as_str())?;
                }
                if !xmp.dc_creator.is_empty() {
                    h.aset("dc_creator", xmp.dc_creator.clone())?;
                }
                if let Some(ref d) = xmp.dc_description {
                    h.aset("dc_description", d.as_str())?;
                }
                if !xmp.dc_subject.is_empty() {
                    h.aset("dc_subject", xmp.dc_subject.clone())?;
                }
                if let Some(ref l) = xmp.dc_language {
                    h.aset("dc_language", l.as_str())?;
                }
                if let Some(ref t) = xmp.xmp_creator_tool {
                    h.aset("xmp_creator_tool", t.as_str())?;
                }
                if let Some(ref d) = xmp.xmp_create_date {
                    h.aset("xmp_create_date", d.as_str())?;
                }
                if let Some(ref d) = xmp.xmp_modify_date {
                    h.aset("xmp_modify_date", d.as_str())?;
                }
                if let Some(ref p) = xmp.pdf_producer {
                    h.aset("pdf_producer", p.as_str())?;
                }
                if let Some(ref k) = xmp.pdf_keywords {
                    h.aset("pdf_keywords", k.as_str())?;
                }
                Ok(Some(h))
            },
            None => Ok(None),
        }
    }

    // -- compliance ----------------------------------------------------------

    /// `doc.validate_pdf_a(level = "1b") #=> Hash` (`"valid"`, `"level"`,
    /// `"errors"`, `"warnings"`)
    fn validate_pdf_a(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        let args = scan_args::<(), (Option<String>,), (), (), (), ()>(args)?;
        let level = args.optional.0.unwrap_or_else(|| "1b".to_string());
        let pdf_level = parse_pdf_a_level(ruby, &level)?;

        let mut inner = rb_self.0.borrow_mut();
        let result = pdf_oxide::compliance::validate_pdf_a(&mut inner.doc, pdf_level)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let h = ruby.hash_new();
        h.aset("valid", result.errors.is_empty())?;
        h.aset("level", level)?;
        h.aset("errors", result.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>())?;
        h.aset("warnings", result.warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>())?;
        Ok(h)
    }

    /// `doc.convert_to_pdf_a(level = "2b") #=> Hash` (`"success"`, `"level"`,
    /// `"actions"`, `"errors"`) — converts in place; `to_bytes` afterwards
    /// reflects the converted document.
    fn convert_to_pdf_a(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        let args = scan_args::<(), (Option<String>,), (), (), (), ()>(args)?;
        let level = args.optional.0.unwrap_or_else(|| "2b".to_string());
        let pdf_level = parse_pdf_a_level(ruby, &level)?;

        let mut inner = rb_self.0.borrow_mut();
        let result = pdf_oxide::compliance::convert_to_pdf_a(&mut inner.doc, pdf_level)
            .map_err(|e| map_pdf_error(ruby, e))?;
        // Sync raw_bytes so to_bytes() sees the converted document; drop any
        // stale editor opened from the original bytes.
        inner.raw_bytes = Some(inner.doc.source_bytes.to_vec());
        inner.path = None;
        inner.editor = None;

        let h = ruby.hash_new();
        h.aset("success", result.success)?;
        h.aset("level", level)?;
        h.aset(
            "actions",
            result.actions.iter().map(|a| a.description.clone()).collect::<Vec<_>>(),
        )?;
        h.aset(
            "errors",
            result.errors.iter().map(|e| e.reason.clone()).collect::<Vec<_>>(),
        )?;
        Ok(h)
    }

    /// `doc.validate_pdf_ua #=> Hash` (`"valid"`, `"errors"`, `"warnings"`)
    fn validate_pdf_ua(ruby: &Ruby, rb_self: &Self) -> Result<RHash, Error> {
        use pdf_oxide::compliance::{validate_pdf_ua, PdfUaLevel};
        let mut inner = rb_self.0.borrow_mut();
        let result = validate_pdf_ua(&mut inner.doc, PdfUaLevel::Ua1)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let h = ruby.hash_new();
        h.aset("valid", result.errors.is_empty())?;
        h.aset("errors", result.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>())?;
        h.aset("warnings", result.warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>())?;
        Ok(h)
    }

    /// `doc.validate_pdf_x(level = "1a_2001") #=> Hash`
    fn validate_pdf_x(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        use pdf_oxide::compliance::{validate_pdf_x, PdfXLevel};

        let args = scan_args::<(), (Option<String>,), (), (), (), ()>(args)?;
        let level = args.optional.0.unwrap_or_else(|| "1a_2001".to_string());
        let pdf_level = match level.as_str() {
            "1a_2001" => PdfXLevel::X1a2001,
            "3_2002" => PdfXLevel::X32002,
            "4" => PdfXLevel::X4,
            _ => {
                return Err(Error::new(
                    ruby.exception_arg_error(),
                    format!("Unknown PDF/X level: '{level}'. Use 1a_2001, 3_2002, 4"),
                ))
            },
        };

        let mut inner = rb_self.0.borrow_mut();
        let result =
            validate_pdf_x(&mut inner.doc, pdf_level).map_err(|e| map_pdf_error(ruby, e))?;
        let h = ruby.hash_new();
        h.aset("valid", result.errors.is_empty())?;
        h.aset("level", level)?;
        h.aset("errors", result.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>())?;
        h.aset("warnings", result.warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>())?;
        Ok(h)
    }

    // -- page operations -----------------------------------------------------

    /// `doc.extract_pages([0, 2, 5], "subset.pdf")` — write selected pages
    /// (0-based) to a new file.
    fn extract_pages(
        ruby: &Ruby,
        rb_self: &Self,
        pages: Vec<usize>,
        output: String,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .extract_pages(&pages, &output)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.extract_pages_to_bytes([0, 1]) #=> String (binary PDF)`
    fn extract_pages_to_bytes(
        ruby: &Ruby,
        rb_self: &Self,
        pages: Vec<usize>,
    ) -> Result<RString, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let bytes = editor
            .extract_pages_to_bytes(&pages)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    /// `doc.extract_page_ranges_to_bytes([[0, 3], [3, 6]]) #=> Array<String>`
    /// — each range is `[start, end)`.
    fn extract_page_ranges_to_bytes(
        ruby: &Ruby,
        rb_self: &Self,
        ranges: Vec<(usize, usize)>,
    ) -> Result<magnus::RArray, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let chunks = editor
            .extract_page_ranges_to_bytes(&ranges)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for b in chunks {
            out.push(ruby.str_from_slice(&b))?;
        }
        Ok(out)
    }

    /// `doc.select_pages([2, 0, 1])` — restrict (and reorder) the document to
    /// the listed pages; a later `save`/`to_bytes` writes only those.
    fn select_pages(ruby: &Ruby, rb_self: &Self, pages: Vec<usize>) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .select_pages(&pages)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.delete_page(index)`
    fn delete_page(ruby: &Ruby, rb_self: &Self, index: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.remove_page(index).map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.move_page(from_index, to_index)`
    fn move_page(ruby: &Ruby, rb_self: &Self, from: usize, to: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.move_page(from, to).map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.flatten_to_images(dpi = 150) #=> String (binary PDF)` — each page
    /// rendered to an image ("burns in" annotations/forms/overlays).
    fn flatten_to_images(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RString, Error> {
        let args = scan_args::<(), (Option<u32>,), (), (), (), ()>(args)?;
        let dpi = args.optional.0.unwrap_or(150);

        let inner = rb_self.0.borrow();
        let bytes = pdf_oxide::rendering::flatten_to_images(&inner.doc, dpi)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    // -- conversions --------------------------------------------------------

    /// `doc.to_plain_text(page, **opts) #=> String`
    fn to_plain_text(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let options = conversion_options(args.keywords)?;
        rb_self
            .0
            .borrow()
            .doc
            .to_plain_text(page, &options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_plain_text_all(**opts) #=> String`
    fn to_plain_text_all(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let options = conversion_options(args.keywords)?;
        rb_self
            .0
            .borrow()
            .doc
            .to_plain_text_all(&options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_markdown(page, **opts) #=> String`
    fn to_markdown(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let options = conversion_options(args.keywords)?;
        rb_self
            .0
            .borrow()
            .doc
            .to_markdown(page, &options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_markdown_all(**opts) #=> String`
    fn to_markdown_all(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let options = conversion_options(args.keywords)?;
        rb_self
            .0
            .borrow()
            .doc
            .to_markdown_all(&options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_html(page, **opts) #=> String`
    fn to_html(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        let args = scan_args::<(usize,), (), (), (), RHash, ()>(args)?;
        let (page,) = args.required;
        let options = conversion_options(args.keywords)?;
        rb_self
            .0
            .borrow()
            .doc
            .to_html(page, &options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_html_all(**opts) #=> String`
    fn to_html_all(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<String, Error> {
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let options = conversion_options(args.keywords)?;
        rb_self
            .0
            .borrow()
            .doc
            .to_html_all(&options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- office export ------------------------------------------------------

    /// `doc.to_docx(path)`
    fn to_docx(ruby: &Ruby, rb_self: &Self, path: String) -> Result<(), Error> {
        rb_self
            .0
            .borrow()
            .doc
            .to_docx(&path)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_docx_bytes #=> String (binary)`
    fn to_docx_bytes(ruby: &Ruby, rb_self: &Self) -> Result<RString, Error> {
        let bytes = rb_self
            .0
            .borrow()
            .doc
            .to_docx_bytes()
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    /// `doc.to_pptx(path)`
    fn to_pptx(ruby: &Ruby, rb_self: &Self, path: String) -> Result<(), Error> {
        rb_self
            .0
            .borrow()
            .doc
            .to_pptx(&path)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_pptx_bytes #=> String (binary)`
    fn to_pptx_bytes(ruby: &Ruby, rb_self: &Self) -> Result<RString, Error> {
        let bytes = rb_self
            .0
            .borrow()
            .doc
            .to_pptx_bytes()
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    /// `doc.to_xlsx(path)`
    fn to_xlsx(ruby: &Ruby, rb_self: &Self, path: String) -> Result<(), Error> {
        rb_self
            .0
            .borrow()
            .doc
            .to_xlsx(&path)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_xlsx_bytes #=> String (binary)`
    fn to_xlsx_bytes(ruby: &Ruby, rb_self: &Self) -> Result<RString, Error> {
        let bytes = rb_self
            .0
            .borrow()
            .doc
            .to_xlsx_bytes()
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    // -- header/footer/artifact removal ------------------------------------

    /// `doc.remove_headers(threshold = 0.8) #=> Integer`
    ///
    /// NOTE: python.rs mirrors the computed erase regions into the editor
    /// afterwards (`sync_editor_erasures`); the region map is `pub(crate)`
    /// upstream so an out-of-crate binding cannot do that yet. Extraction
    /// APIs see the removal; `save` output does not.
    fn remove_headers(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<usize, Error> {
        let args = scan_args::<(), (Option<f32>,), (), (), (), ()>(args)?;
        let threshold = args.optional.0.unwrap_or(0.8);
        rb_self
            .0
            .borrow()
            .doc
            .remove_headers(threshold)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.remove_footers(threshold = 0.8) #=> Integer` (see remove_headers note)
    fn remove_footers(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<usize, Error> {
        let args = scan_args::<(), (Option<f32>,), (), (), (), ()>(args)?;
        let threshold = args.optional.0.unwrap_or(0.8);
        rb_self
            .0
            .borrow()
            .doc
            .remove_footers(threshold)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.remove_artifacts(threshold = 0.8) #=> Integer` (see remove_headers note)
    fn remove_artifacts(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<usize, Error> {
        let args = scan_args::<(), (Option<f32>,), (), (), (), ()>(args)?;
        let threshold = args.optional.0.unwrap_or(0.8);
        rb_self
            .0
            .borrow()
            .doc
            .remove_artifacts(threshold)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.erase_header(page)` (see remove_headers note)
    fn erase_header(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        inner.ensure_editor(ruby)?;
        inner
            .doc
            .erase_header(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// Deprecated: use `erase_header` instead.
    fn edit_header(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        Self::erase_header(ruby, rb_self, page)
    }

    /// `doc.erase_footer(page)` (see remove_headers note)
    fn erase_footer(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        inner.ensure_editor(ruby)?;
        inner
            .doc
            .erase_footer(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// Deprecated: use `erase_footer` instead.
    fn edit_footer(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        Self::erase_footer(ruby, rb_self, page)
    }

    /// `doc.erase_artifacts(page)` (see remove_headers note)
    fn erase_artifacts(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        inner.ensure_editor(ruby)?;
        inner
            .doc
            .erase_artifacts(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- explicit erase regions --------------------------------------------

    /// `doc.erase_region(page, llx, lly, urx, ury)`
    #[allow(clippy::too_many_arguments)]
    fn erase_region(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        llx: f32,
        lly: f32,
        urx: f32,
        ury: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let rect = pdf_oxide::geometry::Rect::new(llx, lly, urx - llx, ury - lly);
        inner
            .doc
            .erase_region(page, rect)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let editor = inner.ensure_editor(ruby)?;
        editor
            .erase_region(page, [llx, lly, urx, ury])
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.erase_regions(page, [[llx, lly, urx, ury], ...])`
    fn erase_regions(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        rects: Vec<(f32, f32, f32, f32)>,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        for (llx, lly, urx, ury) in &rects {
            let rect = pdf_oxide::geometry::Rect::new(*llx, *lly, *urx - *llx, *ury - *lly);
            inner
                .doc
                .erase_region(page, rect)
                .map_err(|e| map_pdf_error(ruby, e))?;
        }
        let editor = inner.ensure_editor(ruby)?;
        let arrays: Vec<[f32; 4]> = rects.iter().map(|r| [r.0, r.1, r.2, r.3]).collect();
        editor
            .erase_regions(page, &arrays)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.clear_erase_regions(page)`
    fn clear_erase_regions(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        inner
            .doc
            .clear_erase_regions(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        if let Some(ref mut editor) = inner.editor {
            editor.clear_erase_regions(page);
        }
        Ok(())
    }

    // -- rotation -----------------------------------------------------------

    /// `doc.page_rotation(page) #=> Integer (degrees)`
    fn page_rotation(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<i32, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .get_page_rotation(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.set_page_rotation(page, degrees)`
    fn set_page_rotation(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        degrees: i32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .set_page_rotation(page, degrees)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.rotate_page(page, degrees)` — relative rotation.
    fn rotate_page(ruby: &Ruby, rb_self: &Self, page: usize, degrees: i32) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .rotate_page_by(page, degrees)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.rotate_all_pages(degrees)`
    fn rotate_all_pages(ruby: &Ruby, rb_self: &Self, degrees: i32) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .rotate_all_pages(degrees)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- page boxes ---------------------------------------------------------

    /// `doc.page_media_box(page) #=> [llx, lly, urx, ury]`
    fn page_media_box(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
    ) -> Result<(f32, f32, f32, f32), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let b = editor
            .get_page_media_box(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok((b[0], b[1], b[2], b[3]))
    }

    /// `doc.set_page_media_box(page, llx, lly, urx, ury)`
    #[allow(clippy::too_many_arguments)]
    fn set_page_media_box(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        llx: f32,
        lly: f32,
        urx: f32,
        ury: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .set_page_media_box(page, [llx, lly, urx, ury])
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.page_crop_box(page) #=> [llx, lly, urx, ury] | nil`
    fn page_crop_box(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
    ) -> Result<Option<(f32, f32, f32, f32)>, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let b = editor
            .get_page_crop_box(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(b.map(|b| (b[0], b[1], b[2], b[3])))
    }

    /// `doc.set_page_crop_box(page, llx, lly, urx, ury)`
    #[allow(clippy::too_many_arguments)]
    fn set_page_crop_box(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        llx: f32,
        lly: f32,
        urx: f32,
        ury: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .set_page_crop_box(page, [llx, lly, urx, ury])
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.crop_margins(left, right, top, bottom)`
    #[allow(clippy::too_many_arguments)]
    fn crop_margins(
        ruby: &Ruby,
        rb_self: &Self,
        left: f32,
        right: f32,
        top: f32,
        bottom: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .crop_margins(left, right, top, bottom)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- annotation flattening ---------------------------------------------

    /// `doc.flatten_page_annotations(page)`
    fn flatten_page_annotations(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .flatten_page_annotations(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.flatten_all_annotations`
    fn flatten_all_annotations(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .flatten_all_annotations()
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.is_page_marked_for_flatten(page) #=> true/false`
    fn is_page_marked_for_flatten(_ruby: &Ruby, rb_self: &Self, page: usize) -> Result<bool, Error> {
        Ok(rb_self
            .0
            .borrow()
            .editor
            .as_ref()
            .is_some_and(|e| e.is_page_marked_for_flatten(page)))
    }

    /// `doc.unmark_page_for_flatten(page)`
    fn unmark_page_for_flatten(_ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        if let Some(ref mut editor) = rb_self.0.borrow_mut().editor {
            editor.unmark_page_for_flatten(page);
        }
        Ok(())
    }

    // -- redaction ----------------------------------------------------------

    /// `doc.add_redaction(page, [x0, y0, x1, y1], fill = nil)` — `fill` is an
    /// optional `[r, g, b]` DeviceRGB overlay colour.
    fn add_redaction(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<(), Error> {
        let args = scan_args::<
            (usize, (f32, f32, f32, f32)),
            (Option<(f32, f32, f32)>,),
            (),
            (),
            (),
            (),
        >(args)?;
        let (page, rect) = args.required;
        let (fill,) = args.optional;

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .add_redaction(
                page,
                [rect.0, rect.1, rect.2, rect.3],
                fill.map(|(r, g, b)| [r, g, b]),
            )
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.redaction_count(page) #=> Integer`
    fn redaction_count(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<usize, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .redaction_count(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.apply_page_redactions(page)`
    fn apply_page_redactions(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .apply_page_redactions(page)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.apply_all_redactions`
    fn apply_all_redactions(ruby: &Ruby, rb_self: &Self) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .apply_all_redactions()
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.is_page_marked_for_redaction(page) #=> true/false`
    fn is_page_marked_for_redaction(
        _ruby: &Ruby,
        rb_self: &Self,
        page: usize,
    ) -> Result<bool, Error> {
        Ok(rb_self
            .0
            .borrow()
            .editor
            .as_ref()
            .is_some_and(|e| e.is_page_marked_for_redaction(page)))
    }

    /// `doc.unmark_page_for_redaction(page)`
    fn unmark_page_for_redaction(_ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        if let Some(ref mut editor) = rb_self.0.borrow_mut().editor {
            editor.unmark_page_for_redaction(page);
        }
        Ok(())
    }

    /// `doc.apply_redactions_destructive(scrub_metadata: true,
    /// remove_javascript: true, remove_embedded_files: true,
    /// fill: [0.0, 0.0, 0.0]) #=> Hash` — true content removal
    /// (ISO 32000-1:2008 §12.5.6.23); returns a report Hash.
    fn apply_redactions_destructive(
        ruby: &Ruby,
        rb_self: &Self,
        args: &[Value],
    ) -> Result<RHash, Error> {
        type Opts = (
            Option<bool>,            // scrub_metadata
            Option<bool>,            // remove_javascript
            Option<bool>,            // remove_embedded_files
            Option<(f32, f32, f32)>, // fill
        );
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let kw = get_kwargs::<_, (), Opts, ()>(
            args.keywords,
            &[],
            &["scrub_metadata", "remove_javascript", "remove_embedded_files", "fill"],
        )?;
        let (scrub_metadata, remove_javascript, remove_embedded_files, fill) = kw.optional;
        let fill = fill.unwrap_or((0.0, 0.0, 0.0));
        let mut opts = RedactionOptions::default();
        opts.scrub_metadata = scrub_metadata.unwrap_or(true);
        opts.remove_javascript = remove_javascript.unwrap_or(true);
        opts.remove_embedded_files = remove_embedded_files.unwrap_or(true);
        opts.default_fill = [fill.0, fill.1, fill.2];

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let report = editor
            .apply_redactions_destructive(opts)
            .map_err(|e| map_pdf_error(ruby, e))?;
        redaction_report_to_hash(ruby, &report)
    }

    /// `doc.sanitize_document(scrub_metadata: true, remove_javascript: true,
    /// remove_embedded_files: true) #=> Hash` — strips /Info, XMP metadata,
    /// document JavaScript and embedded files without geometric redaction.
    fn sanitize_document(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RHash, Error> {
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let kw = get_kwargs::<_, (), (Option<bool>, Option<bool>, Option<bool>), ()>(
            args.keywords,
            &[],
            &["scrub_metadata", "remove_javascript", "remove_embedded_files"],
        )?;
        let (scrub_metadata, remove_javascript, remove_embedded_files) = kw.optional;
        let mut opts = RedactionOptions::default();
        opts.scrub_metadata = scrub_metadata.unwrap_or(true);
        opts.remove_javascript = remove_javascript.unwrap_or(true);
        opts.remove_embedded_files = remove_embedded_files.unwrap_or(true);

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let report = editor
            .sanitize_document(opts)
            .map_err(|e| map_pdf_error(ruby, e))?;
        redaction_report_to_hash(ruby, &report)
    }

    // -- page images --------------------------------------------------------

    /// `doc.page_images(page) #=> Array<Hash>` — image placements on the
    /// page (`"name"`, `"x"`, `"y"`, `"width"`, `"height"`, `"matrix"`).
    fn page_images(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<magnus::RArray, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let images = editor
            .get_page_images(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for img in images {
            let h = ruby.hash_new();
            h.aset("name", img.name.as_str())?;
            h.aset("x", img.bounds[0])?;
            h.aset("y", img.bounds[1])?;
            h.aset("width", img.bounds[2])?;
            h.aset("height", img.bounds[3])?;
            h.aset(
                "matrix",
                (
                    img.matrix[0],
                    img.matrix[1],
                    img.matrix[2],
                    img.matrix[3],
                    img.matrix[4],
                    img.matrix[5],
                ),
            )?;
            out.push(h)?;
        }
        Ok(out)
    }

    /// `doc.reposition_image(page, image_name, x, y)`
    fn reposition_image(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        image_name: String,
        x: f32,
        y: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .reposition_image(page, &image_name, x, y)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.resize_image(page, image_name, width, height)`
    fn resize_image(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        image_name: String,
        width: f32,
        height: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .resize_image(page, &image_name, width, height)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.set_image_bounds(page, image_name, x, y, width, height)`
    #[allow(clippy::too_many_arguments)]
    fn set_image_bounds(
        ruby: &Ruby,
        rb_self: &Self,
        page: usize,
        image_name: String,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .set_image_bounds(page, &image_name, x, y, width, height)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.clear_image_modifications(page)`
    fn clear_image_modifications(_ruby: &Ruby, rb_self: &Self, page: usize) -> Result<(), Error> {
        if let Some(ref mut editor) = rb_self.0.borrow_mut().editor {
            editor.clear_image_modifications(page);
        }
        Ok(())
    }

    /// `doc.has_image_modifications(page) #=> true/false`
    fn has_image_modifications(_ruby: &Ruby, rb_self: &Self, page: usize) -> Result<bool, Error> {
        Ok(rb_self
            .0
            .borrow()
            .editor
            .as_ref()
            .is_some_and(|e| e.has_image_modifications(page)))
    }

    // -- metadata -----------------------------------------------------------

    /// `doc.set_title(title)`
    fn set_title(ruby: &Ruby, rb_self: &Self, title: String) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.set_title(title);
        Ok(())
    }

    /// `doc.set_author(author)`
    fn set_author(ruby: &Ruby, rb_self: &Self, author: String) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.set_author(author);
        Ok(())
    }

    /// `doc.set_subject(subject)`
    fn set_subject(ruby: &Ruby, rb_self: &Self, subject: String) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.set_subject(subject);
        Ok(())
    }

    /// `doc.set_keywords(keywords)`
    fn set_keywords(ruby: &Ruby, rb_self: &Self, keywords: String) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor.set_keywords(keywords);
        Ok(())
    }

    // -- signatures ----------------------------------------------------------

    /// `doc.signatures #=> Array<PdfDioxide::Signature>`
    fn signatures(ruby: &Ruby, rb_self: &Self) -> Result<magnus::RArray, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let list = pdf_oxide::signatures::enumerate_signatures(&mut inner.doc)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let out = ruby.ary_new();
        for info in list {
            out.push(RbSignature(info))?;
        }
        Ok(out)
    }

    /// `doc.signature_count #=> Integer`
    fn signature_count(ruby: &Ruby, rb_self: &Self) -> Result<usize, Error> {
        let mut inner = rb_self.0.borrow_mut();
        pdf_oxide::signatures::count_signatures(&mut inner.doc).map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.dss #=> PdfDioxide::Dss | nil`
    fn dss(ruby: &Ruby, rb_self: &Self) -> Result<Option<RbDss>, Error> {
        let inner = rb_self.0.borrow();
        pdf_oxide::signatures::read_dss(&inner.doc)
            .map(|opt| opt.map(RbDss))
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- layout analysis -----------------------------------------------------

    /// `doc.page_layout_params(page) #=> PdfDioxide::LayoutParams`
    fn page_layout_params(ruby: &Ruby, rb_self: &Self, page: usize) -> Result<RbLayoutParams, Error> {
        use pdf_oxide::layout::{AdaptiveLayoutParams, DocumentProperties};

        let inner = rb_self.0.borrow();
        let spans = inner
            .doc
            .extract_spans(page)
            .map_err(|e| map_pdf_error(ruby, e))?;
        let media_box = inner
            .doc
            .get_page_media_box(page)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let page_bbox =
            pdf_oxide::geometry::Rect::new(media_box.0, media_box.1, media_box.2, media_box.3);

        let all_chars: Vec<_> = spans.iter().flat_map(|s| s.to_chars()).collect();
        let props = DocumentProperties::analyze(&all_chars, page_bbox)
            .map_err(|e| Error::new(ruby.get_inner(&ERROR), e.to_string()))?;
        let params = AdaptiveLayoutParams::from_properties(&props);

        Ok(RbLayoutParams {
            word_gap_threshold: params.word_gap_threshold,
            line_gap_threshold: params.line_gap_threshold,
            median_char_width: props.median_char_width,
            median_font_size: props.median_font_size,
            median_line_spacing: props.median_line_spacing,
            column_count: props.column_count,
        })
    }

    // -- editor DOM ----------------------------------------------------------

    /// `doc.page(index) #=> PdfDioxide::PdfPage` — editable page snapshot.
    fn page(ruby: &Ruby, rb_self: &Self, index: usize) -> Result<RbPdfPage, Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let page = editor.get_page(index).map_err(|e| map_pdf_error(ruby, e))?;
        Ok(RbPdfPage(RefCell::new(page)))
    }

    /// `doc.save_page(page)` — write a modified `PdfPage` back.
    fn save_page(ruby: &Ruby, rb_self: &Self, page: &RbPdfPage) -> Result<(), Error> {
        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .save_page(page.0.borrow().clone())
            .map_err(|e| map_pdf_error(ruby, e))
    }

    // -- saving -------------------------------------------------------------

    fn save_options_from_kwargs(kw: RHash) -> Result<SaveOptions, Error> {
        let kw = get_kwargs::<_, (), (Option<bool>, Option<bool>, Option<bool>), ()>(
            kw,
            &[],
            &["compress", "garbage_collect", "linearize"],
        )?;
        let (compress, garbage_collect, linearize) = kw.optional;
        Ok(SaveOptions {
            compress: compress.unwrap_or(true),
            garbage_collect: garbage_collect.unwrap_or(true),
            linearize: linearize.unwrap_or(false),
            incremental: false,
            encryption: None,
        })
    }

    /// `doc.save(path, compress: true, garbage_collect: true, linearize: false)`
    fn save(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<(), Error> {
        let args = scan_args::<(String,), (), (), (), RHash, ()>(args)?;
        let (path,) = args.required;
        let options = Self::save_options_from_kwargs(args.keywords)?;

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .save_with_options(&path, options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_bytes(compress: true, garbage_collect: true, linearize: false)
    /// #=> String (binary)`
    fn to_bytes(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RString, Error> {
        let args = scan_args::<(), (), (), (), RHash, ()>(args)?;
        let options = Self::save_options_from_kwargs(args.keywords)?;

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let bytes = editor
            .save_to_bytes_with_options(options)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }

    fn encryption_config_from_args(
        user_password: String,
        owner_password: Option<String>,
        kw: RHash,
    ) -> Result<EncryptionConfig, Error> {
        type Opts = (Option<bool>, Option<bool>, Option<bool>, Option<bool>);
        let kw = get_kwargs::<_, (), Opts, ()>(
            kw,
            &[],
            &["allow_print", "allow_copy", "allow_modify", "allow_annotate"],
        )?;
        let (allow_print, allow_copy, allow_modify, allow_annotate) = kw.optional;
        let allow_print = allow_print.unwrap_or(true);
        let allow_copy = allow_copy.unwrap_or(true);
        let allow_modify = allow_modify.unwrap_or(true);
        let allow_annotate = allow_annotate.unwrap_or(true);

        let owner_pwd = owner_password.unwrap_or_else(|| user_password.clone());
        let permissions = Permissions {
            print: allow_print,
            print_high_quality: allow_print,
            modify: allow_modify,
            copy: allow_copy,
            annotate: allow_annotate,
            fill_forms: allow_annotate,
            accessibility: true,
            assemble: allow_modify,
        };
        Ok(EncryptionConfig::new(user_password, owner_pwd)
            .with_algorithm(EncryptionAlgorithm::Aes256)
            .with_permissions(permissions))
    }

    /// `doc.save_encrypted(path, user_password, owner_password = nil,
    /// allow_print: true, allow_copy: true, allow_modify: true,
    /// allow_annotate: true)` — AES-256 encrypted save.
    fn save_encrypted(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<(), Error> {
        let args = scan_args::<(String, String), (Option<String>,), (), (), RHash, ()>(args)?;
        let (path, user_password) = args.required;
        let (owner_password,) = args.optional;
        let config = Self::encryption_config_from_args(user_password, owner_password, args.keywords)?;
        let options = SaveOptions::with_encryption(config);

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        editor
            .save_with_options(&path, options)
            .map_err(|e| map_pdf_error(ruby, e))
    }

    /// `doc.to_bytes_encrypted(user_password, owner_password = nil, **opts)
    /// #=> String (binary)` — like `save_encrypted` but in memory.
    fn to_bytes_encrypted(ruby: &Ruby, rb_self: &Self, args: &[Value]) -> Result<RString, Error> {
        let args = scan_args::<(String,), (Option<String>,), (), (), RHash, ()>(args)?;
        let (user_password,) = args.required;
        let (owner_password,) = args.optional;
        let config = Self::encryption_config_from_args(user_password, owner_password, args.keywords)?;
        let options = SaveOptions::with_encryption(config);

        let mut inner = rb_self.0.borrow_mut();
        let editor = inner.ensure_editor(ruby)?;
        let bytes = editor
            .save_to_bytes_with_options(options)
            .map_err(|e| map_pdf_error(ruby, e))?;
        Ok(ruby.str_from_slice(&bytes))
    }
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module = ruby.get_inner(&PDF_OXIDE);

    // Force the exception classes into existence now, not on first raise.
    ruby.get_inner(&ERROR);
    ruby.get_inner(&IO_ERROR);
    ruby.get_inner(&PARSE_ERROR);
    ruby.get_inner(&PASSWORD_ERROR);
    ruby.get_inner(&UNSUPPORTED_ERROR);

    // -- text object classes ------------------------------------------------
    let text_char = module.define_class("TextChar", ruby.class_object())?;
    text_char.define_method("char", method!(RbTextChar::char, 0))?;
    text_char.define_method("bbox", method!(RbTextChar::bbox, 0))?;
    text_char.define_method("font_name", method!(RbTextChar::font_name, 0))?;
    text_char.define_method("font_size", method!(RbTextChar::font_size, 0))?;
    text_char.define_method("font_weight", method!(RbTextChar::font_weight, 0))?;
    text_char.define_method("is_italic", method!(RbTextChar::is_italic, 0))?;
    text_char.define_method("is_monospace", method!(RbTextChar::is_monospace, 0))?;
    text_char.define_method("color", method!(RbTextChar::color, 0))?;
    text_char.define_method("rotation_degrees", method!(RbTextChar::rotation_degrees, 0))?;
    text_char.define_method("origin_x", method!(RbTextChar::origin_x, 0))?;
    text_char.define_method("origin_y", method!(RbTextChar::origin_y, 0))?;
    text_char.define_method("advance_width", method!(RbTextChar::advance_width, 0))?;
    text_char.define_method("mcid", method!(RbTextChar::mcid, 0))?;

    let text_span = module.define_class("TextSpan", ruby.class_object())?;
    text_span.define_method("text", method!(RbTextSpan::text, 0))?;
    text_span.define_method("bbox", method!(RbTextSpan::bbox, 0))?;
    text_span.define_method("page_bbox", method!(RbTextSpan::page_bbox, 0))?;
    text_span.define_method("font_name", method!(RbTextSpan::font_name, 0))?;
    text_span.define_method("font_size", method!(RbTextSpan::font_size, 0))?;
    text_span.define_method("is_bold", method!(RbTextSpan::is_bold, 0))?;
    text_span.define_method("is_italic", method!(RbTextSpan::is_italic, 0))?;
    text_span.define_method("is_monospace", method!(RbTextSpan::is_monospace, 0))?;
    text_span.define_method("char_widths", method!(RbTextSpan::char_widths, 0))?;
    text_span.define_method("color", method!(RbTextSpan::color, 0))?;
    text_span.define_method("sequence", method!(RbTextSpan::sequence, 0))?;
    text_span.define_method("provenance", method!(RbTextSpan::provenance, 0))?;

    let text_word = module.define_class("TextWord", ruby.class_object())?;
    text_word.define_method("text", method!(RbTextWord::text, 0))?;
    text_word.define_method("bbox", method!(RbTextWord::bbox, 0))?;
    text_word.define_method("font_name", method!(RbTextWord::font_name, 0))?;
    text_word.define_method("font_size", method!(RbTextWord::font_size, 0))?;
    text_word.define_method("is_bold", method!(RbTextWord::is_bold, 0))?;
    text_word.define_method("is_italic", method!(RbTextWord::is_italic, 0))?;
    text_word.define_method("chars", method!(RbTextWord::chars, 0))?;
    text_word.define_method("sequence", method!(RbTextWord::sequence, 0))?;
    text_word.define_method("rotation_degrees", method!(RbTextWord::rotation_degrees, 0))?;

    let text_line = module.define_class("TextLine", ruby.class_object())?;
    text_line.define_method("text", method!(RbTextLine::text, 0))?;
    text_line.define_method("bbox", method!(RbTextLine::bbox, 0))?;
    text_line.define_method("words", method!(RbTextLine::words, 0))?;
    text_line.define_method("chars", method!(RbTextLine::chars, 0))?;

    // -- module-level functions ----------------------------------------------
    module.define_module_function(
        "sign_pdf_bytes_pades",
        function!(sign_pdf_bytes_pades, -1),
    )?;
    module.define_module_function(
        "has_document_timestamp",
        function!(has_document_timestamp, 1),
    )?;
    module.define_module_function("sign_pdf_bytes", function!(sign_pdf_bytes, -1))?;
    module.define_module_function(
        "plan_split_by_bookmarks",
        function!(plan_split_by_bookmarks, -1),
    )?;
    module.define_module_function("split_by_bookmarks", function!(split_by_bookmarks, -1))?;
    module.define_module_function("prefetch_models", function!(prefetch_models, 1))?;
    module.define_module_function("model_manifest", function!(model_manifest, 0))?;
    module.define_module_function("prefetch_available", function!(prefetch_available, 0))?;
    module.define_module_function(
        "crypto_active_provider",
        function!(crypto_active_provider, 0),
    )?;
    module.define_module_function(
        "crypto_available_providers",
        function!(crypto_available_providers, 0),
    )?;
    module.define_module_function("crypto_use_fips", function!(crypto_use_fips, 0))?;
    module.define_module_function("crypto_set_policy", function!(crypto_set_policy, 1))?;
    module.define_module_function("crypto_policy", function!(crypto_policy, 0))?;
    module.define_module_function("crypto_inventory", function!(crypto_inventory, 0))?;
    module.define_module_function("crypto_cbom", function!(crypto_cbom, 0))?;
    module.define_module_function(
        "set_max_ops_per_stream",
        function!(set_max_ops_per_stream, 1),
    )?;
    module.define_module_function(
        "set_preserve_unmapped_glyphs",
        function!(set_preserve_unmapped_glyphs, 1),
    )?;
    module.define_module_function("set_log_level", function!(set_log_level, 1))?;
    module.define_module_function("get_log_level", function!(get_log_level, 0))?;
    module.define_module_function("disable_logging", function!(disable_logging, 0))?;
    module.define_module_function("generate_barcode_svg", function!(generate_barcode_svg, 2))?;
    module.define_module_function("generate_qr_svg", function!(generate_qr_svg, 3))?;

    // -- writer value classes ------------------------------------------------
    let color = module.define_class("Color", ruby.class_object())?;
    color.define_singleton_method("new", function!(RbColor::new, 3))?;
    color.define_singleton_method("black", function!(RbColor::black, 0))?;
    color.define_singleton_method("white", function!(RbColor::white, 0))?;
    color.define_singleton_method("red", function!(RbColor::red, 0))?;
    color.define_singleton_method("green", function!(RbColor::green, 0))?;
    color.define_singleton_method("blue", function!(RbColor::blue, 0))?;
    color.define_singleton_method("from_hex", function!(RbColor::from_hex, 1))?;
    color.define_method("r", method!(RbColor::r, 0))?;
    color.define_method("g", method!(RbColor::g, 0))?;
    color.define_method("b", method!(RbColor::b, 0))?;

    let blend_mode = module.define_class("BlendMode", ruby.class_object())?;
    {
        use pdf_oxide::writer::BlendMode as BM;
        // Python exposes SCREAMING_CASE statics; Ruby gets idiomatic
        // lowercase class methods.
        blend_mode.define_singleton_method("normal", function!(|| RbBlendMode::make(BM::Normal), 0))?;
        blend_mode
            .define_singleton_method("multiply", function!(|| RbBlendMode::make(BM::Multiply), 0))?;
        blend_mode.define_singleton_method("screen", function!(|| RbBlendMode::make(BM::Screen), 0))?;
        blend_mode
            .define_singleton_method("overlay", function!(|| RbBlendMode::make(BM::Overlay), 0))?;
        blend_mode.define_singleton_method("darken", function!(|| RbBlendMode::make(BM::Darken), 0))?;
        blend_mode
            .define_singleton_method("lighten", function!(|| RbBlendMode::make(BM::Lighten), 0))?;
        blend_mode.define_singleton_method(
            "color_dodge",
            function!(|| RbBlendMode::make(BM::ColorDodge), 0),
        )?;
        blend_mode.define_singleton_method(
            "color_burn",
            function!(|| RbBlendMode::make(BM::ColorBurn), 0),
        )?;
        blend_mode.define_singleton_method(
            "hard_light",
            function!(|| RbBlendMode::make(BM::HardLight), 0),
        )?;
        blend_mode.define_singleton_method(
            "soft_light",
            function!(|| RbBlendMode::make(BM::SoftLight), 0),
        )?;
        blend_mode.define_singleton_method(
            "difference",
            function!(|| RbBlendMode::make(BM::Difference), 0),
        )?;
        blend_mode.define_singleton_method(
            "exclusion",
            function!(|| RbBlendMode::make(BM::Exclusion), 0),
        )?;
    }

    let ext_gstate = module.define_class("ExtGState", ruby.class_object())?;
    ext_gstate.define_singleton_method("new", function!(RbExtGState::new, 0))?;
    ext_gstate.define_singleton_method(
        "semi_transparent",
        function!(RbExtGState::semi_transparent, 0),
    )?;
    ext_gstate.define_method("alpha", method!(RbExtGState::alpha, 1))?;
    ext_gstate.define_method("fill_alpha", method!(RbExtGState::fill_alpha, 1))?;
    ext_gstate.define_method("stroke_alpha", method!(RbExtGState::stroke_alpha, 1))?;
    ext_gstate.define_method("blend_mode", method!(RbExtGState::blend_mode, 1))?;

    let linear_gradient = module.define_class("LinearGradient", ruby.class_object())?;
    linear_gradient.define_singleton_method("new", function!(RbLinearGradient::new, 0))?;
    linear_gradient
        .define_singleton_method("horizontal", function!(RbLinearGradient::horizontal, 3))?;
    linear_gradient.define_singleton_method("vertical", function!(RbLinearGradient::vertical, 3))?;
    linear_gradient.define_method("start", method!(RbLinearGradient::start, 2))?;
    linear_gradient.define_method("end", method!(RbLinearGradient::end, 2))?;
    linear_gradient.define_method("add_stop", method!(RbLinearGradient::add_stop, 2))?;

    let radial_gradient = module.define_class("RadialGradient", ruby.class_object())?;
    radial_gradient.define_singleton_method("new", function!(RbRadialGradient::new, 0))?;
    radial_gradient.define_singleton_method("centered", function!(RbRadialGradient::centered, 3))?;
    radial_gradient.define_method("inner_circle", method!(RbRadialGradient::inner_circle, 3))?;
    radial_gradient.define_method("outer_circle", method!(RbRadialGradient::outer_circle, 3))?;
    radial_gradient.define_method("add_stop", method!(RbRadialGradient::add_stop, 2))?;

    let line_cap = module.define_class("LineCap", ruby.class_object())?;
    {
        use pdf_oxide::writer::LineCap as LC;
        line_cap.define_singleton_method("butt", function!(|| RbLineCap(LC::Butt), 0))?;
        line_cap.define_singleton_method("round", function!(|| RbLineCap(LC::Round), 0))?;
        line_cap.define_singleton_method("square", function!(|| RbLineCap(LC::Square), 0))?;
    }
    line_cap.define_method("inspect", method!(RbLineCap::inspect, 0))?;

    let line_join = module.define_class("LineJoin", ruby.class_object())?;
    {
        use pdf_oxide::writer::LineJoin as LJ;
        line_join.define_singleton_method("miter", function!(|| RbLineJoin(LJ::Miter), 0))?;
        line_join.define_singleton_method("round", function!(|| RbLineJoin(LJ::Round), 0))?;
        line_join.define_singleton_method("bevel", function!(|| RbLineJoin(LJ::Bevel), 0))?;
    }
    line_join.define_method("inspect", method!(RbLineJoin::inspect, 0))?;

    let pattern_presets = module.define_class("PatternPresets", ruby.class_object())?;
    pattern_presets.define_singleton_method(
        "horizontal_stripes",
        function!(RbPatternPresets::horizontal_stripes, 4),
    )?;
    pattern_presets.define_singleton_method(
        "vertical_stripes",
        function!(RbPatternPresets::vertical_stripes, 4),
    )?;
    pattern_presets
        .define_singleton_method("checkerboard", function!(RbPatternPresets::checkerboard, 3))?;
    pattern_presets.define_singleton_method("dots", function!(RbPatternPresets::dots, 3))?;
    pattern_presets.define_singleton_method(
        "diagonal_lines",
        function!(RbPatternPresets::diagonal_lines, 3),
    )?;
    pattern_presets
        .define_singleton_method("crosshatch", function!(RbPatternPresets::crosshatch, 3))?;

    let artifact_style = module.define_class("ArtifactStyle", ruby.class_object())?;
    artifact_style.define_singleton_method("new", function!(RbArtifactStyle::new, 0))?;
    artifact_style.define_method("font", method!(RbArtifactStyle::font, 2))?;
    artifact_style.define_method("bold", method!(RbArtifactStyle::bold, 0))?;

    let artifact = module.define_class("Artifact", ruby.class_object())?;
    artifact.define_singleton_method("new", function!(RbArtifact::new, 0))?;
    artifact.define_singleton_method("center", function!(RbArtifact::center, 1))?;
    artifact.define_method("with_left", method!(RbArtifact::with_left, 1))?;

    let header = module.define_class("Header", ruby.class_object())?;
    header.define_singleton_method("new", function!(RbHeader::new, 0))?;
    header.define_singleton_method("center", function!(RbHeader::center, 1))?;

    let footer = module.define_class("Footer", ruby.class_object())?;
    footer.define_singleton_method("new", function!(RbFooter::new, 0))?;
    footer.define_singleton_method("center", function!(RbFooter::center, 1))?;

    let page_template = module.define_class("PageTemplate", ruby.class_object())?;
    page_template.define_singleton_method("new", function!(RbPageTemplate::new, 0))?;
    page_template.define_method("header", method!(RbPageTemplate::header, 1))?;
    page_template.define_method("footer", method!(RbPageTemplate::footer, 1))?;

    let column = module.define_class("Column", ruby.class_object())?;
    column.define_singleton_method("new", function!(RbColumn::new, -1))?;
    column.define_method("header", method!(RbColumn::header, 0))?;
    column.define_method("width", method!(RbColumn::width, 0))?;
    column.define_method("align", method!(RbColumn::align, 0))?;
    column.define_method("inspect", method!(RbColumn::inspect, 0))?;

    let table = module.define_class("Table", ruby.class_object())?;
    table.define_singleton_method("new", function!(RbTable::new, -1))?;
    table.define_method("inspect", method!(RbTable::inspect, 0))?;

    // -- PDF creation --------------------------------------------------------
    let pdf = module.define_class("Pdf", ruby.class_object())?;
    pdf.define_singleton_method("from_markdown", function!(RbPdf::from_markdown, -1))?;
    pdf.define_singleton_method("from_html", function!(RbPdf::from_html, -1))?;
    pdf.define_singleton_method("from_text", function!(RbPdf::from_text, -1))?;
    pdf.define_singleton_method(
        "from_markdown_with_template",
        function!(RbPdf::from_markdown_with_template, -1),
    )?;
    pdf.define_singleton_method("from_html_css", function!(RbPdf::from_html_css, 3))?;
    pdf.define_singleton_method(
        "from_html_css_with_fonts",
        function!(RbPdf::from_html_css_with_fonts, 3),
    )?;
    pdf.define_singleton_method("from_image", function!(RbPdf::from_image, 1))?;
    pdf.define_singleton_method("from_images", function!(RbPdf::from_images, 1))?;
    pdf.define_singleton_method("from_image_bytes", function!(RbPdf::from_image_bytes, 1))?;
    pdf.define_singleton_method("from_bytes", function!(RbPdf::from_bytes, 1))?;
    pdf.define_singleton_method("merge", function!(RbPdf::merge, 1))?;
    pdf.define_method("save", method!(RbPdf::save, 1))?;
    pdf.define_method("to_bytes", method!(RbPdf::to_bytes, 0))?;
    pdf.define_method("length", method!(RbPdf::length, 0))?;
    pdf.define_method("size", method!(RbPdf::length, 0))?;
    pdf.define_method("inspect", method!(RbPdf::inspect, 0))?;

    let office = module.define_class("OfficeConverter", ruby.class_object())?;
    office.define_singleton_method("new", function!(RbOfficeConverter::new, 0))?;
    office.define_singleton_method("from_docx", function!(RbOfficeConverter::from_docx, 1))?;
    office.define_singleton_method(
        "from_docx_bytes",
        function!(RbOfficeConverter::from_docx_bytes, 1),
    )?;
    office.define_singleton_method("from_xlsx", function!(RbOfficeConverter::from_xlsx, 1))?;
    office.define_singleton_method(
        "from_xlsx_bytes",
        function!(RbOfficeConverter::from_xlsx_bytes, 1),
    )?;
    office.define_singleton_method("from_pptx", function!(RbOfficeConverter::from_pptx, 1))?;
    office.define_singleton_method(
        "from_pptx_bytes",
        function!(RbOfficeConverter::from_pptx_bytes, 1),
    )?;
    office.define_singleton_method("convert", function!(RbOfficeConverter::convert, 1))?;

    let embedded_font = module.define_class("EmbeddedFont", ruby.class_object())?;
    embedded_font.define_singleton_method("from_file", function!(RbEmbeddedFont::from_file, 1))?;
    embedded_font.define_singleton_method("from_bytes", function!(RbEmbeddedFont::from_bytes, -1))?;
    embedded_font.define_method("name", method!(RbEmbeddedFont::name, 0))?;
    embedded_font.define_method("inspect", method!(RbEmbeddedFont::inspect, 0))?;

    let doc_builder = module.define_class("DocumentBuilder", ruby.class_object())?;
    doc_builder.define_singleton_method("new", function!(RbDocumentBuilder::new, 0))?;
    doc_builder.define_method("title", method!(RbDocumentBuilder::title, 1))?;
    doc_builder.define_method("author", method!(RbDocumentBuilder::author, 1))?;
    doc_builder.define_method("subject", method!(RbDocumentBuilder::subject, 1))?;
    doc_builder.define_method("keywords", method!(RbDocumentBuilder::keywords, 1))?;
    doc_builder.define_method("creator", method!(RbDocumentBuilder::creator, 1))?;
    doc_builder.define_method("on_open", method!(RbDocumentBuilder::on_open, 1))?;
    doc_builder.define_method("tagged_pdf_ua1", method!(RbDocumentBuilder::tagged_pdf_ua1, 0))?;
    doc_builder.define_method("language", method!(RbDocumentBuilder::language, 1))?;
    doc_builder.define_method("role_map", method!(RbDocumentBuilder::role_map, 2))?;
    doc_builder.define_method(
        "register_embedded_font",
        method!(RbDocumentBuilder::register_embedded_font, 2),
    )?;
    doc_builder.define_method("build", method!(RbDocumentBuilder::build, 0))?;
    doc_builder.define_method("save", method!(RbDocumentBuilder::save, 1))?;
    doc_builder.define_method("save_encrypted", method!(RbDocumentBuilder::save_encrypted, 3))?;
    doc_builder.define_method(
        "to_bytes_encrypted",
        method!(RbDocumentBuilder::to_bytes_encrypted, 2),
    )?;
    // Internal: replay target for the pure-Ruby FluentPageBuilder.
    doc_builder.define_private_method("_apply_page", method!(RbDocumentBuilder::apply_page, 2))?;

    // Internal record-time helpers for the pure-Ruby FluentPageBuilder.
    module.define_module_function("_render_barcode_1d", function!(render_barcode_1d, 4))?;
    module.define_module_function("_render_barcode_qr", function!(render_barcode_qr, 2))?;
    module.define_module_function("_measure_text", function!(measure_text, 3))?;

    // -- extraction tuning classes -------------------------------------------
    let profile = module.define_class("ExtractionProfile", ruby.class_object())?;
    profile.define_singleton_method("conservative", function!(RbExtractionProfile::conservative, 0))?;
    profile.define_singleton_method("aggressive", function!(RbExtractionProfile::aggressive, 0))?;
    profile.define_singleton_method("balanced", function!(RbExtractionProfile::balanced, 0))?;
    profile.define_singleton_method("academic", function!(RbExtractionProfile::academic, 0))?;
    profile.define_singleton_method("policy", function!(RbExtractionProfile::policy, 0))?;
    profile.define_singleton_method("form", function!(RbExtractionProfile::form, 0))?;
    profile.define_singleton_method("government", function!(RbExtractionProfile::government, 0))?;
    profile.define_singleton_method("scanned_ocr", function!(RbExtractionProfile::scanned_ocr, 0))?;
    profile.define_singleton_method("adaptive", function!(RbExtractionProfile::adaptive, 0))?;
    profile.define_singleton_method("available", function!(RbExtractionProfile::available, 0))?;
    profile.define_method("name", method!(RbExtractionProfile::name, 0))?;
    profile.define_method(
        "tj_offset_threshold",
        method!(RbExtractionProfile::tj_offset_threshold, 0),
    )?;
    profile.define_method(
        "word_margin_ratio",
        method!(RbExtractionProfile::word_margin_ratio, 0),
    )?;
    profile.define_method(
        "space_threshold_em_ratio",
        method!(RbExtractionProfile::space_threshold_em_ratio, 0),
    )?;
    profile.define_method(
        "space_char_multiplier",
        method!(RbExtractionProfile::space_char_multiplier, 0),
    )?;
    profile.define_method(
        "use_adaptive_threshold",
        method!(RbExtractionProfile::use_adaptive_threshold, 0),
    )?;
    profile.define_method("inspect", method!(RbExtractionProfile::inspect, 0))?;

    let layout_params = module.define_class("LayoutParams", ruby.class_object())?;
    layout_params.define_method(
        "word_gap_threshold",
        method!(RbLayoutParams::word_gap_threshold, 0),
    )?;
    layout_params.define_method(
        "line_gap_threshold",
        method!(RbLayoutParams::line_gap_threshold, 0),
    )?;
    layout_params.define_method(
        "median_char_width",
        method!(RbLayoutParams::median_char_width, 0),
    )?;
    layout_params.define_method(
        "median_font_size",
        method!(RbLayoutParams::median_font_size, 0),
    )?;
    layout_params.define_method(
        "median_line_spacing",
        method!(RbLayoutParams::median_line_spacing, 0),
    )?;
    layout_params.define_method("column_count", method!(RbLayoutParams::column_count, 0))?;
    layout_params.define_method("inspect", method!(RbLayoutParams::inspect, 0))?;

    // -- signature classes ---------------------------------------------------
    let signature = module.define_class("Signature", ruby.class_object())?;
    signature.define_method("signer_name", method!(RbSignature::signer_name, 0))?;
    signature.define_method("reason", method!(RbSignature::reason, 0))?;
    signature.define_method("location", method!(RbSignature::location, 0))?;
    signature.define_method("contact_info", method!(RbSignature::contact_info, 0))?;
    signature.define_method("signing_time", method!(RbSignature::signing_time, 0))?;
    signature.define_method(
        "covers_whole_document",
        method!(RbSignature::covers_whole_document, 0),
    )?;
    signature.define_method("pades_level", method!(RbSignature::pades_level, 0))?;
    signature.define_method("verify", method!(RbSignature::verify, 0))?;
    signature.define_method("verify_detached", method!(RbSignature::verify_detached, 1))?;
    signature.define_method("inspect", method!(RbSignature::inspect, 0))?;

    let certificate = module.define_class("Certificate", ruby.class_object())?;
    certificate.define_singleton_method("load", function!(RbCertificate::load, 1))?;
    certificate.define_singleton_method("load_pem", function!(RbCertificate::load_pem, 2))?;
    certificate.define_singleton_method("load_pkcs12", function!(RbCertificate::load_pkcs12, 2))?;
    certificate.define_method("subject", method!(RbCertificate::subject, 0))?;
    certificate.define_method("issuer", method!(RbCertificate::issuer, 0))?;
    certificate.define_method("serial", method!(RbCertificate::serial, 0))?;
    certificate.define_method("validity", method!(RbCertificate::validity, 0))?;
    certificate.define_method("is_valid", method!(RbCertificate::is_valid, 0))?;
    certificate.define_method("inspect", method!(RbCertificate::inspect, 0))?;

    let timestamp = module.define_class("Timestamp", ruby.class_object())?;
    timestamp.define_singleton_method("parse", function!(RbTimestamp::parse, 1))?;
    timestamp.define_method("time", method!(RbTimestamp::time, 0))?;
    timestamp.define_method("serial", method!(RbTimestamp::serial, 0))?;
    timestamp.define_method("policy_oid", method!(RbTimestamp::policy_oid, 0))?;
    timestamp.define_method("tsa_name", method!(RbTimestamp::tsa_name, 0))?;
    timestamp.define_method("hash_algorithm", method!(RbTimestamp::hash_algorithm, 0))?;
    timestamp.define_method("message_imprint", method!(RbTimestamp::message_imprint, 0))?;
    timestamp.define_method("verify", method!(RbTimestamp::verify, 0))?;
    timestamp.define_method("inspect", method!(RbTimestamp::inspect, 0))?;

    let dss_class = module.define_class("Dss", ruby.class_object())?;
    dss_class.define_method("certs", method!(RbDss::certs, 0))?;
    dss_class.define_method("crls", method!(RbDss::crls, 0))?;
    dss_class.define_method("ocsps", method!(RbDss::ocsps, 0))?;
    dss_class.define_method("vri", method!(RbDss::vri, 0))?;

    let revocation = module.define_class("RevocationMaterial", ruby.class_object())?;
    revocation.define_singleton_method("new", function!(RbRevocationMaterial::new, -1))?;

    // -- editor DOM classes --------------------------------------------------
    let text_id = module.define_class("PdfTextId", ruby.class_object())?;
    text_id.define_method("inspect", method!(RbPdfTextId::inspect, 0))?;

    let pdf_text = module.define_class("PdfText", ruby.class_object())?;
    pdf_text.define_method("id", method!(RbPdfText::id, 0))?;
    pdf_text.define_method("value", method!(RbPdfText::value, 0))?;
    pdf_text.define_method("text", method!(RbPdfText::value, 0))?;
    pdf_text.define_method("bbox", method!(RbPdfText::bbox, 0))?;
    pdf_text.define_method("font_name", method!(RbPdfText::font_name, 0))?;
    pdf_text.define_method("font_size", method!(RbPdfText::font_size, 0))?;
    pdf_text.define_method("is_bold", method!(RbPdfText::is_bold, 0))?;
    pdf_text.define_method("is_italic", method!(RbPdfText::is_italic, 0))?;
    pdf_text.define_method("contains", method!(RbPdfText::contains, 1))?;
    pdf_text.define_method("starts_with", method!(RbPdfText::starts_with, 1))?;
    pdf_text.define_method("ends_with", method!(RbPdfText::ends_with, 1))?;
    pdf_text.define_method("inspect", method!(RbPdfText::inspect, 0))?;

    let pdf_image = module.define_class("PdfImage", ruby.class_object())?;
    pdf_image.define_method("bbox", method!(RbPdfImage::bbox, 0))?;
    pdf_image.define_method("width", method!(RbPdfImage::width, 0))?;
    pdf_image.define_method("height", method!(RbPdfImage::height, 0))?;
    pdf_image.define_method("aspect_ratio", method!(RbPdfImage::aspect_ratio, 0))?;
    pdf_image.define_method("inspect", method!(RbPdfImage::inspect, 0))?;

    let pdf_annotation = module.define_class("PdfAnnotation", ruby.class_object())?;
    pdf_annotation.define_method("subtype", method!(RbPdfAnnotation::subtype, 0))?;
    pdf_annotation.define_method("rect", method!(RbPdfAnnotation::rect, 0))?;
    pdf_annotation.define_method("contents", method!(RbPdfAnnotation::contents, 0))?;
    pdf_annotation.define_method("color", method!(RbPdfAnnotation::color, 0))?;
    pdf_annotation.define_method("is_modified", method!(RbPdfAnnotation::is_modified, 0))?;
    pdf_annotation.define_method("is_new", method!(RbPdfAnnotation::is_new, 0))?;
    pdf_annotation.define_method("inspect", method!(RbPdfAnnotation::inspect, 0))?;

    let pdf_element = module.define_class("PdfElement", ruby.class_object())?;
    pdf_element.define_method("is_text", method!(RbPdfElement::is_text, 0))?;
    pdf_element.define_method("is_image", method!(RbPdfElement::is_image, 0))?;
    pdf_element.define_method("is_path", method!(RbPdfElement::is_path, 0))?;
    pdf_element.define_method("is_table", method!(RbPdfElement::is_table, 0))?;
    pdf_element.define_method("is_structure", method!(RbPdfElement::is_structure, 0))?;
    pdf_element.define_method("as_text", method!(RbPdfElement::as_text, 0))?;
    pdf_element.define_method("as_image", method!(RbPdfElement::as_image, 0))?;
    pdf_element.define_method("bbox", method!(RbPdfElement::bbox, 0))?;
    pdf_element.define_method("inspect", method!(RbPdfElement::inspect, 0))?;

    let pdf_page = module.define_class("PdfPage", ruby.class_object())?;
    pdf_page.define_method("index", method!(RbPdfPage::index, 0))?;
    pdf_page.define_method("width", method!(RbPdfPage::width, 0))?;
    pdf_page.define_method("height", method!(RbPdfPage::height, 0))?;
    pdf_page.define_method("children", method!(RbPdfPage::children, 0))?;
    pdf_page.define_method(
        "find_text_containing",
        method!(RbPdfPage::find_text_containing, 1),
    )?;
    pdf_page.define_method("find_images", method!(RbPdfPage::find_images, 0))?;
    pdf_page.define_method("set_text", method!(RbPdfPage::set_text, 2))?;
    pdf_page.define_method("annotations", method!(RbPdfPage::annotations, 0))?;
    pdf_page.define_method("add_link", method!(RbPdfPage::add_link, 5))?;
    pdf_page.define_method("add_highlight", method!(RbPdfPage::add_highlight, 5))?;
    pdf_page.define_method("add_note", method!(RbPdfPage::add_note, 3))?;
    pdf_page.define_method("remove_annotation", method!(RbPdfPage::remove_annotation, 1))?;
    pdf_page.define_method("add_text", method!(RbPdfPage::add_text, -1))?;
    pdf_page.define_method("remove_element", method!(RbPdfPage::remove_element, 1))?;
    pdf_page.define_method("inspect", method!(RbPdfPage::inspect, 0))?;

    // -- form field class ----------------------------------------------------
    let form_field = module.define_class("FormField", ruby.class_object())?;
    form_field.define_method("name", method!(RbFormField::name, 0))?;
    form_field.define_method("field_type", method!(RbFormField::field_type, 0))?;
    form_field.define_method("value", method!(RbFormField::value, 0))?;
    form_field.define_method("tooltip", method!(RbFormField::tooltip, 0))?;
    form_field.define_method("bounds", method!(RbFormField::bounds, 0))?;
    form_field.define_method("flags", method!(RbFormField::flags, 0))?;
    form_field.define_method("max_length", method!(RbFormField::max_length, 0))?;
    form_field.define_method("is_readonly", method!(RbFormField::is_readonly, 0))?;
    form_field.define_method("is_required", method!(RbFormField::is_required, 0))?;
    form_field.define_method("inspect", method!(RbFormField::inspect, 0))?;

    let class = module.define_class("PdfDocument", ruby.class_object())?;

    // search
    class.define_method("search", method!(RbPdfDocument::search, -1))?;
    class.define_method("search_page", method!(RbPdfDocument::search_page, -1))?;
    class.define_method("prepare_search", method!(RbPdfDocument::prepare_search, 0))?;
    class.define_method("clear_search_index", method!(RbPdfDocument::clear_search_index, 0))?;

    // images / tables / vector content
    class.define_method("extract_images", method!(RbPdfDocument::extract_images, -1))?;
    class.define_method("extract_image_bytes", method!(RbPdfDocument::extract_image_bytes, 1))?;
    class.define_method("extract_tables", method!(RbPdfDocument::extract_tables, -1))?;
    class.define_method("extract_paths", method!(RbPdfDocument::extract_paths, -1))?;
    class.define_method("extract_rects", method!(RbPdfDocument::extract_rects, -1))?;
    class.define_method("extract_lines", method!(RbPdfDocument::extract_lines, -1))?;

    // page-level structured output
    class.define_method("extract_page_text", method!(RbPdfDocument::extract_page_text, -1))?;
    class.define_method("get_outline", method!(RbPdfDocument::get_outline, 0))?;
    class.define_method("extract_structured", method!(RbPdfDocument::extract_structured, 1))?;
    class.define_method("get_annotations", method!(RbPdfDocument::get_annotations, 1))?;
    class.define_method("classify_page", method!(RbPdfDocument::classify_page, 1))?;
    class.define_method("classify_document", method!(RbPdfDocument::classify_document, 0))?;
    class.define_method("extract_page_auto", method!(RbPdfDocument::extract_page_auto, -1))?;

    // forms
    class.define_method("get_form_fields", method!(RbPdfDocument::get_form_fields, 0))?;
    class.define_method("get_form_field_value", method!(RbPdfDocument::get_form_field_value, 1))?;
    class.define_method("set_form_field_value", method!(RbPdfDocument::set_form_field_value, 2))?;
    class.define_method("has_xfa", method!(RbPdfDocument::has_xfa, 0))?;
    class.define_method("export_form_data", method!(RbPdfDocument::export_form_data, -1))?;
    class.define_method("flatten_forms", method!(RbPdfDocument::flatten_forms, 0))?;
    class.define_method("flatten_forms_on_page", method!(RbPdfDocument::flatten_forms_on_page, 1))?;
    class.define_method("flatten_warnings", method!(RbPdfDocument::flatten_warnings, 0))?;

    // document assembly
    class.define_method("merge_from", method!(RbPdfDocument::merge_from, 1))?;
    class.define_method("embed_file", method!(RbPdfDocument::embed_file, 2))?;
    class.define_method("page_labels", method!(RbPdfDocument::page_labels, 0))?;
    class.define_method("xmp_metadata", method!(RbPdfDocument::xmp_metadata, 0))?;

    // compliance
    class.define_method("validate_pdf_a", method!(RbPdfDocument::validate_pdf_a, -1))?;
    class.define_method("convert_to_pdf_a", method!(RbPdfDocument::convert_to_pdf_a, -1))?;
    class.define_method("validate_pdf_ua", method!(RbPdfDocument::validate_pdf_ua, 0))?;
    class.define_method("validate_pdf_x", method!(RbPdfDocument::validate_pdf_x, -1))?;

    // page operations
    class.define_method("extract_pages", method!(RbPdfDocument::extract_pages, 2))?;
    class.define_method(
        "extract_pages_to_bytes",
        method!(RbPdfDocument::extract_pages_to_bytes, 1),
    )?;
    class.define_method(
        "extract_page_ranges_to_bytes",
        method!(RbPdfDocument::extract_page_ranges_to_bytes, 1),
    )?;
    class.define_method("select_pages", method!(RbPdfDocument::select_pages, 1))?;
    class.define_method("delete_page", method!(RbPdfDocument::delete_page, 1))?;
    class.define_method("move_page", method!(RbPdfDocument::move_page, 2))?;
    class.define_method("flatten_to_images", method!(RbPdfDocument::flatten_to_images, -1))?;

    // constructors
    class.define_singleton_method("open", function!(RbPdfDocument::open, -1))?;
    class.define_singleton_method("from_bytes", function!(RbPdfDocument::from_bytes, -1))?;

    // basics
    class.define_method("version", method!(RbPdfDocument::version, 0))?;
    class.define_method("authenticate", method!(RbPdfDocument::authenticate, 1))?;
    class.define_method("page_count", method!(RbPdfDocument::page_count, 0))?;
    class.define_method("has_structure_tree", method!(RbPdfDocument::has_structure_tree, 0))?;
    class.define_method("get_layers", method!(RbPdfDocument::get_layers, 0))?;
    class.define_method("get_page_inks", method!(RbPdfDocument::get_page_inks, 1))?;
    class.define_method("get_page_inks_deep", method!(RbPdfDocument::get_page_inks_deep, 1))?;

    // text extraction
    class.define_method("extract_text", method!(RbPdfDocument::extract_text, -1))?;
    class.define_method("extract_text_auto", method!(RbPdfDocument::extract_text_auto, 1))?;
    class.define_method("extract_chars", method!(RbPdfDocument::extract_chars, -1))?;
    class.define_method("extract_words", method!(RbPdfDocument::extract_words, -1))?;
    class.define_method("extract_text_lines", method!(RbPdfDocument::extract_text_lines, -1))?;
    class.define_method("extract_spans", method!(RbPdfDocument::extract_spans, -1))?;
    class.define_method("has_text_layer", method!(RbPdfDocument::has_text_layer, 1))?;
    class.define_method("permissions", method!(RbPdfDocument::permissions, 0))?;
    class.define_method(
        "structured_warnings",
        method!(RbPdfDocument::structured_warnings, 0),
    )?;

    // rendering
    class.define_method("render_page", method!(RbPdfDocument::render_page, -1))?;
    class.define_method("render_page_fit", method!(RbPdfDocument::render_page_fit, -1))?;
    class.define_method("render_pixmap", method!(RbPdfDocument::render_pixmap, -1))?;
    class.define_method("render_separations", method!(RbPdfDocument::render_separations, -1))?;
    class.define_method("render_separation", method!(RbPdfDocument::render_separation, -1))?;

    // conversions
    class.define_method("to_plain_text", method!(RbPdfDocument::to_plain_text, -1))?;
    class.define_method("to_plain_text_all", method!(RbPdfDocument::to_plain_text_all, -1))?;
    class.define_method("to_markdown", method!(RbPdfDocument::to_markdown, -1))?;
    class.define_method("to_markdown_all", method!(RbPdfDocument::to_markdown_all, -1))?;
    class.define_method("to_html", method!(RbPdfDocument::to_html, -1))?;
    class.define_method("to_html_all", method!(RbPdfDocument::to_html_all, -1))?;

    // office export
    class.define_method("to_docx", method!(RbPdfDocument::to_docx, 1))?;
    class.define_method("to_docx_bytes", method!(RbPdfDocument::to_docx_bytes, 0))?;
    class.define_method("to_pptx", method!(RbPdfDocument::to_pptx, 1))?;
    class.define_method("to_pptx_bytes", method!(RbPdfDocument::to_pptx_bytes, 0))?;
    class.define_method("to_xlsx", method!(RbPdfDocument::to_xlsx, 1))?;
    class.define_method("to_xlsx_bytes", method!(RbPdfDocument::to_xlsx_bytes, 0))?;

    // header/footer/artifact removal
    class.define_method("remove_headers", method!(RbPdfDocument::remove_headers, -1))?;
    class.define_method("remove_footers", method!(RbPdfDocument::remove_footers, -1))?;
    class.define_method("remove_artifacts", method!(RbPdfDocument::remove_artifacts, -1))?;
    class.define_method("erase_header", method!(RbPdfDocument::erase_header, 1))?;
    class.define_method("edit_header", method!(RbPdfDocument::edit_header, 1))?;
    class.define_method("erase_footer", method!(RbPdfDocument::erase_footer, 1))?;
    class.define_method("edit_footer", method!(RbPdfDocument::edit_footer, 1))?;
    class.define_method("erase_artifacts", method!(RbPdfDocument::erase_artifacts, 1))?;

    // explicit erase regions
    class.define_method("erase_region", method!(RbPdfDocument::erase_region, 5))?;
    class.define_method("erase_regions", method!(RbPdfDocument::erase_regions, 2))?;
    class.define_method("clear_erase_regions", method!(RbPdfDocument::clear_erase_regions, 1))?;

    // rotation
    class.define_method("page_rotation", method!(RbPdfDocument::page_rotation, 1))?;
    class.define_method("set_page_rotation", method!(RbPdfDocument::set_page_rotation, 2))?;
    class.define_method("rotate_page", method!(RbPdfDocument::rotate_page, 2))?;
    class.define_method("rotate_all_pages", method!(RbPdfDocument::rotate_all_pages, 1))?;

    // page boxes
    class.define_method("page_media_box", method!(RbPdfDocument::page_media_box, 1))?;
    class.define_method("set_page_media_box", method!(RbPdfDocument::set_page_media_box, 5))?;
    class.define_method("page_crop_box", method!(RbPdfDocument::page_crop_box, 1))?;
    class.define_method("set_page_crop_box", method!(RbPdfDocument::set_page_crop_box, 5))?;
    class.define_method("crop_margins", method!(RbPdfDocument::crop_margins, 4))?;

    // annotation flattening
    class.define_method(
        "flatten_page_annotations",
        method!(RbPdfDocument::flatten_page_annotations, 1),
    )?;
    class.define_method(
        "flatten_all_annotations",
        method!(RbPdfDocument::flatten_all_annotations, 0),
    )?;
    class.define_method(
        "is_page_marked_for_flatten",
        method!(RbPdfDocument::is_page_marked_for_flatten, 1),
    )?;
    class.define_method(
        "unmark_page_for_flatten",
        method!(RbPdfDocument::unmark_page_for_flatten, 1),
    )?;

    // redaction
    class.define_method("add_redaction", method!(RbPdfDocument::add_redaction, -1))?;
    class.define_method("redaction_count", method!(RbPdfDocument::redaction_count, 1))?;
    class.define_method(
        "apply_page_redactions",
        method!(RbPdfDocument::apply_page_redactions, 1),
    )?;
    class.define_method(
        "apply_all_redactions",
        method!(RbPdfDocument::apply_all_redactions, 0),
    )?;
    class.define_method(
        "is_page_marked_for_redaction",
        method!(RbPdfDocument::is_page_marked_for_redaction, 1),
    )?;
    class.define_method(
        "unmark_page_for_redaction",
        method!(RbPdfDocument::unmark_page_for_redaction, 1),
    )?;
    class.define_method(
        "apply_redactions_destructive",
        method!(RbPdfDocument::apply_redactions_destructive, -1),
    )?;
    class.define_method("sanitize_document", method!(RbPdfDocument::sanitize_document, -1))?;

    // page images
    class.define_method("page_images", method!(RbPdfDocument::page_images, 1))?;
    class.define_method("reposition_image", method!(RbPdfDocument::reposition_image, 4))?;
    class.define_method("resize_image", method!(RbPdfDocument::resize_image, 4))?;
    class.define_method("set_image_bounds", method!(RbPdfDocument::set_image_bounds, 6))?;
    class.define_method(
        "clear_image_modifications",
        method!(RbPdfDocument::clear_image_modifications, 1),
    )?;
    class.define_method(
        "has_image_modifications",
        method!(RbPdfDocument::has_image_modifications, 1),
    )?;

    // metadata
    class.define_method("set_title", method!(RbPdfDocument::set_title, 1))?;
    class.define_method("set_author", method!(RbPdfDocument::set_author, 1))?;
    class.define_method("set_subject", method!(RbPdfDocument::set_subject, 1))?;
    class.define_method("set_keywords", method!(RbPdfDocument::set_keywords, 1))?;

    // signatures
    class.define_method("signatures", method!(RbPdfDocument::signatures, 0))?;
    class.define_method("signature_count", method!(RbPdfDocument::signature_count, 0))?;
    class.define_method("dss", method!(RbPdfDocument::dss, 0))?;

    // layout analysis
    class.define_method("page_layout_params", method!(RbPdfDocument::page_layout_params, 1))?;

    // editor DOM
    class.define_method("page", method!(RbPdfDocument::page, 1))?;
    class.define_method("save_page", method!(RbPdfDocument::save_page, 1))?;

    // saving
    class.define_method("save", method!(RbPdfDocument::save, -1))?;
    class.define_method("to_bytes", method!(RbPdfDocument::to_bytes, -1))?;
    class.define_method("save_encrypted", method!(RbPdfDocument::save_encrypted, -1))?;
    class.define_method("to_bytes_encrypted", method!(RbPdfDocument::to_bytes_encrypted, -1))?;

    // Version drift detection: the pdf_oxide crate actually linked in.
    module.const_set("UPSTREAM_VERSION", pdf_oxide::VERSION)?;

    ext::init(ruby, module)?;

    Ok(())
}
