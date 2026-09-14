# frozen_string_literal: true

require "bundler/gem_tasks"
require "minitest/test_task"

Minitest::TestTask.create

require "rb_sys/extensiontask"

task build: :compile

GEMSPEC = Gem::Specification.load("pdf_dioxide.gemspec")

# Cross builds (rb-sys-dock sets RUBY_TARGET) need at least two
# RUBY_CC_VERSIONs: rake-compiler only uses per-version binary directories
# (lib/pdf_dioxide/<major.minor>/) when targeting several Rubies. With a
# single version the cross binary shares lib/pdf_dioxide/pdf_dioxide.so with
# the host build, the two `file` tasks merge, and the host build runs first
# with the wrong linker ("Relocations in generic ELF").
if ENV.key?("RUBY_TARGET") && ENV.fetch("RUBY_CC_VERSION", "").split(":").size < 2
  abort "cross build needs >= 2 Ruby versions, e.g. " \
        "rb-sys-dock -p #{ENV["RUBY_TARGET"]} --ruby-versions 3.4,4.0 --build"
end

RbSys::ExtensionTask.new("pdf_dioxide", GEMSPEC) do |ext|
  ext.lib_dir = "lib/pdf_dioxide"
end

task default: %i[compile test]
