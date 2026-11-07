//! One bounded process runner and one process-group stop for every crate.
//! `run_bounded` runs a command in its own process group, reads its output without letting
//! it grow past a bound, and on a timeout stops the whole group and keeps what was read.
//! `terminate_group` is the one stop: the git runner, panels, the web server's shell lines
//! and compact copies, the terminal, routines, previews and review providers all use it.

use std::{
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Command, ExitStatus, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

/// ^ Once the process has ended, its output streams get this long to close. A process that
/// left the group (a daemon a test run started) can hold them open for hours; then the output
/// read so far is the result, and the reader thread ends with that process.
const STREAM_GRACE: Duration = Duration::from_secs(2);
/// How long `terminate_group` waits after SIGKILL for the group to be gone.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// What a stream may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Unlimited,
    /// More than this many bytes is an error, and the pipe closes.
    Limit(usize),
    /// Read everything; keep the last this many bytes.
    Tail(usize),
}

/// Where the standard error goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stderr {
    /// Its own pipe, read with this bound.
    Own(Bound),
    /// Into the standard output's pipe, in order.
    Merge,
    Null,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub timeout: Duration,
    pub stdout: Bound,
    pub stderr: Stderr,
    /// When the streams stay open after the process ended, stop what it left in its group.
    /// Off, the output read so far is the result and the leftovers run on.
    pub reap: bool,
}

#[derive(Debug)]
pub struct Run {
    /// After a timeout: the status of the killed process.
    pub status: ExitStatus,
    pub timed_out: bool,
    /// From the start to the end of the process, not of the streams.
    pub elapsed: Duration,
    /// All of it, or the part read before a timeout.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Bytes a `Tail` bound dropped from the start of each stream.
    pub stdout_cut: usize,
    pub stderr_cut: usize,
}

/// Run `cmd` in its own process group with its output piped, for at most `options.timeout`.
/// Standard input is the caller's. A timeout is not an error: `timed_out` is set and the
/// output is what was read. A stream over its `Limit` is an error unless the run timed out.
pub fn run_bounded(cmd: &mut Command, options: &Options) -> io::Result<Run> {
    // ! own process group so a signal to the group does not hit the parent
    cmd.process_group(0).stdout(Stdio::piped());
    let mut merged = None;
    match options.stderr {
        Stderr::Own(_) => {
            cmd.stderr(Stdio::piped());
        }
        Stderr::Null => {
            cmd.stderr(Stdio::null());
        }
        Stderr::Merge => {
            let (reader, writer) = io::pipe()?;
            cmd.stdout(writer.try_clone()?).stderr(writer);
            merged = Some(reader);
        }
    }
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    if merged.is_some() {
        // ^ `cmd` holds the pipe's write end until its stdio changes; the reader sees the end
        // of the stream only when no process and no `Command` holds it.
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let group = child.id() as libc::pid_t;
    let out: Option<Box<dyn Read + Send>> = match merged {
        Some(reader) => Some(Box::new(reader)),
        None => child.stdout.take().map(|pipe| Box::new(pipe) as _),
    };
    let mut stdout = Stream::read(out, options.stdout);
    let mut stderr = match options.stderr {
        Stderr::Own(bound) => Stream::read(child.stderr.take(), bound),
        _ => Stream::read(None::<io::Empty>, Bound::Unlimited),
    };

    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= options.timeout {
            // Kill the entire group: git + ssh + credential helpers, ffmpeg + whisper
            signal_group(group, libc::SIGKILL);
            timed_out = true;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let elapsed = started.elapsed();
    let deadline = Instant::now() + STREAM_GRACE;
    let closed = stdout.settle(deadline) & stderr.settle(deadline);
    if !closed && options.reap {
        signal_group(group, libc::SIGKILL);
        let deadline = Instant::now() + STREAM_GRACE;
        stdout.settle(deadline);
        stderr.settle(deadline);
    }
    let (stdout, stdout_cut) = stdout.take(timed_out)?;
    let (stderr, stderr_cut) = stderr.take(timed_out)?;
    Ok(Run {
        status,
        timed_out,
        elapsed,
        stdout,
        stderr,
        stdout_cut,
        stderr_cut,
    })
}

#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    dropped: usize,
}

/// One output stream, read in its own thread into a buffer the caller can take at any time.
struct Stream {
    kept: Arc<Mutex<Kept>>,
    done: Option<mpsc::Receiver<io::Result<()>>>,
    error: Option<io::Error>,
    bound: Bound,
}

impl Stream {
    fn read<R: Read + Send + 'static>(reader: Option<R>, bound: Bound) -> Self {
        let kept = Arc::new(Mutex::new(Kept::default()));
        let done = reader.map(|reader| {
            let (sent, done) = mpsc::channel();
            let shared = kept.clone();
            std::thread::spawn(move || {
                let _ = sent.send(read_bounded(reader, bound, &shared));
            });
            done
        });
        Self {
            kept,
            done,
            error: None,
            bound,
        }
    }

    /// Wait for the stream to close until `deadline`; whether it did.
    fn settle(&mut self, deadline: Instant) -> bool {
        let Some(done) = &self.done else { return true };
        match done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => self.error = result.err(),
            Err(mpsc::RecvTimeoutError::Timeout) => return false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.error = Some(io::Error::other("subprocess output reader panicked"));
            }
        }
        self.done = None;
        true
    }

    /// The bytes read and how many a `Tail` dropped. The read's error is the result unless
    /// `ignore_error`.
    fn take(self, ignore_error: bool) -> io::Result<(Vec<u8>, usize)> {
        if let Some(error) = self.error.filter(|_| !ignore_error) {
            return Err(error);
        }
        let mut kept = std::mem::take(
            &mut *self
                .kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if let Bound::Tail(keep) = self.bound {
            let extra = kept.bytes.len().saturating_sub(keep);
            kept.bytes.drain(..extra);
            kept.dropped += extra;
        }
        Ok((kept.bytes, kept.dropped))
    }
}

fn read_bounded<R: Read>(mut reader: R, bound: Bound, kept: &Mutex<Kept>) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let mut kept = kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        kept.bytes.extend_from_slice(&chunk[..read]);
        match bound {
            Bound::Limit(limit) if kept.bytes.len() > limit => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "subprocess output exceeded byte limit",
                ));
            }
            Bound::Tail(keep) if kept.bytes.len() > 2 * keep => {
                let excess = kept.bytes.len() - keep;
                kept.bytes.drain(..excess);
                kept.dropped += excess;
            }
            _ => {}
        }
    }
}

/// A group worth signalling: not the caller's own and not init's.
fn signalable(pgid: libc::pid_t) -> bool {
    // SAFETY: getpgrp has no arguments and cannot fail.
    pgid > 1 && pgid != unsafe { libc::getpgrp() }
}

/// Send `signal` to the process group `pgid`. A group that is the caller's own, or init's,
/// gets none.
pub fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    if signalable(pgid) {
        // SAFETY: a signal to a process group by number; failure means the group is gone.
        unsafe { libc::kill(-pgid, signal) };
    }
}

/// SIGTERM the group, give it `grace` to end, then SIGKILL what is left and wait up to 2 s
/// for it to go. A zero `grace` sends SIGKILL only. A child of the caller that led the
/// group is reaped while it waits: do not `wait` for it afterwards.
pub fn terminate_group(pgid: libc::pid_t, grace: Duration) {
    if !signalable(pgid) {
        return;
    }
    if !grace.is_zero() {
        signal_group(pgid, libc::SIGTERM);
        if wait_gone(pgid, grace) {
            return;
        }
    }
    signal_group(pgid, libc::SIGKILL);
    if !grace.is_zero() {
        wait_gone(pgid, KILL_WAIT);
    }
}

/// Whether the group has no process left within `limit`.
fn wait_gone(pgid: libc::pid_t, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        // ^ A child of this process is a zombie until reaped, and a zombie keeps its group.
        // SAFETY: waitpid with WNOHANG and no status pointer touches no memory.
        unsafe { libc::waitpid(-pgid, std::ptr::null_mut(), libc::WNOHANG) };
        if !group_exists(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn group_exists(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    let alive = unsafe { libc::kill(-pgid, 0) } == 0;
    alive || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
