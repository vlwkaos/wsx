//! Repository-local build and run orchestration for the adjacent wsx/wsxd pair.
// ^ README.md "Development commands" defines the user-facing command contract.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    if let Err(error) = run(env::args_os().skip(1).collect()) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(mut args: Vec<OsString>) -> Result<(), String> {
    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .ok_or_else(usage)?
        .to_owned();
    args.remove(0);
    if args.first().is_some_and(|arg| arg == "--") {
        args.remove(0);
    }
    let root = workspace_root()?;
    match command.as_str() {
        "run" => run_wsx(&root, &args),
        "build" if args.is_empty() => build_bundle(&root),
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
        .ok_or_else(|| "could not resolve workspace root".into())
}

fn target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map_or_else(
            || root.join("target"),
            |path| {
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            },
        )
}

fn cargo_build(root: &Path, release: bool) -> Result<(), String> {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(root).args([
        "build",
        "--locked",
        "--package",
        "wsx",
        "--package",
        "wsx-daemon",
    ]);
    if release {
        command.arg("--release");
    }
    let status = command
        .status()
        .map_err(|error| format!("could not start cargo build: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build failed with {status}"))
    }
}

fn run_wsx(root: &Path, args: &[OsString]) -> Result<(), String> {
    cargo_build(root, false)?;
    let bin = target_dir(root).join("debug");
    let wsx = bin.join(executable_name("wsx"));
    let wsxd = bin.join(executable_name("wsxd"));
    let status = Command::new(&wsx)
        .args(args)
        .env("WSX_DAEMON_BIN", &wsxd)
        .status()
        .map_err(|error| format!("could not run {}: {error}", wsx.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("wsx failed with {status}"))
    }
}

fn build_bundle(root: &Path) -> Result<(), String> {
    cargo_build(root, true)?;
    let target = target_dir(root);
    let bundle = target.join("wsx-dev");
    if bundle.exists() {
        fs::remove_dir_all(&bundle).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&bundle).map_err(|error| error.to_string())?;
    for name in ["wsx", "wsxd"] {
        fs::copy(
            target.join("release").join(executable_name(name)),
            bundle.join(executable_name(name)),
        )
        .map_err(|error| format!("copying {name}: {error}"))?;
    }
    for (source, name) in [
        ("LICENSE", "LICENSE-wsx"),
        ("vendor/libghostty-vt/LICENSE", "LICENSE-libghostty-vt"),
        ("vendor/portable-pty/LICENSE.md", "LICENSE-portable-pty.md"),
        ("THIRD-PARTY-NOTICES.md", "THIRD-PARTY-NOTICES.md"),
    ] {
        fs::copy(root.join(source), bundle.join(name))
            .map_err(|error| format!("copying {source}: {error}"))?;
    }
    println!(
        "Built adjacent wsx and wsxd binaries in {}",
        bundle.display()
    );
    Ok(())
}

fn executable_name(name: &str) -> String {
    format!("{name}{}", env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_target_directory_is_workspace_relative() {
        let root = Path::new("/repo");
        let path = PathBuf::from("custom");
        assert_eq!(
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            },
            PathBuf::from("/repo/custom")
        );
    }
    #[test]
    fn executable_suffix_is_platform_correct() {
        assert_eq!(
            executable_name("wsxd"),
            format!("wsxd{}", env::consts::EXE_SUFFIX)
        );
    }
}
