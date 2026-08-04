//! Backoff for the optimistic-commit retry loop.
//!
//! The commit protocol resolves a contested precondition to exactly one winner
//! and hands the loser [`CommitOutcome::Rejected`] with zero effect
//! (`DESIGN.md` §5.2). *Correctness* needs nothing more. **Fairness** does: a
//! loser that retries immediately collides with the same winner again, and the
//! measured result was `fair(min/max) = 0.000` at 8 racing threads — at least one
//! thread committing nothing at all while the protocol reported no errors
//! (`docs/PERFORMANCE-ENT.md` §3).
//!
//! [`Backoff`] is the missing half: **randomized exponential backoff plus a
//! bounded attempt count**, so contending writers spread out instead of
//! synchronizing, and a livelock surfaces as a real error rather than an
//! infinite spin.
//!
//! ## The jitter is not nondeterminism
//!
//! The Replay law says wall-clock and randomness may enter the world only as
//! committed data ([`ClockDriver`](crate::ClockDriver)). Backoff never enters the
//! world: it decides *when a thread tries again*, never *what it writes*. The
//! retried patch is rebuilt from a fresh snapshot by the caller's closure and is
//! a pure function of committed data exactly as before.
//!
//! Even so, this deliberately draws no entropy from the environment. Jitter's
//! job is to **decorrelate** contending threads, not to be unpredictable, so the
//! sequence comes from a xorshift seeded off a process-wide counter: two
//! `Backoff`s made in the same process always differ, and the whole thing stays
//! reproducible under a debugger. No clock is read and no OS randomness is
//! sampled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use grmpl_core::{Edition, Error, Result};

use crate::commit::CommitOutcome;

/// Distinguishes successive `Backoff`s so contending threads desynchronize.
static STREAM: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);

/// Randomized exponential backoff with a bounded attempt count.
///
/// The delay before attempt *k* is drawn uniformly from `[0, min(base · 2ᵏ,
/// cap))` — "full jitter", which decorrelates contenders better than a fixed
/// or capped-exponential delay, since two threads that collide once do not
/// collide again on a schedule.
#[derive(Clone, Debug)]
pub struct Backoff {
    /// How many attempts remain (the first attempt is not a retry).
    left: u32,
    /// The window for the next retry, in microseconds; doubles per retry.
    window_us: u64,
    /// The ceiling on `window_us`.
    cap_us: u64,
    /// xorshift64 state.
    state: u64,
}

impl Default for Backoff {
    /// 16 attempts, a 50 µs first window doubling to a 5 ms ceiling.
    ///
    /// The floor is chosen against the substrate's own durability floor: a
    /// commit costs roughly 1 ms of `fsync`, so a first retry inside ~50 µs is
    /// still "immediately" in commit terms, while 16 doublings past it span far
    /// more contention than any healthy world produces.
    fn default() -> Backoff {
        Backoff::new(16, 50, 5_000)
    }
}

impl Backoff {
    /// A policy of at most `attempts` tries, backing off from a `base_us`
    /// window up to `cap_us`. `attempts` counts the first try, so
    /// `new(1, ..)` never retries.
    pub fn new(attempts: u32, base_us: u64, cap_us: u64) -> Backoff {
        // A distinct stream per Backoff: the point of jitter is that two
        // contenders diverge, which a shared sequence would defeat.
        let seed = STREAM.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) | 1;
        Backoff {
            left: attempts.max(1),
            window_us: base_us.max(1),
            cap_us: cap_us.max(1),
            state: seed,
        }
    }

    /// A policy that never retries — the pre-existing "one shot, report the
    /// outcome" behavior, for callers that treat a rejection as an answer rather
    /// than as contention.
    pub fn none() -> Backoff {
        Backoff::new(1, 1, 1)
    }

    /// How many attempts remain.
    pub fn remaining(&self) -> u32 {
        self.left
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*: cheap, no dependency, well-distributed in the low bits we
        // use here.
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Consume one attempt. Returns `false` once the budget is spent (the
    /// caller should surface a contention error); otherwise sleeps for a jittered
    /// interval and returns `true`.
    pub fn wait(&mut self) -> bool {
        if self.left <= 1 {
            self.left = 0;
            return false;
        }
        self.left -= 1;
        let delay = self.next_u64() % self.window_us;
        self.window_us = (self.window_us * 2).min(self.cap_us);
        if delay > 0 {
            std::thread::sleep(Duration::from_micros(delay));
        }
        true
    }
}

/// Run `attempt` until it commits, backing off between rejections.
///
/// `attempt` must **rebuild** its patch from the store's current state on every
/// call — that is the whole point of the optimistic protocol: the loser of a
/// race re-reads the winner's world and decides again. A closure that returns a
/// patch captured from a stale snapshot will be rejected forever (and then
/// surface as a contention error, which is the honest outcome).
///
/// Exhausting the policy is an [`Error::Store`], not a silent `Rejected`: a
/// caller that cannot commit after a bounded number of tries is looking at
/// livelock or a permanently-false precondition, and both deserve to be seen.
pub fn commit_retrying(
    mut policy: Backoff,
    mut attempt: impl FnMut() -> Result<CommitOutcome>,
) -> Result<Edition> {
    let mut rejections = 0u32;
    loop {
        match attempt()? {
            CommitOutcome::Committed(e) => return Ok(e),
            CommitOutcome::Rejected => {
                rejections += 1;
                if !policy.wait() {
                    return Err(Error::Store(format!(
                        "commit contention: rejected {rejections} times, giving up"
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_committing_attempt_never_sleeps() {
        let e = commit_retrying(Backoff::default(), || {
            Ok(CommitOutcome::Committed(Edition(7)))
        })
        .unwrap();
        assert_eq!(e, Edition(7));
    }

    #[test]
    fn a_bounded_policy_gives_up_rather_than_spinning() {
        let mut tries = 0;
        let err = commit_retrying(Backoff::new(4, 1, 2), || {
            tries += 1;
            Ok(CommitOutcome::Rejected)
        })
        .unwrap_err();
        assert_eq!(tries, 4, "the budget counts the first attempt");
        assert!(format!("{err:?}").contains("contention"), "{err:?}");
    }

    #[test]
    fn a_loser_that_re_reads_eventually_wins() {
        let mut tries = 0;
        let e = commit_retrying(Backoff::default(), || {
            tries += 1;
            Ok(if tries < 3 {
                CommitOutcome::Rejected
            } else {
                CommitOutcome::Committed(Edition(tries))
            })
        })
        .unwrap();
        assert_eq!(e, Edition(3));
    }

    #[test]
    fn none_is_a_single_shot() {
        let mut tries = 0;
        let _ = commit_retrying(Backoff::none(), || {
            tries += 1;
            Ok(CommitOutcome::Rejected)
        });
        assert_eq!(tries, 1);
    }

    #[test]
    fn two_backoffs_draw_different_jitter() {
        let (mut a, mut b) = (Backoff::default(), Backoff::default());
        let xs: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_ne!(xs, ys, "contenders must not share a jitter sequence");
    }

    #[test]
    fn the_window_doubles_to_the_cap_and_stops() {
        let mut p = Backoff::new(64, 10, 40);
        let mut windows = Vec::new();
        for _ in 0..5 {
            windows.push(p.window_us);
            p.wait();
        }
        assert_eq!(windows, vec![10, 20, 40, 40, 40]);
    }
}
