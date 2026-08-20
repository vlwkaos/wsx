#!/usr/bin/env ruby
# frozen_string_literal: true

require "digest"
require "fileutils"

version, tarball, tap = ARGV
abort "usage: update-homebrew-formula.rb VERSION TARBALL TAP" unless version && tarball && tap
abort "invalid release version: #{version}" unless /\A\d+\.\d+\.\d+\z/.match?(version)
abort "release tarball not found: #{tarball}" unless File.file?(tarball)
abort "Homebrew tap not found: #{tap}" unless File.directory?(tap)

name = File.basename(tarball)
expected_name = "asched-#{version}-darwin-universal.tar.gz"
abort "release tarball name must be #{expected_name}" unless name == expected_name

sha256 = Digest::SHA256.file(tarball).hexdigest
formula = <<~FORMULA
  class Asched < Formula
    desc "Minimal TUI and agent-friendly CLI for local scheduled routines"
    homepage "https://github.com/vlwkaos/asched"
    url "https://github.com/vlwkaos/asched/releases/download/v#{version}/#{name}"
    version "#{version}"
    sha256 "#{sha256}"
    license "MIT"

    def install
      bin.install "asched"
    end

    test do
      assert_match "asched", shell_output("\#{bin}/asched --help")
    end
  end
FORMULA

path = File.join(tap, "Formula", "asched.rb")
FileUtils.mkdir_p(File.dirname(path))
File.write(path, formula)
puts path
