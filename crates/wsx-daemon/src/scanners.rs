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
        }
        drop(state);
        thread::sleep(FOREGROUND_SCAN_INTERVAL);
    })
}

pub(crate) fn spawn_port_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
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
        let detected = if process_groups.is_empty() {
            Some(HashMap::new())
        } else {
            scan_listening_ports().map(|listeners| {
                let process_terminals = scan_process_terminals();
                attribute_listening_ports(&process_groups, &listeners, process_terminals.as_ref())
            })
        };
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

pub(crate) fn scan_listening_ports() -> Option<Vec<ListenerProcess>> {
    let mut command = Command::new("lsof");
    command.args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fpgn"]);
    let bytes = bounded_command_output(command, true)?;
    parse_lsof_ports(&String::from_utf8_lossy(&bytes))
}

pub(crate) fn scan_process_terminals() -> Option<HashMap<libc::pid_t, ProcessTerminal>> {
    let mut command = Command::new("ps");
    command.args(["-axo", "pid=,pgid=,tty="]);
    let bytes = bounded_command_output(command, false)?;
    parse_process_terminals(&String::from_utf8_lossy(&bytes))
}

pub(crate) fn bounded_command_output(
    mut command: Command,
    accept_code_one: bool,
) -> Option<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut bounded = stdout.take(MAX_PORT_SCAN_BYTES + 1);
        bounded.read_to_end(&mut bytes).ok()?;
        (bytes.len() as u64 <= MAX_PORT_SCAN_BYTES).then_some(bytes)
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PORT_SCAN_TIMEOUT => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()??;
    (status.success() || (accept_code_one && status.code() == Some(1))).then_some(bytes)
}

pub(crate) fn parse_lsof_ports(output: &str) -> Option<Vec<ListenerProcess>> {
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
    Some(listeners)
}

pub(crate) fn parse_process_terminals(
    output: &str,
) -> Option<HashMap<libc::pid_t, ProcessTerminal>> {
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
    Some(processes)
}

pub(crate) fn attribute_listening_ports(
    process_groups: &HashMap<PaneId, libc::pid_t>,
    listeners: &[ListenerProcess],
    process_terminals: Option<&HashMap<libc::pid_t, ProcessTerminal>>,
) -> HashMap<PaneId, Vec<u16>> {
    let panes_by_group = process_groups
        .iter()
        .map(|(pane_id, group)| (*group, *pane_id))
        .collect::<HashMap<_, _>>();
    let mut panes_by_tty = HashMap::<&str, Option<PaneId>>::new();
    if let Some(processes) = process_terminals {
        for process in processes.values() {
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
    }

    let mut ports_by_pane = HashMap::<PaneId, Vec<u16>>::new();
    for listener in listeners {
        let pane_id = panes_by_group.get(&listener.group).copied().or_else(|| {
            let processes = process_terminals?;
            let tty = &processes.get(&listener.pid)?.tty;
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
