# frozen_string_literal: true

require "bundler/gem_tasks"
require "minitest/test_task"

Minitest::TestTask.create

require "rb_sys/extensiontask"

task build: :compile

GEMSPEC = Gem::Specification.load("pdf_dioxide.gemspec")

RbSys::ExtensionTask.new("pdf_dioxide", GEMSPEC) do |ext|
  ext.lib_dir = "lib/pdf_dioxide"
end

task default: %i[compile test]
