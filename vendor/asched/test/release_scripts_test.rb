# frozen_string_literal: true

require "digest"
require "fileutils"
require "minitest/autorun"
require "open3"
require "tmpdir"

class ReleaseScriptsTest < Minitest::Test
  PREPARE_RELEASE = File.expand_path("../scripts/prepare-release.rb", __dir__)
  RELEASE_WORKFLOW = File.expand_path("../.github/workflows/release.yml", __dir__)
  RELEASE_NOTES = File.expand_path("../scripts/release-notes.rb", __dir__)
  UPDATE_FORMULA = File.expand_path("../scripts/update-homebrew-formula.rb", __dir__)

  def with_workspace(root_version: "0.1.0", dependency_version: "0.1.0")
    Dir.mktmpdir("asched-release-test") do |root|
      FileUtils.mkdir_p(File.join(root, "crates/asched"))
      File.write(
        File.join(root, "Cargo.toml"),
        <<~TOML
          [workspace]
          members = ["crates/asched"]

          [workspace.package]
          version = "#{root_version}"
          edition = "2021"
        TOML
      )
      File.write(
        File.join(root, "crates/asched/Cargo.toml"),
        <<~TOML
          [package]
          name = "asched"
          version.workspace = true

          [dependencies]
          asched-core = { path = "../asched-core", version = "=#{dependency_version}" }
        TOML
      )
      fake_bin = File.join(root, "fake-bin")
      FileUtils.mkdir_p(fake_bin)
      fake_cargo = File.join(fake_bin, "cargo")
      File.write(
        fake_cargo,
        <<~RUBY
          #!/usr/bin/env ruby
          File.write(ENV.fetch("CARGO_LOG"), ARGV.join(" "))
          exit Integer(ENV.fetch("CARGO_EXIT", "0"))
        RUBY
      )
      FileUtils.chmod(0o755, fake_cargo)
      yield root, fake_bin
    end
  end

  def run_prepare(root, fake_bin, *arguments, cargo_exit: 0)
    cargo_log = File.join(root, "cargo.log")
    environment = {
      "PATH" => "#{fake_bin}:#{ENV.fetch("PATH")}",
      "CARGO_LOG" => cargo_log,
      "CARGO_EXIT" => cargo_exit.to_s
    }
    output, error, status = Open3.capture3(
      environment,
      "ruby",
      PREPARE_RELEASE,
      *arguments,
      chdir: root
    )
    [output, error, status, cargo_log]
  end

  def manifests(root)
    [
      File.read(File.join(root, "Cargo.toml")),
      File.read(File.join(root, "crates/asched/Cargo.toml"))
    ]
  end

  def run_formula(root, *arguments)
    Open3.capture3("ruby", UPDATE_FORMULA, *arguments, chdir: root)
  end

  def run_release_notes(root, *arguments)
    Open3.capture3("ruby", RELEASE_NOTES, *arguments, chdir: root)
  end

  def test_given_valid_version_when_prepared_then_both_versions_change_and_cargo_check_gates_success
    with_workspace do |root, fake_bin|
      _output, error, status, cargo_log = run_prepare(root, fake_bin, "1.2.3")
      workspace, binary = manifests(root)

      assert(
        status.success? &&
          error.empty? &&
          workspace.include?("[workspace.package]\nversion = \"1.2.3\"") &&
          binary.include?('asched-core = { path = "../asched-core", version = "=1.2.3" }') &&
          File.read(cargo_log) == "check --workspace"
      )
    end
  end

  def test_given_failing_cargo_check_when_prepared_then_command_fails_without_manifest_writes()
    with_workspace do |root, fake_bin|
      before = manifests(root)
      _output, error, status, cargo_log =
        run_prepare(root, fake_bin, "1.2.3", cargo_exit: 1)

      assert(
        !status.success? &&
          error.include?("cargo check failed after version update") &&
          File.read(cargo_log) == "check --workspace" &&
          manifests(root) == before
      )
    end
  end

  def test_given_exact_manifest_versions_when_checked_then_check_succeeds_without_cargo()
    with_workspace(root_version: "1.2.3", dependency_version: "1.2.3") do |root, fake_bin|
      before = manifests(root)
      _output, error, status, cargo_log =
        run_prepare(root, fake_bin, "--check", "1.2.3")

      assert(
        status.success? &&
          error.empty? &&
          !File.exist?(cargo_log) &&
          manifests(root) == before
      )
    end
  end

  def test_given_mismatched_dependency_version_when_checked_then_check_fails_without_writes()
    with_workspace(root_version: "1.2.3", dependency_version: "1.2.2") do |root, fake_bin|
      before = manifests(root)
      _output, error, status, _cargo_log =
        run_prepare(root, fake_bin, "--check", "1.2.3")

      assert(
        !status.success? &&
          error.include?("asched-core dependency does not match 1.2.3") &&
          manifests(root) == before
      )
    end
  end

  def test_given_malformed_versions_when_prepared_then_each_fails_without_writes()
    with_workspace do |root, fake_bin|
      before = manifests(root)
      results = ["1.2", "v1.2.3", "1.2.3+metadata"].map do |version|
        run_prepare(root, fake_bin, version)
      end

      assert(
        results.all? { |_output, error, status, _log| !status.success? && error.include?("invalid release version") } &&
          manifests(root) == before
      )
    end
  end

  def test_given_one_missing_or_malformed_version_target_when_prepared_then_no_manifest_is_written_or_cargo_run
    results = %w[missing-binary malformed-root malformed-binary].map do |scenario|
      with_workspace do |root, fake_bin|
        root_path = File.join(root, "Cargo.toml")
        binary_path = File.join(root, "crates/asched/Cargo.toml")
        case scenario
        when "missing-binary"
          FileUtils.rm_f(binary_path)
        when "malformed-root"
          File.write(root_path, "[workspace]\nmembers = []\n")
        when "malformed-binary"
          File.write(binary_path, "[package]\nname = \"asched\"\n")
        end
        before_root = File.read(root_path)
        before_binary = File.exist?(binary_path) ? File.read(binary_path) : nil

        _output, _error, status, cargo_log = run_prepare(root, fake_bin, "1.2.3")

        [
          status.success?,
          File.read(root_path),
          File.exist?(binary_path) ? File.read(binary_path) : nil,
          before_root,
          before_binary,
          File.exist?(cargo_log)
        ]
      end
    end

    assert(
      results.all? do |success, root_after, binary_after, root_before, binary_before, cargo_ran|
        !success && root_after == root_before && binary_after == binary_before && !cargo_ran
      end
    )
  end

  def test_given_release_paths_with_spaces_when_scripts_run_then_both_succeed
    prepare = Dir.mktmpdir("asched release workspace ") do |root|
      FileUtils.mkdir_p(File.join(root, "crates/asched"))
      File.write(
        File.join(root, "Cargo.toml"),
        "[workspace]\nmembers = [\"crates/asched\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n"
      )
      File.write(
        File.join(root, "crates/asched/Cargo.toml"),
        "[package]\nname = \"asched\"\n\n[dependencies]\nasched-core = { path = \"../asched-core\", version = \"=0.1.0\" }\n"
      )
      fake_bin = File.join(root, "fake bin")
      FileUtils.mkdir_p(fake_bin)
      File.write(
        File.join(fake_bin, "cargo"),
        "#!/usr/bin/env ruby\nFile.write(ENV.fetch(\"CARGO_LOG\"), ARGV.join(\" \"))\n"
      )
      FileUtils.chmod(0o755, File.join(fake_bin, "cargo"))
      _output, error, status, cargo_log = run_prepare(root, fake_bin, "1.2.3")
      [status.success?, error, File.read(cargo_log)]
    end
    formula = Dir.mktmpdir("asched formula paths ") do |root|
      archive = File.join(root, "asched-1.2.3-darwin-universal.tar.gz")
      tap = File.join(root, "tap with spaces")
      File.write(archive, "archive")
      FileUtils.mkdir_p(tap)
      output, error, status = run_formula(root, "1.2.3", archive, tap)
      path = File.join(tap, "Formula/asched.rb")
      [status.success?, error, output.strip, path, File.exist?(path)]
    end

    assert(
      prepare == [true, "", "check --workspace"] &&
        formula[0..1] == [true, ""] &&
        formula[2] == formula[3] &&
        formula[4]
    )
  end

  def test_given_invalid_or_missing_formula_inputs_when_updated_then_each_fails_without_formula()
    Dir.mktmpdir("asched-formula-input-test") do |root|
      archive = File.join(root, "asched.tar.gz")
      tap = File.join(root, "tap")
      File.write(archive, "archive")
      FileUtils.mkdir_p(tap)
      results = [
        run_formula(root),
        run_formula(root, "1.2.3-beta", archive, tap),
        run_formula(root, "1.2.3", File.join(root, "missing.tar.gz"), tap),
        run_formula(root, "1.2.3", archive, File.join(root, "missing-tap"))
      ]

      assert(
        results.all? { |_output, _error, status| !status.success? } &&
          !File.exist?(File.join(tap, "Formula/asched.rb"))
      )
    end
  end

  def test_given_mismatched_archive_version_when_formula_is_updated_then_it_is_rejected_without_output
    Dir.mktmpdir("asched-formula-version-test") do |root|
      archive = File.join(root, "asched-9.9.9-darwin-universal.tar.gz")
      tap = File.join(root, "tap")
      File.write(archive, "archive")
      FileUtils.mkdir_p(tap)

      _output, error, status = run_formula(root, "1.2.3", archive, tap)

      assert(
        !status.success? &&
          error.include?("release tarball name must be asched-1.2.3-darwin-universal.tar.gz") &&
          !File.exist?(File.join(tap, "Formula/asched.rb"))
      )
    end
  end

  def test_given_stable_version_and_archive_when_formula_is_updated_then_release_contract_is_written
    Dir.mktmpdir("asched-formula-test") do |root|
      version = "1.2.3"
      archive = File.join(root, "asched-1.2.3-darwin-universal.tar.gz")
      tap = File.join(root, "tap")
      File.write(archive, "deterministic release archive")
      FileUtils.mkdir_p(tap)
      output, error, status = run_formula(root, version, archive, tap)
      formula_path = File.join(tap, "Formula/asched.rb")
      formula = File.exist?(formula_path) ? File.read(formula_path) : ""
      sha256 = Digest::SHA256.file(archive).hexdigest

      assert(
        status.success? &&
          error.empty? &&
          output.strip == formula_path &&
          formula.include?("https://github.com/vlwkaos/asched/releases/download/v#{version}/#{File.basename(archive)}") &&
          formula.include?("sha256 \"#{sha256}\"") &&
          formula.include?('license "MIT"') &&
          formula.include?('bin.install "asched"') &&
          formula.include?('assert_match "asched", shell_output("#{bin}/asched --help")')
      )
    end
  end

  def test_given_versioned_changelog_when_release_notes_are_generated_then_only_that_section_is_written
    Dir.mktmpdir("asched-release-notes-test") do |root|
      changelog = File.join(root, "CHANGELOG.md")
      output_path = File.join(root, "dist/release-notes.md")
      File.write(
        changelog,
        <<~MARKDOWN
          # Changelog

          ## Unreleased

          - Future work.

          ## [1.2.3] - 2026-08-05

          ### Features

          - Added the release feature.

          ## [1.2.2] - 2026-07-24

          - Previous release.
        MARKDOWN
      )

      output, error, status = run_release_notes(root, "1.2.3", changelog, output_path)
      notes = File.exist?(output_path) ? File.read(output_path) : ""

      assert(
        status.success? &&
          error.empty? &&
          output.strip == output_path &&
          notes.start_with?("# v1.2.3\n") &&
          notes.include?("Added the release feature") &&
          !notes.include?("Future work") &&
          !notes.include?("Previous release")
      )
    end
  end

  def test_given_missing_release_section_when_release_notes_are_generated_then_no_file_is_written
    Dir.mktmpdir("asched-missing-release-notes-test") do |root|
      changelog = File.join(root, "CHANGELOG.md")
      output_path = File.join(root, "release-notes.md")
      File.write(changelog, "# Changelog\n\n## Unreleased\n")

      _output, error, status = run_release_notes(root, "1.2.3", changelog, output_path)

      assert(
        !status.success? &&
          error.include?("changelog section not found for 1.2.3") &&
          !File.exist?(output_path)
      )
    end
  end

  def test_given_release_workflow_when_release_is_created_or_rerun_then_changelog_notes_are_applied
    workflow = File.read(RELEASE_WORKFLOW)

    assert(
      workflow.include?('ruby scripts/release-notes.rb "$VERSION" CHANGELOG.md') &&
        workflow.include?('gh release create "v${VERSION}"') &&
        workflow.include?('gh release edit "v${VERSION}"') &&
        workflow.scan('--notes-file "$NOTES"').length == 2 &&
        !workflow.include?("--generate-notes")
    )
  end

  def test_given_tap_push_when_workflow_runs_then_git_auth_is_configured_before_clone
    workflow = File.read(RELEASE_WORKFLOW)
    auth = workflow.index("gh auth setup-git")
    clone = workflow.index("gh repo clone vlwkaos/homebrew-tap")

    assert(auth && clone && auth < clone)
  end
end
