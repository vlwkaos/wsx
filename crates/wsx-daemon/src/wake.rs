// ^ [[wsx Architecture]] Wake assertions are bounded children of wsxd and never infer agent state.
use std::time::{Duration, Instant};

const ASSERTION_LIFETIME_SECS: u64 = 10 * 60;
const RENEW_AFTER: Duration = Duration::from_secs(9 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Keep,
    Start,
    Replace,
    Stop,
}

fn action(should_hold: bool, running: bool, age: Option<Duration>, retry_ready: bool) -> Action {
    if !should_hold {
        return if running { Action::Stop } else { Action::Keep };
    }
    if running {
        return if retry_ready && age.is_some_and(|age| age >= RENEW_AFTER) {
            Action::Replace
        } else {
            Action::Keep
        };
    }
    if retry_ready {
        Action::Start
    } else {
        Action::Keep
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::io;
    use std::process::{Child, Command, Stdio};

    struct Assertion {
        child: Child,
        started: Instant,
    }

    pub(crate) struct Controller {
        assertion: Option<Assertion>,
        retry_at: Option<Instant>,
    }

    impl Controller {
        pub(crate) fn new() -> Self {
            Self {
                assertion: None,
                retry_at: None,
            }
        }

        pub(crate) fn reconcile(&mut self, should_hold: bool, now: Instant) {
            let running = self
                .assertion
                .as_mut()
                .is_some_and(|assertion| matches!(assertion.child.try_wait(), Ok(None)));
            if !running {
                self.stop_current();
            }
            let retry_ready = self.retry_at.is_none_or(|retry_at| now >= retry_at);
            let age = self
                .assertion
                .as_ref()
                .map(|assertion| now.saturating_duration_since(assertion.started));
            match action(should_hold, running, age, retry_ready) {
                Action::Keep => {}
                Action::Stop => self.stop_current(),
                Action::Start => self.start(now),
                Action::Replace => self.replace(now),
            };
        }

        fn start(&mut self, now: Instant) {
            match spawn(now) {
                Ok(assertion) => {
                    self.assertion = Some(assertion);
                    self.retry_at = None;
                }
                Err(error) => {
                    eprintln!("wsxd wake mode could not start caffeinate: {error}");
                    self.retry_at = Some(now + RETRY_AFTER);
                }
            }
        }

        fn stop_current(&mut self) {
            if let Some(assertion) = self.assertion.take() {
                stop(assertion);
            }
        }

        fn replace(&mut self, now: Instant) {
            match spawn(now) {
                Ok(next) => {
                    self.stop_current();
                    self.assertion = Some(next);
                    self.retry_at = None;
                }
                Err(error) => {
                    eprintln!("wsxd wake mode could not renew caffeinate: {error}");
                    self.retry_at = Some(now + RETRY_AFTER);
                }
            }
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            self.stop_current();
        }
    }

    fn spawn(now: Instant) -> io::Result<Assertion> {
        let child = Command::new("/usr/bin/caffeinate")
            .args(["-i", "-t"])
            .arg(ASSERTION_LIFETIME_SECS.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Assertion {
            child,
            started: now,
        })
    }

    fn stop(mut assertion: Assertion) {
        let _ = assertion.child.kill();
        let _ = assertion.child.wait();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn sleeping_assertion() -> (Assertion, u32) {
            let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
            let pid = child.id();
            (
                Assertion {
                    child,
                    started: Instant::now(),
                },
                pid,
            )
        }

        fn assert_reaped(pid: u32) {
            let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
            assert_eq!(result, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }

        #[test]
        fn disabling_controller_terminates_and_reaps_assertion() {
            let (assertion, pid) = sleeping_assertion();
            let mut controller = Controller {
                assertion: Some(assertion),
                retry_at: None,
            };

            controller.reconcile(false, Instant::now());

            assert!(controller.assertion.is_none());
            assert_reaped(pid);
        }

        #[test]
        fn dropping_controller_terminates_and_reaps_assertion() {
            let (assertion, pid) = sleeping_assertion();
            drop(Controller {
                assertion: Some(assertion),
                retry_at: None,
            });
            assert_reaped(pid);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::time::Instant;

    pub(crate) struct Controller;

    impl Controller {
        pub(crate) fn new() -> Self {
            Self
        }

        pub(crate) fn reconcile(&mut self, _should_hold: bool, _now: Instant) {}
    }
}

pub(crate) use platform::Controller;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_starts_keeps_renews_and_stops_bounded_assertions() {
        assert_eq!(action(true, false, None, true), Action::Start);
        assert_eq!(action(true, false, None, false), Action::Keep);
        assert_eq!(
            action(true, true, Some(RENEW_AFTER - Duration::from_secs(1)), true),
            Action::Keep
        );
        assert_eq!(action(true, true, Some(RENEW_AFTER), true), Action::Replace);
        assert_eq!(action(true, true, Some(RENEW_AFTER), false), Action::Keep);
        assert_eq!(
            action(false, true, Some(Duration::ZERO), true),
            Action::Stop
        );
        assert_eq!(action(false, false, None, true), Action::Keep);
    }

    #[test]
    fn assertion_lifetime_exceeds_renewal_boundary() {
        assert!(Duration::from_secs(ASSERTION_LIFETIME_SECS) > RENEW_AFTER);
    }
}
