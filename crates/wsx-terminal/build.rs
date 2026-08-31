use std::{env, fs, path::PathBuf, process::Command};

fn zig_target(target: &str) -> &str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        other => panic!("unsupported libghostty-vt target: {other}"),
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest.join("../../vendor/libghostty-vt");
    for path in [
        "build.zig",
        "build.zig.zon",
        "include",
        "pkg",
        "src",
        "VERSION",
    ] {
        println!("cargo:rerun-if-changed={}", source.join(path).display());
    }
    println!("cargo:rerun-if-env-changed=ZIG");
    let target = env::var("TARGET").expect("TARGET");
    let version = fs::read_to_string(source.join("VERSION"))
        .expect("libghostty-vt VERSION")
        .trim()
        .to_owned();
    let status = Command::new(env::var("ZIG").unwrap_or_else(|_| "zig".into()))
        .current_dir(&source)
        .args([
            "build",
            "-Demit-lib-vt",
            "-Doptimize=ReleaseFast",
            "-Dsimd=true",
            &format!("-Dtarget={}", zig_target(&target)),
            &format!("-Dversion-string={version}"),
            "-Demit-xcframework=false",
        ])
        .status()
        .expect("run Zig for libghostty-vt");
    assert!(status.success(), "libghostty-vt build failed: {status}");

    let built = source.join("zig-out/lib/libghostty-vt.a");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let linked = out.join("libwsx_ghostty_vt.a");
    fs::copy(built, &linked).expect("copy static libghostty-vt archive");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=wsx_ghostty_vt");
}
