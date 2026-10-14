//! The git commit the binary was built from, shown in the web client's Settings
//! (docs/web-client.md "Build"). A build without git, such as a release archive, has none.
use std::process::Command;

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

fn main() {
    // ^ Rebuild when HEAD moves to another commit, not on every source change.
    for name in ["HEAD", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &reference,
        ]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) {
        println!("cargo:rustc-env=WSX_BUILD_COMMIT={commit}");
    }
}
