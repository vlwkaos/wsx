//! The land's fast-forward meets `index.lock` in the checkout that has the default branch:
//! a stale one (over 5 minutes, no holder) is cleared and the merge runs again; a fresh one,
//! or one a live process holds open, refuses naming the lock, its age and the pid. Real git
//! in a scratch repository, the real `copy::fast_forward` the host's land calls.
//! ref: docs/tasks.md "Landing"

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};
use wsx_core::task::copy;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository on `main` and a commit ahead of it: (checkout, old tip, new commit).
fn fixture(name: &str) -> (PathBuf, String, String) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let tip = git(&root, &["rev-parse", "HEAD"]);
    fs::write(root.join("new.txt"), "x\n").unwrap();
    git(&root, &["checkout", "-q", "-b", "task"]);
    git(&root, &["add", "new.txt"]);
    git(&root, &["commit", "-q", "-m", "task"]);
    let commit = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    (root, tip, commit)
}

fn age(lock: &Path, seconds: u64) {
    File::options()
        .write(true)
        .open(lock)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(seconds))
        .unwrap();
}

#[test]
fn a_stale_lock_is_cleared_and_the_land_goes_on() {
    let (root, tip, commit) = fixture("lock-stale");
    let lock = root.join(".git/index.lock");
    let other = root.join(".git/shallow.lock");
    fs::write(&lock, "").unwrap();
    fs::write(&other, "").unwrap();
    age(&lock, 600);
    let cleared = copy::fast_forward(&root, "main", &tip, &commit)
        .unwrap()
        .expect("the stale lock is reported");
    assert_eq!(cleared.path, lock);
    assert!(cleared.age_s >= 600);
    assert!(!lock.exists() && other.exists(), "only index.lock goes");
    assert_eq!(git(&root, &["rev-parse", "main"]), commit);
}

#[test]
fn a_fresh_lock_refuses_and_stays() {
    let (root, tip, commit) = fixture("lock-fresh");
    let lock = root.join(".git/index.lock");
    fs::write(&lock, "").unwrap();
    age(&lock, 240);
    let error = copy::fast_forward(&root, "main", &tip, &commit)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&lock.display().to_string()), "{error}");
    assert!(
        error.contains("s old") && error.contains("younger than 300 s"),
        "{error}"
    );
    assert!(lock.exists());
    assert_eq!(git(&root, &["rev-parse", "main"]), tip);
}

#[test]
fn an_old_lock_a_live_process_holds_refuses_with_its_pid() {
    let (root, tip, commit) = fixture("lock-held");
    let lock = root.join(".git/index.lock");
    fs::write(&lock, "").unwrap();
    age(&lock, 600);
    let mut holder = Command::new("sleep")
        .arg("60")
        .stdout(Stdio::from(
            File::options().append(true).open(&lock).unwrap(),
        ))
        .spawn()
        .unwrap();
    let error = copy::fast_forward(&root, "main", &tip, &commit)
        .unwrap_err()
        .to_string();
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(
        error.contains(&format!("held open by pid {}", holder.id())),
        "{error}"
    );
    assert!(lock.exists());
    assert_eq!(git(&root, &["rev-parse", "main"]), tip);
}
