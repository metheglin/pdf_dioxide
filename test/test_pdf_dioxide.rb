# frozen_string_literal: true

require "test_helper"

class TestPdfDioxide < Minitest::Test
  FIXTURE = File.expand_path("fixtures/hello.pdf", __dir__)
  ENCRYPTED_FIXTURE = File.expand_path("fixtures/encrypted_stub.pdf", __dir__)

  def test_that_it_has_a_version_number
    refute_nil ::PdfDioxide::VERSION
  end

  def test_open_returns_a_document
    doc = PdfDioxide::PdfDocument.open(FIXTURE)

    assert_instance_of PdfDioxide::PdfDocument, doc
  end

  def test_extract_text_round_trips_page_content
    doc = PdfDioxide::PdfDocument.open(FIXTURE)
    text = doc.extract_text(0)

    assert_includes text, "Hello from pdf_oxide"
    assert_equal Encoding::UTF_8, text.encoding
  end

  def test_page_count
    doc = PdfDioxide::PdfDocument.open(FIXTURE)

    assert_equal 1, doc.page_count
  end

  def test_error_hierarchy
    assert_operator PdfDioxide::Error, :<, StandardError
    assert_operator PdfDioxide::IoError, :<, PdfDioxide::Error
    assert_operator PdfDioxide::ParseError, :<, PdfDioxide::Error
    assert_operator PdfDioxide::PasswordError, :<, PdfDioxide::Error
    assert_operator PdfDioxide::UnsupportedError, :<, PdfDioxide::Error
  end

  def test_open_raises_io_error_for_a_missing_file
    error = assert_raises(PdfDioxide::IoError) do
      PdfDioxide::PdfDocument.open(File.join(__dir__, "fixtures/does_not_exist.pdf"))
    end

    refute_empty error.message
  end

  def test_open_raises_parse_error_for_a_non_pdf_file
    assert_raises(PdfDioxide::ParseError) do
      PdfDioxide::PdfDocument.open(__FILE__)
    end
  end

  def test_extract_text_raises_parse_error_for_an_out_of_range_page
    doc = PdfDioxide::PdfDocument.open(FIXTURE)

    assert_raises(PdfDioxide::ParseError) { doc.extract_text(99) }
  end

  def test_page_count_raises_password_error_for_an_undecryptable_pdf
    doc = PdfDioxide::PdfDocument.open(ENCRYPTED_FIXTURE)

    error = assert_raises(PdfDioxide::PasswordError) { doc.page_count }

    assert_match(/password/i, error.message)
  end

  def test_specific_errors_are_rescuable_as_the_base_error
    assert_raises(PdfDioxide::Error) do
      PdfDioxide::PdfDocument.open(File.join(__dir__, "fixtures/does_not_exist.pdf"))
    end
  end
end
