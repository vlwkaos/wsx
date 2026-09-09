//! Bounded review transport. See docs/worktree-review.md.
use std::{
    io::{self, Read, Write},
    os::unix::{io::AsRawFd, process::CommandExt},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use wsx_core::runtime::{PluginManifest, ReviewRequest, ReviewResponse};

#[derive(Default)]
pub struct Registry(
    std::sync::Mutex<std::collections::HashMap<String, (std::sync::Arc<AtomicBool>, Instant)>>,
);

impl Registry {
    pub fn start(&self, id: &str) -> io::Result<std::sync::Arc<AtomicBool>> {
        if id.is_empty() || id.len() > 512 {
            return Err(io::Error::other("invalid review request ID"));
        }
        let mut jobs = self
            .0
            .lock()
            .map_err(|_| io::Error::other("review registry unavailable"))?;
        jobs.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(10));
        if jobs.len() >= 32 {
            return Err(io::Error::other("review capacity exceeded"));
        }
        if jobs.contains_key(id) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "review cancelled or duplicate",
            ));
        }
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        jobs.insert(id.into(), (flag.clone(), Instant::now()));
        Ok(flag)
    }

    pub fn cancel(&self, id: &str) {
        if id.is_empty() || id.len() > 512 {
            return;
        }
        if let Ok(mut jobs) = self.0.lock() {
            jobs.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(10));
            if let Some((flag, _)) = jobs.get(id) {
                flag.store(true, Ordering::Release);
            } else if jobs.len() < 32 {
                jobs.insert(
                    id.into(),
                    (std::sync::Arc::new(AtomicBool::new(true)), Instant::now()),
                );
            }
        }
    }

    pub fn finish(&self, id: &str) {
        if let Ok(mut jobs) = self.0.lock() {
            jobs.remove(id);
        }
    }
}

const MAX_STDOUT: usize = 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
const MAX_REQUEST: usize = 16 * 1024;

/// The owner supplies cancellation; this operation never holds daemon state locks.
pub fn invoke(
    plugin: &PluginManifest,
    request: &ReviewRequest,
    cancelled: &AtomicBool,
) -> io::Result<ReviewResponse> {
    super::validate(plugin).map_err(io::Error::other)?;
    request
        .validate()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let input = serde_json::to_vec(request).map_err(io::Error::other)?;
    if input.len() > MAX_REQUEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "review request too large",
        ));
    }
    let output = exchange(
        Command::new(&plugin.command[0]).args(&plugin.command[1..]),
        &input,
        cancelled,
        Duration::from_secs(3),
    )?;
    let response: ReviewResponse = serde_json::from_slice(&output)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    response
        .validate_for(request)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(response)
}

fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain(reader: &mut impl Read, output: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut buffer = [0u8; 8192];
    // Bound each poll so a continuously writing plugin cannot starve cancellation.
    for _ in 0..8 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if output.len().saturating_add(n) > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "review output too large",
                    ));
                }
                output.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

fn exchange(
    command: &mut Command,
    input: &[u8],
    cancelled: &AtomicBool,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "review cancelled",
        ));
    }
    let mut child = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pgid = child.id() as libc::pid_t;
    let result = (|| {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        nonblocking(&stdin)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let mut stdin = Some(stdin);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let mut written = 0;
        let start = Instant::now();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "review cancelled",
                ));
            }
            if start.elapsed() >= timeout {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "review timed out"));
            }
            if let Some(writer) = stdin.as_mut() {
                match writer.write(&input[written..]) {
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
                if written == input.len() {
                    // Dropping the owned pipe delivers EOF to the provider.
                    stdin = None;
                }
            }
            let out_done = drain(&mut stdout, &mut output, MAX_STDOUT)?;
            let err_done = drain(&mut stderr, &mut errors, MAX_STDERR)?;
            if out_done && err_done {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err(io::Error::other("review provider exited unsuccessfully"));
                    }
                    return Ok(output);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // Always stop descendants, including ones holding pipes after the parent exits.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    let reaped = child.wait();
    match result {
        Ok(output) => {
            reaped?;
            Ok(output)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_output_pipe_does_not_defeat_deadline() {
        let start = Instant::now();
        let error = exchange(
            Command::new("/bin/sh").args(["-c", "sleep 20 & exit 0"]),
            b"",
            &AtomicBool::new(false),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn stdin_eof_is_delivered_and_output_is_collected() {
        let output = exchange(
            &mut Command::new("/bin/cat"),
            b"request",
            &AtomicBool::new(false),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(output, b"request");
    }

    #[test]
    fn external_git_provider_uses_public_request_and_validated_response() {
        use wsx_core::runtime::*;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work")
            .join(format!("review-contract-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        std::fs::write(root.join("new.txt"), "hello\n").unwrap();
        let plugin = PluginManifest {
            api_version: 1,
            id: "git-review".into(),
            name: "Git review".into(),
            command: vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/wsx-git-review.py")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()],
            events: vec![],
            enabled: true,
            sidecar: None,
            worktree_review: Some(ReviewSpec {
                api_version: 1,
                priority: 0,
                comparisons: vec![ReviewComparison::WorkingAgainstHead],
            }),
        };
        let mut request = ReviewRequest {
            api_version: 1,
            request_id: "list-contract".into(),
            worktree_id: WorktreeId(1),
            worktree_path: root.clone(),
            comparison: ReviewComparison::WorkingAgainstHead,
            limits: ReviewLimits::default(),
            operation: ReviewOperation::ListFiles,
        };
        let result = invoke(&plugin, &request, &AtomicBool::new(false)).unwrap();
        let ReviewResult::Files(files) = result.result else {
            panic!("expected file list");
        };
        assert_eq!(files.files[0].additions, Some(1));
        request.request_id = "diff-contract".into();
        request.operation = ReviewOperation::FileDiff {
            snapshot: files.snapshot,
            file_id: files.files[0].file_id.clone(),
        };
        let result = invoke(&plugin, &request, &AtomicBool::new(false)).unwrap();
        let ReviewResult::Diff(diff) = result.result else {
            panic!("expected diff");
        };
        assert_eq!(diff.hunks[0].lines[0], ReviewLine::Addition("hello".into()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_stops_a_running_process_group() {
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            signal.store(true, Ordering::Release);
        });
        let error = exchange(
            Command::new("/bin/sh").args(["-c", "sleep 20"]),
            b"",
            &cancelled,
            Duration::from_secs(1),
        )
        .unwrap_err();
        trigger.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn cancellation_prevents_spawn() {
        let error = exchange(
            &mut Command::new("/not/a/program"),
            b"",
            &AtomicBool::new(true),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
