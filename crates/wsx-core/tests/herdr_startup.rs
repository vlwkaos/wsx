use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

const HERDR_BIN_ENV: &str = "WSX_HERDR_BIN";
const HERDR_SOCKET_ENV: &str = "HERDR_SOCKET_PATH";

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: &Path) -> Self {
        let previous = env::var_os(name);
        env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => env::set_var(self.name, value),
            None => env::remove_var(self.name),
        }
    }
}

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
    log: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("herdr-startup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create target-local fixture directory");

        let log = root.join("commands.log");
        let marker = root.join("server.running");
        let pid = root.join("server.pid");
        let bin = root.join("herdr");
        let script = format!(
            r#"#!/bin/sh
set -eu
LOG={log}
MARKER={marker}
PID_FILE={pid}
case "${{1-}}" in
  --version)
    [ "$#" -eq 1 ] || exit 2
    printf '%s\n' '0.8.2'
    ;;
  status)
    [ "$#" -eq 3 ] && [ "$2" = server ] && [ "$3" = --json ] || exit 2
    if [ -e "$MARKER" ]; then
      printf '%s\n' '{{"status":"running","running":true,"version":"0.8.2","protocol":20}}'
    else
      printf '%s\n' '{{"status":"not_running","running":false}}'
    fi
    ;;
  server)
    [ "$#" -eq 1 ] || exit 2
    printf '%s\n' server >> "$LOG"
    printf '%s\n' "$$" > "$PID_FILE"
    : > "$MARKER"
    exec sleep 3600
    ;;
  stop)
    [ "$#" -eq 1 ] || exit 2
    if [ -f "$PID_FILE" ]; then
      server_pid=$(cat "$PID_FILE")
      kill -TERM "$server_pid" 2>/dev/null || true
      count=0
      while kill -0 "$server_pid" 2>/dev/null && [ "$count" -lt 200 ]; do
        count=$((count + 1))
        sleep 0.01
      done
      if kill -0 "$server_pid" 2>/dev/null; then
        kill -KILL "$server_pid" 2>/dev/null || true
        count=0
        while kill -0 "$server_pid" 2>/dev/null && [ "$count" -lt 200 ]; do
          count=$((count + 1))
          sleep 0.01
        done
      fi
      kill -0 "$server_pid" 2>/dev/null && exit 1
      rm -f "$MARKER" "$PID_FILE"
    fi
    ;;
  *)
    printf '%s\n' "unsupported fake Herdr command: $*" >&2
    exit 2
    ;;
esac
"#,
            log = shell_quote(&log),
            marker = shell_quote(&marker),
            pid = shell_quote(&pid),
        );
        fs::write(&bin, script).expect("write fake Herdr executable");
        let mut permissions = fs::metadata(&bin).expect("stat fake Herdr").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).expect("make fake Herdr executable");

        Self {
            root,
            bin,
            log,
            marker,
        }
    }

    fn stop(&self) {
        let status = Command::new(&self.bin)
            .arg("stop")
            .status()
            .expect("run fake Herdr cleanup");
        assert!(status.success(), "fake Herdr cleanup failed: {status}");
        assert!(!self.marker.exists(), "fake Herdr server did not stop");
    }

    fn server_commands(&self) -> usize {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "server")
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin).arg("stop").status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[test]
fn ensure_available_starts_stopped_herdr_once() {
    let fixture = Fixture::new();
    let socket = fixture.root.join("herdr.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake Herdr socket");
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .expect("restrict fake Herdr socket");
    let socket_server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept snapshot request");
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .expect("read snapshot request");
            let request: serde_json::Value =
                serde_json::from_str(&request).expect("parse snapshot request");
            assert_eq!(request["method"], "session.snapshot");
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "id": request["id"],
                    "result": {
                        "type": "session_snapshot",
                        "snapshot": {
                            "version": "0.8.2",
                            "protocol": 20,
                            "workspaces": [],
                            "tabs": [],
                            "panes": [],
                            "layouts": [],
                            "agents": []
                        }
                    }
                })
            )
            .expect("write snapshot response");
        }
    });
    let _binary_environment = EnvironmentGuard::set(HERDR_BIN_ENV, &fixture.bin);
    let _socket_environment = EnvironmentGuard::set(HERDR_SOCKET_ENV, &socket);

    let version = wsx_core::herdr::ensure_available()
        .expect("ensure_available should start and await fake Herdr 0.8.2");
    assert_eq!((version.major, version.minor, version.patch), (0, 8, 2));

    let second_version = wsx_core::herdr::ensure_available()
        .expect("ensure_available should accept the already-running fake Herdr");
    assert_eq!(
        (
            second_version.major,
            second_version.minor,
            second_version.patch
        ),
        (0, 8, 2)
    );
    assert_eq!(
        fixture.server_commands(),
        1,
        "Herdr must start exactly once"
    );
    socket_server.join().expect("join fake Herdr socket");

    fixture.stop();
}
