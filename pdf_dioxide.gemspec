# frozen_string_literal: true

require_relative "lib/pdf_dioxide/version"

Gem::Specification.new do |spec|
  spec.name = "pdf_dioxide"
  spec.version = PdfDioxide::VERSION
  spec.authors = ["metheglin"]
  spec.email = ["pigmybank@gmail.com"]

  spec.summary = "Ruby bindings for the pdf_oxide PDF engine, built with Magnus."
  spec.description = "A native Ruby extension wrapping the pdf_oxide Rust crate via Magnus, " \
                     "in the same spirit as pdf_oxide's PyO3 bindings for Python."
  spec.homepage = "https://github.com/metheglin/pdf_dioxide"
  spec.license = "MIT"
  spec.required_ruby_version = ">= 3.2.0"
  spec.metadata["homepage_uri"] = spec.homepage
  spec.metadata["source_code_uri"] = spec.homepage
  # Guard against an accidental `rake release`; set this to the real gem server
  # (e.g. "https://rubygems.org") only when the gem is actually ready to ship.
  spec.metadata["allowed_push_host"] = "https://example.invalid"

  # Uncomment the line below to require MFA for gem pushes.
  # This helps protect your gem from supply chain attacks by ensuring
  # no one can publish a new version without multi-factor authentication.
  # See: https://guides.rubygems.org/mfa-requirement-opt-in/
  # spec.metadata["rubygems_mfa_required"] = "true"

  # Specify which files should be added to the gem when it is released.
  # The `git ls-files -z` loads the files in the RubyGem that have been added into git.
  gemspec = File.basename(__FILE__)
  spec.files = IO.popen(%w[git ls-files -z], chdir: __dir__, err: IO::NULL) do |ls|
    ls.readlines("\x0", chomp: true).reject do |f|
      (f == gemspec) ||
        f.start_with?(*%w[bin/ Gemfile .gitignore test/ tools/ .claude/])
    end
  end
  spec.bindir = "exe"
  spec.executables = spec.files.grep(%r{\Aexe/}) { |f| File.basename(f) }
  spec.require_paths = ["lib"]
  spec.extensions = ["ext/pdf_dioxide/extconf.rb"]

  # Uncomment to register a new dependency of your gem
  # spec.add_dependency "example-gem", "~> 1.0"
  spec.add_dependency "rb_sys", "~> 0.9.128"

  # For more information and examples about making a new gem, check out our
  # guide at: https://guides.rubygems.org/make-your-own-gem/
end
