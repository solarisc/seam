//! The agent checks that its own control channel still answers. A lock that
//! never comes free can leave the window, the tray and `seam-agent stop` with
//! no way to reach the agent, and input sharing stuck, until Seam restarts.
//! After a run of unanswered checks the agent writes every thread's stack to
//! the log, so the stuck lock can be found, and exits for its supervisor to
//! start a fresh one.
use crate::{config::Config, shutdown::Signal, status};
use std::{sync::Arc, time::Duration};

/// How often the agent asks itself for its status.
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Unanswered checks in a row that mean the agent is stuck. Counted rather
/// than timed, so waking from sleep is not mistaken for a long silence. Well
/// past the slowest answer the agent gives, resetting input on every link.
const CHECKS_BEFORE_RESTART: u32 = 6;
/// Exit even when writing the log gets stuck too.
const EXIT_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) fn spawn(config: Config, shutdown: Arc<Signal>) {
    std::thread::spawn(move || {
        let agent = status::agent(&config);
        if watch(
            || agent.status().is_ok(),
            &shutdown,
            CHECK_INTERVAL,
            CHECKS_BEFORE_RESTART,
        ) {
            restart();
        }
    });
}

/// Run `check` every `interval` until `shutdown`. Returns `true` once `limit`
/// checks in a row failed after an earlier one succeeded; an agent that never
/// answered is still starting, or failed to start, which is reported elsewhere.
fn watch(
    mut check: impl FnMut() -> bool,
    shutdown: &Signal,
    interval: Duration,
    limit: u32,
) -> bool {
    let mut answered = false;
    let mut failures = 0;
    while !shutdown.wait(interval) {
        if check() {
            answered = true;
            failures = 0;
        } else if answered {
            failures += 1;
            if failures >= limit {
                return true;
            }
        }
    }
    false
}

fn restart() -> ! {
    std::thread::spawn(|| {
        std::thread::sleep(EXIT_DEADLINE);
        std::process::exit(1);
    });
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    let stacks = crate::windows_stacks::capture();
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    let stacks = String::from("(thread stacks are recorded on 64-bit Windows only)\n");
    log::error!(
        "Seam stopped answering its own status checks; restarting it. Thread stacks:\n{stacks}"
    );
    log::logger().flush();
    // Not a requested stop: the supervisor starts the agent again.
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, time::Instant};

    #[test]
    fn restarts_only_after_enough_unanswered_checks_in_a_row() {
        let shutdown = Signal::default();
        let answers = [true, false, false, true, false, false, false];
        let mut checks = 0;
        let stuck = watch(
            || {
                checks += 1;
                answers[checks - 1]
            },
            &shutdown,
            Duration::from_millis(1),
            3,
        );
        assert!(stuck);
        assert_eq!(checks, answers.len(), "an answer starts the count again");
    }

    #[test]
    fn an_agent_that_never_answered_is_left_alone() {
        let shutdown = Signal::default();
        let mut checks = 0;
        let stuck = watch(
            || {
                checks += 1;
                if checks == 10 {
                    shutdown.stop();
                }
                false
            },
            &shutdown,
            Duration::from_millis(1),
            3,
        );
        assert!(!stuck);
        assert_eq!(checks, 10);
    }

    #[test]
    fn a_control_port_that_never_replies_fails_the_check_promptly() {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            listen_port: silent.local_addr().unwrap().port() - 2,
            ..Config::default()
        };
        let started = Instant::now();
        assert!(status::agent(&config).status().is_err());
        assert!(started.elapsed() < CHECK_INTERVAL);
    }
}
