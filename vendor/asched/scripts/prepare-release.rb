#!/usr/bin/env ruby
# frozen_string_literal: true

# ^ [[Rust Workspace Release and Homebrew Publishing]]
VERSION_PATTERN = /\A\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\z/

check_only = ARGV.first == "--check"
ARGV.shift if check_only
version = ARGV.fetch(0) { abort "usage: prepare-release.rb [--check] VERSION" }
abort "invalid release version: #{version}" unless VERSION_PATTERN.match?(version)

root_path = "Cargo.toml"
binary_path = "crates/asched/Cargo.toml"
root = File.read(root_path)
binary = File.read(binary_path)

workspace_pattern = /(\[workspace\.package\].*?^version = ")[^"]+(")/m
dependency_pattern = /(asched-core = \{[^}]*version = "=)[^"]+(")/
abort "workspace version field not found" unless workspace_pattern.match?(root)
abort "asched-core dependency field not found" unless dependency_pattern.match?(binary)

expected_root = root.sub(workspace_pattern, "\\1#{version}\\2")
expected_binary = binary.sub(dependency_pattern, "\\1#{version}\\2")

if check_only
  abort "workspace version does not match #{version}" unless root == expected_root
  abort "asched-core dependency does not match #{version}" unless binary == expected_binary
  exit 0
end

begin
  File.write(root_path, expected_root)
  File.write(binary_path, expected_binary)
rescue StandardError => error
  File.write(root_path, root)
  File.write(binary_path, binary)
  abort "failed to update release versions: #{error.message}"
end

unless system("cargo", "check", "--workspace")
  File.write(root_path, root)
  File.write(binary_path, binary)
  abort "cargo check failed after version update; restored previous versions"
end
