// Process tree snapshot for tmux pane classification.
// One `ps -ax -o pid,ppid,comm` call per refresh; walk descendants of each
// pane PID so an agent nested under shells/subprocesses still gets classified
// as Agent (tmux's `pane_current_command` reports the deepest spawned child,
// which can mask the real foreground — e.g. claude shows up as a node child
// named "2.1.x" while the actual claude process sits mid-tree).

use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct ProcTree {
    children: HashMap<u32, Vec<u32>>,
    comm: HashMap<u32, String>,
}

impl ProcTree {
    /// Snapshot the live process table. Returns an empty tree if `ps` fails.
    pub fn snapshot() -> Self {
        let Ok(out) = Command::new("ps")
            .args(["-ax", "-o", "pid=,ppid=,comm="])
            .output()
        else {
            return Self::default();
        };
        let text = String::from_utf8_lossy(&out.stdout);
        Self::parse(&text)
    }

    /// Parse the raw `ps` output. Tolerates leading whitespace and `comm`
    /// values that include path separators or spaces.
    pub fn parse(raw: &str) -> Self {
        let mut tree = Self::default();
        for line in raw.lines() {
            let mut iter = line.split_whitespace();
            let Some(pid) = iter.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Some(ppid) = iter.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let comm: String = iter.collect::<Vec<_>>().join(" ");
            if comm.is_empty() {
                continue;
            }
            tree.comm.insert(pid, comm);
            tree.children.entry(ppid).or_default().push(pid);
        }
        tree
    }

    /// Pure builder for tests.
    #[cfg(test)]
    pub fn from_rows(rows: &[(u32, u32, &str)]) -> Self {
        let mut tree = Self::default();
        for (pid, ppid, comm) in rows {
            tree.comm.insert(*pid, (*comm).to_string());
            tree.children.entry(*ppid).or_default().push(*pid);
        }
        tree
    }

    /// Iterate `root` and all its descendants, yielding `(pid, comm)` per
    /// process. `root` itself is yielded first if it exists in the tree.
    pub fn descendants(&self, root: u32) -> Vec<(u32, &str)> {
        let mut out: Vec<(u32, &str)> = Vec::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if let Some(c) = self.comm.get(&pid) {
                out.push((pid, c.as_str()));
            }
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        out
    }
}

/// Strip a `comm` value down to its bare executable name for classification.
/// macOS `ps -o comm` reports full paths (e.g. `/bin/zsh`); login shells
/// arrive prefixed with `-` (e.g. `-zsh`). Both should match `Shell`.
pub fn normalize_comm(comm: &str) -> &str {
    let basename = comm.rsplit('/').next().unwrap_or(comm);
    basename.strip_prefix('-').unwrap_or(basename)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse ────────────────────────────────────────────────────────────────

    #[test]
    fn given_single_line_when_parsed_then_pid_registered() {
        let tree = ProcTree::parse("100 1 zsh");
        let desc = tree.descendants(100);
        assert_eq!(desc, vec![(100u32, "zsh")]);
    }

    #[test]
    fn given_multiple_lines_when_parsed_then_all_pids_registered() {
        let raw = "100 1 zsh\n200 100 claude\n300 200 node";
        let tree = ProcTree::parse(raw);
        let mut desc: Vec<(u32, &str)> = tree.descendants(100);
        desc.sort_by_key(|(p, _)| *p);
        assert_eq!(desc, vec![(100, "zsh"), (200, "claude"), (300, "node")]);
    }

    #[test]
    fn given_leading_whitespace_when_parsed_then_pid_registered() {
        let tree = ProcTree::parse("   100 1 zsh");
        assert_eq!(tree.descendants(100), vec![(100u32, "zsh")]);
    }

    #[test]
    fn given_trailing_newline_and_blank_lines_when_parsed_then_tolerated() {
        let raw = "100 1 zsh\n\n   \n200 100 bash\n";
        let tree = ProcTree::parse(raw);
        let mut desc: Vec<(u32, &str)> = tree.descendants(100);
        desc.sort_by_key(|(p, _)| *p);
        assert_eq!(desc, vec![(100, "zsh"), (200, "bash")]);
    }

    #[test]
    fn given_tab_separator_when_parsed_then_pid_registered() {
        let tree = ProcTree::parse("100\t1\tzsh");
        assert_eq!(tree.descendants(100), vec![(100u32, "zsh")]);
    }

    #[test]
    fn given_comm_with_path_separator_when_parsed_then_full_path_kept() {
        let tree = ProcTree::parse("100 1 /bin/zsh");
        assert_eq!(tree.descendants(100), vec![(100u32, "/bin/zsh")]);
    }

    #[test]
    fn given_comm_with_internal_space_when_parsed_then_space_preserved() {
        let tree = ProcTree::parse("100 1 my prog");
        assert_eq!(tree.descendants(100), vec![(100u32, "my prog")]);
    }

    #[test]
    fn given_non_numeric_pid_when_parsed_then_line_skipped() {
        let tree = ProcTree::parse("abc 1 zsh\n100 1 bash");
        assert_eq!(tree.descendants(100), vec![(100u32, "bash")]);
    }

    #[test]
    fn given_non_numeric_ppid_when_parsed_then_line_skipped() {
        let tree = ProcTree::parse("100 xyz zsh");
        assert!(tree.descendants(100).is_empty());
    }

    #[test]
    fn given_empty_comm_when_parsed_then_line_skipped() {
        let tree = ProcTree::parse("100 1");
        assert!(tree.descendants(100).is_empty());
    }

    #[test]
    fn given_empty_input_when_parsed_then_tree_is_empty() {
        let tree = ProcTree::parse("");
        assert!(tree.descendants(1).is_empty());
        assert!(tree.descendants(0).is_empty());
    }

    // ── descendants ──────────────────────────────────────────────────────────

    #[test]
    fn given_chain_when_descendants_called_then_all_returned() {
        let tree = ProcTree::from_rows(&[
            (10, 1, "root"),
            (20, 10, "a"),
            (30, 20, "b"),
            (40, 30, "c"),
        ]);
        let mut pids: Vec<u32> = tree.descendants(10).into_iter().map(|(p, _)| p).collect();
        pids.sort();
        assert_eq!(pids, vec![10, 20, 30, 40]);
    }

    #[test]
    fn given_chain_when_descendants_called_then_root_returned_first() {
        let tree = ProcTree::from_rows(&[
            (10, 1, "root"),
            (20, 10, "a"),
            (30, 20, "b"),
        ]);
        let pids: Vec<u32> = tree.descendants(10).into_iter().map(|(p, _)| p).collect();
        assert_eq!(pids.first().copied(), Some(10));
    }

    #[test]
    fn given_root_with_no_children_when_descendants_called_then_only_root_returned() {
        let tree = ProcTree::from_rows(&[(10, 1, "root")]);
        assert_eq!(tree.descendants(10), vec![(10u32, "root")]);
    }

    #[test]
    fn given_absent_root_when_descendants_called_then_empty() {
        let tree = ProcTree::from_rows(&[(10, 1, "root")]);
        assert!(tree.descendants(999).is_empty());
    }

    #[test]
    fn given_multi_fanout_when_descendants_called_then_all_children_returned() {
        let tree = ProcTree::from_rows(&[
            (10, 1, "root"),
            (20, 10, "a"),
            (30, 10, "b"),
            (40, 10, "c"),
        ]);
        let mut pids: Vec<u32> = tree.descendants(10).into_iter().map(|(p, _)| p).collect();
        pids.sort();
        assert_eq!(pids, vec![10, 20, 30, 40]);
    }

    #[test]
    fn given_orphan_ppid_row_when_descendants_called_from_orphan_then_subtree_returned() {
        // orphan(200) claims ppid=999 which has no comm — reachable from its
        // own pid; descendants(999) traverses the children link but yields no
        // entry for the phantom parent.
        let tree = ProcTree::from_rows(&[
            (200, 999, "claude"),
            (300, 200, "node"),
        ]);
        let mut pids: Vec<u32> = tree.descendants(200).into_iter().map(|(p, _)| p).collect();
        pids.sort();
        assert_eq!(pids, vec![200, 300]);
        let mut phantom: Vec<u32> =
            tree.descendants(999).into_iter().map(|(p, _)| p).collect();
        phantom.sort();
        assert_eq!(phantom, vec![200, 300]);
    }

    // ── from_rows ────────────────────────────────────────────────────────────

    #[test]
    fn given_same_data_when_from_rows_and_parse_then_descendants_match() {
        let rows: &[(u32, u32, &str)] = &[
            (100, 1, "zsh"),
            (200, 100, "claude"),
            (300, 200, "node"),
        ];
        let from_rows = ProcTree::from_rows(rows);
        let parsed = ProcTree::parse("100 1 zsh\n200 100 claude\n300 200 node");
        let mut a: Vec<(u32, String)> = from_rows
            .descendants(100)
            .into_iter()
            .map(|(p, c)| (p, c.to_string()))
            .collect();
        let mut b: Vec<(u32, String)> = parsed
            .descendants(100)
            .into_iter()
            .map(|(p, c)| (p, c.to_string()))
            .collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    // ── normalize_comm ───────────────────────────────────────────────────────

    #[test]
    fn given_bare_name_when_normalized_then_unchanged() {
        assert_eq!(normalize_comm("claude"), "claude");
    }

    #[test]
    fn given_full_path_when_normalized_then_basename_returned() {
        assert_eq!(normalize_comm("/bin/zsh"), "zsh");
    }

    #[test]
    fn given_dash_prefix_when_normalized_then_dash_stripped() {
        assert_eq!(normalize_comm("-zsh"), "zsh");
    }

    #[test]
    fn given_dash_and_path_when_normalized_then_both_stripped() {
        assert_eq!(normalize_comm("-/usr/bin/bash"), "bash");
    }

    #[test]
    fn given_double_dash_prefix_when_normalized_then_only_one_dash_stripped() {
        assert_eq!(normalize_comm("--zsh"), "-zsh");
    }

    #[test]
    fn given_empty_string_when_normalized_then_empty() {
        assert_eq!(normalize_comm(""), "");
    }
}
