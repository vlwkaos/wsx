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
// independent of the two-second lsof/ps listener scan and never infer provider identity.
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
        // ^ Both commands must finish: with the `ps` tty map missing, a listener that
        // left its pane's process group is missed and the scan would erase its port.
        // `ps` names only the pane leaders and the listeners: `ps -ax` took 5.6 s on a
        // host at load 50, `lsof` 136 ms (task 528).
        let detected = if process_groups.is_empty() {
            Some(HashMap::new())
        } else {
            scan_listening_ports()
                .and_then(|listeners| {
                    let wanted = process_groups
                        .values()
                        .copied()
                        .chain(listeners.iter().map(|listener| listener.pid))
                        .collect::<HashSet<_>>();
                    let terminals = scan_process_terminals(&wanted)?;
                    Ok(attribute_listening_ports(
                        &process_groups,
                        &listeners,
                        &terminals,
                    ))
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
    state.listener_scan_complete = true;
    if state.listening_ports == next {
        return false;
    }
    state.listening_ports = next;
    true
}

pub(crate) fn scan_listening_ports() -> Result<Vec<ListenerProcess>, String> {
    let mut command = Command::new("lsof");
    command.args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fpgn"]);
    let bytes = bounded_command_output("lsof", command, true)?;
    Ok(parse_lsof_ports(&String::from_utf8_lossy(&bytes)))
}

/// The group and tty of each of `pids`. `ps` exits 1 when none of them lives (a listener that
/// ended since `lsof`); its other failures still fail the scan.
pub(crate) fn scan_process_terminals(
    pids: &HashSet<libc::pid_t>,
) -> Result<HashMap<libc::pid_t, ProcessTerminal>, String> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = pids
        .iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut command = Command::new("ps");
    command.args(["-o", "pid=,pgid=,tty=", "-p", &list]);
    let bytes = bounded_command_output("ps", command, true)?;
    Ok(parse_process_terminals(&String::from_utf8_lossy(&bytes)))
}

/// A scan command's output, or why there is none. `lsof` exits 1 when nothing listens.
pub(crate) fn bounded_command_output(
    name: &str,
    mut command: Command,
    accept_code_one: bool,
) -> Result<Vec<u8>, String> {
    let options = process::Options {
        timeout: PORT_SCAN_TIMEOUT,
        stdout: process::Bound::Limit(MAX_PORT_SCAN_BYTES as usize),
        stderr: process::Stderr::Null,
        reap: true,
    };
    let run =
        process::run_bounded(&mut command, &options).map_err(|error| format!("{name}: {error}"))?;
    if run.timed_out {
        return Err(format!(
            "{name} took over {} ms",
            options.timeout.as_millis()
        ));
    }
    if run.status.success() || (accept_code_one && run.status.code() == Some(1)) {
        Ok(run.stdout)
    } else {
        Err(format!("{name} exited with {}", run.status))
    }
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

pub(crate) fn parse_process_terminals(output: &str) -> HashMap<libc::pid_t, ProcessTerminal> {
    let mut processes = HashMap::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(group), Some(tty), None) = (
            fields.next().and_then(|value| value.parse().ok()),
            fields.next().and_then(|value| value.parse().ok()),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if !matches!(tty, "?" | "??" | "-") {
            processes.insert(
                pid,
                ProcessTerminal {
                    group,
                    tty: tty.to_string(),
                },
            );
        }
    }
    processes
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
