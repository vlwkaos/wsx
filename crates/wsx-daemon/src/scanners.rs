//! Listening-port and foreground-job scanners. They run outside the daemon locks.

use super::*;

pub(crate) fn live_runtimes(daemon: &Daemon) -> Option<Vec<(PaneId, Arc<TerminalRuntime>)>> {
    let state = lock(&daemon.state);
    (!state.stopping).then(|| {
        state
            .runtimes
            .iter()
            .filter(|(_, runtime)| !runtime.exited())
            .map(|(pane_id, runtime)| (*pane_id, Arc::clone(runtime)))
            .collect()
    })
}

pub(crate) fn scan_runtimes_current(
    state: &State,
    sampled: &[(PaneId, Arc<TerminalRuntime>)],
) -> bool {
    !state.stopping
        && state
            .runtimes
            .iter()
            .filter(|(_, runtime)| !runtime.exited())
            .count()
            == sampled.len()
        && sampled.iter().all(|(pane_id, runtime)| {
            state
                .runtimes
                .get(pane_id)
                .is_some_and(|current| Arc::ptr_eq(current, runtime))
                && !runtime.exited()
        })
}

// ^ A foreground group is cheap kernel PTY metadata. Keep its live indicator
// independent of the two-second lsof listener scan and never infer provider identity.
pub(crate) fn spawn_foreground_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let Some(runtimes) = live_runtimes(&daemon) else {
            return;
        };
        let foreground_jobs = runtimes
            .iter()
            .filter_map(|(pane_id, runtime)| runtime.has_foreground_job().then_some(*pane_id))
            .collect();
        let mut state = lock(&daemon.state);
        if scan_runtimes_current(&state, &runtimes)
            && apply_foreground_scan(&mut state, foreground_jobs)
        {
            bump(&daemon, &mut state, "pane_activity.changed", 0);
        }
        // ^ This loop's short interval also times rounds that were never taken.
        if !state.handoff_in_progress {
            if let Err(error) = fail_untaken_rounds(&daemon, &mut state) {
                eprintln!("wsxd: cannot end an untaken round: {}", error.message);
            }
            if let Err(error) = end_stale_compactions(&daemon, &mut state) {
                eprintln!("wsxd: cannot end a compaction: {}", error.message);
            }
        }
        drop(state);
        thread::sleep(FOREGROUND_SCAN_INTERVAL);
    })
}

pub(crate) fn spawn_port_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    let mut failing = false;
    thread::spawn(move || loop {
        let Some(runtimes) = live_runtimes(&daemon) else {
            return;
        };
        let process_groups = runtimes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                runtime.process_group_id().map(|group| (*pane_id, group))
            })
            .collect::<HashMap<_, _>>();
        // ^ `lsof` must finish: a scan that failed keeps the last result instead of erasing
        // ports. The terminals come from the kernel, not `ps`: `ps -ax` took 4.6 s to 5.6 s on a
        // loaded host and `ps -p` with one pid took 11 ms; a pane set needs many (task 528).
        let detected = if process_groups.is_empty() {
            Some(HashMap::new())
        } else {
            scan_listening_ports()
                .map(|listeners| {
                    let wanted = process_groups
                        .values()
                        .copied()
                        .chain(listeners.iter().map(|listener| listener.pid));
                    let terminals = scan_process_terminals(wanted);
                    attribute_listening_ports(&process_groups, &listeners, &terminals)
                })
                .map_err(|reason| {
                    // One line for a run of failed scans, not one every two seconds.
                    if !failing {
                        eprintln!(
                            "wsxd: listening-port scan failed, keeping the last result: {reason}"
                        );
                    }
                })
                .ok()
        };
        failing = detected.is_none();
        let mut state = lock(&daemon.state);
        if scan_runtimes_current(&state, &runtimes) && apply_port_scan(&mut state, detected) {
            bump(&daemon, &mut state, "pane_activity.changed", 0);
        }
        drop(state);

        let started = Instant::now();
        while started.elapsed() < PORT_SCAN_INTERVAL {
            if lock(&daemon.state).stopping {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListenerProcess {
    pub(crate) pid: libc::pid_t,
    pub(crate) group: libc::pid_t,
    pub(crate) ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessTerminal {
    pub(crate) group: libc::pid_t,
    pub(crate) tty: String,
}

pub(crate) fn apply_foreground_scan(state: &mut State, foreground_jobs: HashSet<PaneId>) -> bool {
    if state.foreground_jobs == foreground_jobs {
        return false;
    }
    state.foreground_jobs = foreground_jobs;
    true
}

pub(crate) fn apply_port_scan(
    state: &mut State,
    detected: Option<HashMap<PaneId, Vec<u16>>>,
) -> bool {
    let Some(next) = detected else { return false };
    if state.listening_ports == next {
        return false;
    }
    state.listening_ports = next;
    true
}

pub(crate) fn scan_listening_ports() -> Result<Vec<ListenerProcess>, String> {
    let mut command = Command::new("lsof");
    command.args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fpgn"]);
    let options = process::Options {
        timeout: PORT_SCAN_TIMEOUT,
        stdout: process::Bound::Limit(MAX_PORT_SCAN_BYTES as usize),
        stderr: process::Stderr::Null,
        reap: true,
    };
    let run =
        process::run_bounded(&mut command, &options).map_err(|error| format!("lsof: {error}"))?;
    if run.timed_out {
        return Err(format!("lsof took over {} ms", options.timeout.as_millis()));
    }
    // `lsof` exits 1 when nothing listens.
    if run.status.success() || run.status.code() == Some(1) {
        Ok(parse_lsof_ports(&String::from_utf8_lossy(&run.stdout)))
    } else {
        Err(format!("lsof exited with {}", run.status))
    }
}

/// The group and controlling terminal of each of `pids` that lives and has one.
pub(crate) fn scan_process_terminals(
    pids: impl IntoIterator<Item = libc::pid_t>,
) -> HashMap<libc::pid_t, ProcessTerminal> {
    pids.into_iter()
        .filter_map(|pid| Some((pid, process_terminal(pid)?)))
        .collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn process_terminal(pid: libc::pid_t) -> Option<ProcessTerminal> {
    // SAFETY: a zeroed proc_bsdinfo is valid, and proc_pidinfo writes at most its size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    // e_tdev is NODEV (-1) without a controlling terminal.
    (read == size && info.e_tdev != u32::MAX).then(|| ProcessTerminal {
        group: info.pbi_pgid as libc::pid_t,
        tty: info.e_tdev.to_string(),
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn process_terminal(pid: libc::pid_t) -> Option<ProcessTerminal> {
    // ^ /proc/<pid>/stat: "pid (comm) state ppid pgrp session tty_nr ...", comm may hold ")".
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace().skip(2);
    let group = fields.next()?.parse().ok()?;
    let tty = fields.nth(1)?;
    (tty != "0").then(|| ProcessTerminal {
        group,
        tty: tty.to_string(),
    })
}

pub(crate) fn parse_lsof_ports(output: &str) -> Vec<ListenerProcess> {
    let mut current_pid = None;
    let mut processes = HashMap::<libc::pid_t, (Option<libc::pid_t>, Vec<u16>)>::new();
    for line in output.lines() {
        match line.as_bytes().first().copied() {
            Some(b'p') => {
                current_pid = line[1..].parse::<libc::pid_t>().ok();
                if let Some(pid) = current_pid {
                    processes.entry(pid).or_default();
                }
            }
            Some(b'g') => {
                if let Some(pid) = current_pid {
                    processes.entry(pid).or_default().0 = line[1..].parse().ok();
                }
            }
            Some(b'n') => {
                let Some(pid) = current_pid else {
                    continue;
                };
                let endpoint = line[1..].split_whitespace().next().unwrap_or_default();
                let Some(port) = endpoint
                    .rsplit(':')
                    .next()
                    .and_then(|value| value.parse().ok())
                else {
                    continue;
                };
                processes.entry(pid).or_default().1.push(port);
            }
            _ => {}
        }
    }
    let mut listeners = processes
        .into_iter()
        .filter_map(|(pid, (group, mut ports))| {
            ports.sort_unstable();
            ports.dedup();
            Some(ListenerProcess {
                pid,
                group: group?,
                ports,
            })
        })
        .collect::<Vec<_>>();
    listeners.sort_by_key(|listener| listener.pid);
    listeners
}

pub(crate) fn attribute_listening_ports(
    process_groups: &HashMap<PaneId, libc::pid_t>,
    listeners: &[ListenerProcess],
    process_terminals: &HashMap<libc::pid_t, ProcessTerminal>,
) -> HashMap<PaneId, Vec<u16>> {
    let panes_by_group = process_groups
        .iter()
        .map(|(pane_id, group)| (*group, *pane_id))
        .collect::<HashMap<_, _>>();
    let mut panes_by_tty = HashMap::<&str, Option<PaneId>>::new();
    for process in process_terminals.values() {
        let Some(pane_id) = panes_by_group.get(&process.group).copied() else {
            continue;
        };
        panes_by_tty
            .entry(&process.tty)
            .and_modify(|owner| {
                if *owner != Some(pane_id) {
                    *owner = None;
                }
            })
            .or_insert(Some(pane_id));
    }

    let mut ports_by_pane = HashMap::<PaneId, Vec<u16>>::new();
    for listener in listeners {
        let pane_id = panes_by_group.get(&listener.group).copied().or_else(|| {
            let tty = &process_terminals.get(&listener.pid)?.tty;
            panes_by_tty.get(tty.as_str()).copied().flatten()
        });
        if let Some(pane_id) = pane_id {
            ports_by_pane
                .entry(pane_id)
                .or_default()
                .extend(&listener.ports);
        }
    }
    for ports in ports_by_pane.values_mut() {
        ports.sort_unstable();
        ports.dedup();
    }
    ports_by_pane
}
