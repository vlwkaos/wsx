#!/usr/bin/env ruby
# frozen_string_literal: true

# ^ [[Rust Workspace Release and Homebrew Publishing]]
require "fileutils"

VERSION_PATTERN = /\A\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\z/

version, changelog_path, output_path = ARGV
abort "usage: release-notes.rb VERSION CHANGELOG OUTPUT" unless version && changelog_path && output_path
abort "invalid release version: #{version}" unless VERSION_PATTERN.match?(version)
abort "changelog not found: #{changelog_path}" unless File.file?(changelog_path)

lines = File.readlines(changelog_path)
heading = /^## \[#{Regexp.escape(version)}\] - \d{4}-\d{2}-\d{2}\s*$/
start = lines.index { |line| heading.match?(line) }
abort "changelog section not found for #{version}" unless start

finish = lines.each_index.find { |index| index > start && lines[index].start_with?("## ") } || lines.length
body = lines[(start + 1)...finish].join.strip
abort "changelog section is empty for #{version}" if body.empty?

FileUtils.mkdir_p(File.dirname(output_path))
File.write(output_path, "# v#{version}\n\n#{body}\n")
puts output_path
