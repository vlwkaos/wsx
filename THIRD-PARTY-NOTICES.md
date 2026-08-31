# Third-Party Notices

wsx and wsxd incorporate the following audited vendored terminal components.
Release archives include their license files beside the executables.

## libghostty-vt

- Source: Ghostty terminal core, commit `c5a21edfcbc2d5b46540ad91b7980aca31f5f1f3`
- Version metadata: `vendor/libghostty-vt/VERSION`
- License: MIT
- Release license file: `LICENSE-libghostty-vt`

wsx builds the pinned C API behind the private `wsx-terminal` Rust boundary.

## portable-pty

- Source: https://github.com/wezterm/wezterm
- Package: `portable-pty` 0.9.0
- License: MIT
- Release license file: `LICENSE-portable-pty.md`

The vendored package contains the audited PTY backend used by wsxd.

## YAML project configuration

- Package: `yaml_serde` 0.10.7
- Source: https://github.com/yaml/yaml-serde
- License: MIT OR Apache-2.0
- Transitive package: `libyaml-rs` 0.3.0, MIT

These crates parse and write the bounded `wsx.config.yml` project format.

## Reference source

`vendor/herdr` retains the squashed official Herdr v0.8.2 subtree only as
historical implementation reference and provenance. It is excluded from the
Cargo workspace and release archives. Herdr is not executed, linked, installed,
or required by wsx.
