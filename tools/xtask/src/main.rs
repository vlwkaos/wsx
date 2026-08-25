//! Repository-local run and build orchestration for the wsx + Herdr companion pair.
// ^ See README.md "Development commands" for the user-facing command contract.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

const HERDR_VERSION: &str = "0.8.2";
const HERDR_BASE_URL: &str = "https://github.com/herdrdev/herdr/releases/download/v0.8.2";
// ^ GitHub's immutable v0.8.2 release API publishes the size and SHA-256 values below.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Asset {
    name: &'static str,
    sha256: &'static str,
    size: u64,
}

fn main() {
    if let Err(error) = run(env::args_os().skip(1).collect()) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(mut args: Vec<OsString>) -> Result<(), String> {
    let Some(command) = args.first().and_then(|arg| arg.to_str()).map(str::to_owned) else {
        return Err(usage());
    };
    args.remove(0);
    if args.first().is_some_and(|arg| arg == "--") {
        args.remove(0);
    }

    let root = workspace_root()?;
    match command.as_str() {
        "run" => run_wsx(&root, &args),
        "build" if args.is_empty() => build_bundle(&root),
        "build" => Err(format!(
            "cargo xtask build accepts no arguments\n\n{}",
            usage()
        )),
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage:\n  cargo xtask run [-- <wsx-args>...]\n  cargo xtask build".into()
}

fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not resolve the workspace root".into())
}

fn target_dir(root: &Path) -> PathBuf {
    target_dir_from(root, env::var_os("CARGO_TARGET_DIR"))
}

fn target_dir_from(root: &Path, override_path: Option<OsString>) -> PathBuf {
    match override_path {
        Some(path) if !path.is_empty() => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        }
        _ => root.join("target"),
    }
}

fn run_wsx(root: &Path, args: &[OsString]) -> Result<(), String> {
    cargo_build(root, false)?;
    let target = target_dir(root);
    let wsx = target.join("debug").join(executable_name("wsx"));
    let herdr = resolve_run_herdr(&target)?;
    println!("Running {} with {}", wsx.display(), herdr.display());
    let status = Command::new(&wsx)
        .args(args)
        .env("WSX_HERDR_BIN", &herdr)
        .status()
        .map_err(|error| format!("could not run {}: {error}", wsx.display()))?;
    if status.success() {
        Ok(())
    } else {
        exit_with_status(status)
    }
}

fn build_bundle(root: &Path) -> Result<(), String> {
    cargo_build(root, true)?;
    let target = target_dir(root);
    let herdr = pinned_herdr(&target)?;
    let bundle = target.join("wsx-dev");
    if bundle.exists() {
        fs::remove_dir_all(&bundle)
            .map_err(|error| format!("could not clear {}: {error}", bundle.display()))?;
    }
    fs::create_dir_all(&bundle)
        .map_err(|error| format!("could not create {}: {error}", bundle.display()))?;

    copy_executable(
        &target.join("release").join(executable_name("wsx")),
        &bundle.join(executable_name("wsx")),
    )?;
    copy_executable(&herdr, &bundle.join(executable_name("herdr")))?;
    copy_file(&root.join("LICENSE"), &bundle.join("LICENSE-wsx"))?;
    copy_file(
        &root.join("vendor/herdr/LICENSE"),
        &bundle.join("LICENSE-herdr"),
    )?;
    copy_file(
        &root.join("vendor/herdr/vendor/libghostty-vt/LICENSE"),
        &bundle.join("LICENSE-libghostty-vt"),
    )?;
    copy_file(
        &root.join("vendor/herdr/vendor/portable-pty/LICENSE.md"),
        &bundle.join("LICENSE-portable-pty.md"),
    )?;
    copy_file(
        &root.join("THIRD-PARTY-NOTICES.md"),
        &bundle.join("THIRD-PARTY-NOTICES.md"),
    )?;
    println!(
        "Built adjacent wsx and Herdr binaries in {}",
        bundle.display()
    );
    Ok(())
}

fn cargo_build(root: &Path, release: bool) -> Result<(), String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args(["build", "--locked", "--package", "wsx"]);
    if release {
        command.arg("--release");
    }
    let status = command
        .status()
        .map_err(|error| format!("could not start cargo build: {error}"))?;
    require_success(status, "cargo build")
}

fn resolve_run_herdr(target: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os("WSX_HERDR_BIN").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(explicit);
        validate_compatible_herdr(&path)?;
        return Ok(path);
    }

    for candidate in [
        target.join("debug").join(executable_name("herdr")),
        target.join("release").join(executable_name("herdr")),
    ] {
        if is_executable_file(&candidate) && validate_compatible_herdr(&candidate).is_ok() {
            return Ok(candidate);
        }
    }

    for candidate in find_on_path(executable_name("herdr")) {
        match validate_compatible_herdr(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) => eprintln!("Ignoring incompatible Herdr on PATH: {error}"),
        }
    }

    pinned_herdr(target)
}

fn pinned_herdr(target: &Path) -> Result<PathBuf, String> {
    let asset = host_asset()?;
    let cache_dir = target
        .join("wsx-tools/herdr")
        .join(format!("v{HERDR_VERSION}"));
    let cached = cache_dir.join(executable_name("herdr"));
    fs::create_dir_all(&cache_dir)
        .map_err(|error| format!("could not create {}: {error}", cache_dir.display()))?;
    make_private_directory(&cache_dir)?;
    clean_stale_downloads(&cache_dir)?;
    if cached.is_file() && verify_file(&cached, asset).is_ok() {
        validate_exact_herdr(&cached)?;
        return Ok(cached);
    }

    if cached.exists() {
        fs::remove_file(&cached).map_err(|error| {
            format!(
                "could not remove invalid cache {}: {error}",
                cached.display()
            )
        })?;
    }
    let temporary = cache_dir.join(format!(".download-{}", std::process::id()));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
    let url = format!("{HERDR_BASE_URL}/{}", asset.name);
    println!("Downloading pinned Herdr v{HERDR_VERSION} from {url}");
    let status = Command::new(system_curl())
        .args([
            "--disable",
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "15",
            "--max-time",
            "300",
            "--silent",
            "--show-error",
            "--max-filesize",
        ])
        .arg(asset.size.to_string())
        .arg("--output")
        .arg(&temporary)
        .arg(&url)
        .status()
        .map_err(|error| format!("could not start curl: {error}"))?;
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        return Err(format!("curl failed while downloading {url}"));
    }
    let install = (|| {
        verify_file(&temporary, asset)?;
        make_executable(&temporary)?;
        validate_exact_herdr(&temporary)?;
        fs::rename(&temporary, &cached).map_err(|error| {
            format!(
                "could not move verified Herdr into {}: {error}",
                cached.display()
            )
        })
    })();
    if let Err(error) = install {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(cached)
}

fn host_asset() -> Result<Asset, String> {
    asset_for(env::consts::OS, env::consts::ARCH).ok_or_else(|| {
        format!(
            "unsupported host {}-{}; install Herdr v{HERDR_VERSION} and set WSX_HERDR_BIN",
            env::consts::OS,
            env::consts::ARCH
        )
    })
}

fn asset_for(os: &str, arch: &str) -> Option<Asset> {
    match (os, arch) {
        ("macos", "aarch64") => Some(Asset {
            name: "herdr-macos-aarch64",
            sha256: "a5d4f4d504d8b309c91f811050559300faba31258425f53c50852fc96f6ae574",
            size: 18_969_952,
        }),
        ("macos", "x86_64") => Some(Asset {
            name: "herdr-macos-x86_64",
            sha256: "ab50262c8190cd7aa9056d249d255c08c328c3e8716de9cfa29db4f131b8e2c1",
            size: 20_551_504,
        }),
        ("linux", "aarch64") => Some(Asset {
            name: "herdr-linux-aarch64",
            sha256: "f55610658e1c2e0d2aaef730b4b2ab885f7f8ba00285ab372bfb14f2e3d5b40d",
            size: 20_744_664,
        }),
        ("linux", "x86_64") => Some(Asset {
            name: "herdr-linux-x86_64",
            sha256: "976150a14d490c94b243ea2e1a7eb2dfb67f12e36b182db90936f6728e6aecf4",
            size: 22_733_040,
        }),
        _ => None,
    }
}

fn verify_file(path: &Path, asset: Asset) -> Result<(), String> {
    let size = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
        .len();
    if size != asset.size {
        return Err(format!(
            "{} has size {size}, expected {}",
            path.display(),
            asset.size
        ));
    }
    let digest = sha256(path)?;
    if !digest.eq_ignore_ascii_case(asset.sha256) {
        return Err(format!(
            "{} failed SHA-256 verification: got {digest}, expected {}",
            path.display(),
            asset.sha256
        ));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String, String> {
    let (program, arguments): (&str, &[&str]) = match env::consts::OS {
        "macos" => ("/usr/bin/shasum", &["-a", "256"]),
        "linux" => ("/usr/bin/sha256sum", &[]),
        other => return Err(format!("SHA-256 verification is unsupported on {other}")),
    };
    let output = Command::new(program)
        .args(arguments)
        .arg(path)
        .output()
        .map_err(|error| format!("could not start {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} failed with {}", output.status));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("{program} returned non-UTF-8 output"))?;
    let digest = stdout
        .split_whitespace()
        .next()
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| format!("{program} returned an invalid SHA-256 digest"))?;
    Ok(digest.to_ascii_lowercase())
}

fn validate_compatible_herdr(path: &Path) -> Result<(), String> {
    let version = herdr_version(path)?;
    if version < (0, 8, 2) {
        return Err(format!(
            "{} is Herdr {}.{}.{}; require 0.8.2+",
            path.display(),
            version.0,
            version.1,
            version.2
        ));
    }
    Ok(())
}

fn validate_exact_herdr(path: &Path) -> Result<(), String> {
    let version = herdr_version(path)?;
    if version != (0, 8, 2) {
        return Err(format!(
            "verified asset reported {}.{}.{}, expected {HERDR_VERSION}",
            version.0, version.1, version.2
        ));
    }
    Ok(())
}

fn herdr_version(path: &Path) -> Result<(u64, u64, u64), String> {
    let output = output_with_timeout(path, "--version", Duration::from_secs(5))?;
    if !output.status.success() {
        return Err(format!("{} --version failed", path.display()));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("{} --version returned non-UTF-8 output", path.display()))?;
    parse_version(&stdout).ok_or_else(|| format!("could not parse Herdr version from {stdout:?}"))
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    for word in text.split(|character: char| character.is_whitespace() || character == ',') {
        let parts = word.trim_start_matches('v').split('.').collect::<Vec<_>>();
        if parts.len() != 3
            || parts
                .iter()
                .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        if let (Ok(major), Ok(minor), Ok(patch)) =
            (parts[0].parse(), parts[1].parse(), parts[2].parse())
        {
            return Some((major, minor, patch));
        }
    }
    None
}

fn find_on_path(name: OsString) -> Vec<PathBuf> {
    let Some(paths) = env::var_os("PATH") else {
        return Vec::new();
    };
    find_in_directories(env::split_paths(&paths), &name)
}

fn find_in_directories(
    directories: impl IntoIterator<Item = PathBuf>,
    name: &OsString,
) -> Vec<PathBuf> {
    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .filter(|candidate| is_executable_file(candidate))
        .collect()
}

fn executable_name(name: &str) -> OsString {
    OsString::from(format!("{name}{}", env::consts::EXE_SUFFIX))
}

fn system_curl() -> &'static str {
    // ^ Absolute system tools keep PATH-controlled programs outside the verification boundary.
    "/usr/bin/curl"
}

fn clean_stale_downloads(directory: &Path) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect an entry in {}: {error}",
                directory.display()
            )
        })?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".download-"))
        {
            let file_type = entry.file_type().map_err(|error| {
                format!("could not inspect {}: {error}", entry.path().display())
            })?;
            if file_type.is_file() || file_type.is_symlink() {
                fs::remove_file(entry.path()).map_err(|error| {
                    format!("could not remove stale {}: {error}", entry.path().display())
                })?;
            }
        }
    }
    Ok(())
}

fn output_with_timeout(path: &Path, argument: &str, timeout: Duration) -> Result<Output, String> {
    let mut child = Command::new(path)
        .arg(argument)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {} {argument}: {error}", path.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|error| format!("could not collect {} output: {error}", path.display()));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} {argument} timed out", path.display()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn copy_executable(source: &Path, destination: &Path) -> Result<(), String> {
    copy_file(source, destination)?;
    make_executable(destination)
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), String> {
    fs::copy(source, destination).map_err(|error| {
        format!(
            "could not copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn make_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not restrict {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_private_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("could not chmod {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn exit_with_status(status: ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::exit(
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
        );
    }
    #[cfg(not(unix))]
    std::process::exit(status.code().unwrap_or(1));
}

fn require_success(status: ExitStatus, operation: &str) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{operation} failed with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_pinned_for_supported_hosts() {
        assert_eq!(
            asset_for("macos", "aarch64").unwrap().sha256,
            "a5d4f4d504d8b309c91f811050559300faba31258425f53c50852fc96f6ae574"
        );
        assert_eq!(
            asset_for("linux", "x86_64").unwrap().name,
            "herdr-linux-x86_64"
        );
        assert_eq!(asset_for("windows", "x86_64"), None);
    }

    #[test]
    fn versions_parse_and_compare_without_accepting_incomplete_values() {
        assert_eq!(parse_version("herdr 0.8.2"), Some((0, 8, 2)));
        assert_eq!(parse_version("herdr v0.9.1-preview"), None);
        assert_eq!(parse_version("herdr 0.8.2.9"), None);
        assert_eq!(parse_version("herdr 0.8"), None);
        assert_eq!(parse_version("unknown"), None);
    }

    #[cfg(unix)]
    #[test]
    fn path_search_skips_non_executable_candidates() {
        use std::os::unix::fs::PermissionsExt;
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/tests")
            .join(format!("xtask-path-{}", std::process::id()));
        let first = directory.join("first");
        let second = directory.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("herdr"), b"not executable").unwrap();
        fs::write(second.join("herdr"), b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(second.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_in_directories(vec![first, second.clone()], &"herdr".into()),
            vec![second.join("herdr")]
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_is_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/tests")
            .join(format!("xtask-timeout-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("herdr");
        fs::write(&path, b"#!/bin/sh\nexec sleep 10\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        assert!(output_with_timeout(&path, "--version", Duration::from_millis(20)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_download_cleanup_removes_partial_files() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/tests")
            .join(format!("xtask-stale-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(".download-old"), b"partial").unwrap();
        fs::write(directory.join("herdr"), b"cached").unwrap();
        clean_stale_downloads(&directory).unwrap();
        assert!(!directory.join(".download-old").exists());
        assert!(directory.join("herdr").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn checksum_verification_accepts_exact_content_and_rejects_mismatch() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/tests")
            .join(format!("xtask-checksum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("fixture");
        fs::write(&path, b"hello").unwrap();
        let exact = Asset {
            name: "fixture",
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            size: 5,
        };
        assert!(verify_file(&path, exact).is_ok());
        assert!(verify_file(
            &path,
            Asset {
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                ..exact
            }
        )
        .is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn target_directory_honors_relative_and_absolute_overrides() {
        let root = Path::new("/repo");
        assert_eq!(
            target_dir_from(root, Some("custom-target".into())),
            Path::new("/repo/custom-target")
        );
        assert_eq!(
            target_dir_from(root, Some("/external-target".into())),
            Path::new("/external-target")
        );
        assert_eq!(target_dir_from(root, None), Path::new("/repo/target"));
    }
}
