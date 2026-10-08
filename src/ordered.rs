//! Ordered fan-out: independent jobs produced on worker threads, consumed on
//! the calling thread in job order, with what is staged between the two
//! bounded.
//!
//! This is what decodes and encodes the folders of a non-solid archive in
//! parallel. A job is one folder. Workers claim jobs in index order, never more
//! than `window` ahead of the job the caller is consuming, and each job's
//! output goes through its own queue that holds at most `cap` bytes before the
//! worker filling it waits. So the memory between the workers and the caller
//! is bounded by `window` x `cap` (plus one message per queue, which may be
//! larger than `cap` on its own), whatever the jobs produce, and the caller
//! sees every job's output in job order however the workers finished.
//!
//! There are no timeouts anywhere: every wait is on a condition — a message, a
//! closed queue, a free place in the window, cancellation — and every change to
//! one of those conditions wakes the waiters.

use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
};

/// One job's queue.
struct Slot<M> {
    messages: VecDeque<(M, usize)>,
    /// What `messages` weigh, in the producer's units (bytes).
    weight: usize,
    /// A worker has the job.
    claimed: bool,
    /// The worker is done with the job: its sender is gone, whether the job
    /// finished or the worker unwound.
    closed: bool,
}

impl<M> Default for Slot<M> {
    fn default() -> Self {
        Self {
            messages: VecDeque::new(),
            weight: 0,
            claimed: false,
            closed: false,
        }
    }
}

struct State<M> {
    /// The next job a worker will claim.
    next: usize,
    /// The job the caller is consuming; `slots[0]` is its queue.
    current: usize,
    slots: VecDeque<Slot<M>>,
    /// The caller has stopped: every worker stops at its next send or claim.
    cancelled: bool,
    /// Workers that have not yet exited, so that a caller waiting on a job no
    /// worker is left to claim is told rather than left waiting.
    live: usize,
}

struct Shared<M> {
    state: Mutex<State<M>>,
    changed: Condvar,
    jobs: usize,
    window: usize,
    cap: usize,
}

impl<M> Shared<M> {
    fn lock(&self) -> MutexGuard<'_, State<M>> {
        // No caller code runs under this lock, so a poisoned lock can only be
        // a panic in this module's own bookkeeping, which leaves it
        // consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, State<M>>) -> MutexGuard<'a, State<M>> {
        self.changed
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn slot_mut(state: &mut State<M>, job: usize) -> Option<&mut Slot<M>> {
        let index = job.checked_sub(state.current)?;
        if state.slots.len() <= index {
            state.slots.resize_with(index + 1, Slot::default);
        }
        state.slots.get_mut(index)
    }
}

/// The caller stopped consuming, or moved past this job: stop producing.
#[derive(Debug)]
pub(crate) struct Cancelled;

/// A worker's end of one job's queue.
pub(crate) struct Sender<'a, M> {
    shared: &'a Shared<M>,
    job: usize,
}

impl<M> Sender<'_, M> {
    /// Queues `message`, which weighs `weight`, waiting while the queue holds
    /// `cap` or more. A message is always accepted into an empty queue, so a
    /// message heavier than `cap` passes rather than waiting for ever.
    pub(crate) fn send(&mut self, message: M, weight: usize) -> Result<(), Cancelled> {
        let mut state = self.shared.lock();
        loop {
            if state.cancelled {
                return Err(Cancelled);
            }
            let cap = self.shared.cap;
            let Some(slot) = Shared::slot_mut(&mut state, self.job) else {
                // The caller has moved past this job without reading it all.
                return Err(Cancelled);
            };
            if slot.messages.is_empty() || slot.weight.saturating_add(weight) <= cap {
                slot.messages.push_back((message, weight));
                slot.weight += weight;
                drop(state);
                self.shared.changed.notify_all();
                return Ok(());
            }
            state = self.shared.wait(state);
        }
    }
}

impl<M> Drop for Sender<'_, M> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        if let Some(slot) = Shared::slot_mut(&mut state, self.job) {
            slot.closed = true;
        }
        drop(state);
        self.shared.changed.notify_all();
    }
}

/// The caller's end of the queue of the job it is consuming.
pub(crate) struct Receiver<'a, M> {
    shared: &'a Shared<M>,
    job: usize,
}

impl<M> Receiver<'_, M> {
    /// The job's next message, waiting for it; `None` once the worker is done
    /// with the job and everything it sent has been taken — or when no worker
    /// is left to produce it at all.
    pub(crate) fn recv(&mut self) -> Option<M> {
        let mut state = self.shared.lock();
        loop {
            let live = state.live;
            let slot = Shared::slot_mut(&mut state, self.job)?;
            if let Some((message, weight)) = slot.messages.pop_front() {
                slot.weight -= weight;
                drop(state);
                self.shared.changed.notify_all();
                return Some(message);
            }
            if slot.closed || (!slot.claimed && live == 0) {
                return None;
            }
            state = self.shared.wait(state);
        }
    }
}

/// Counts a worker out when it exits, however it exits.
struct LiveGuard<'a, M>(&'a Shared<M>);

impl<M> Drop for LiveGuard<'_, M> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.live -= 1;
        drop(state);
        self.0.changed.notify_all();
    }
}

fn worker<M, P>(shared: &Shared<M>, produce: &P)
where
    P: Fn(usize, &mut Sender<'_, M>),
{
    let _live = LiveGuard(shared);
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if state.cancelled || state.next >= shared.jobs {
                    return;
                }
                if state.next < state.current + shared.window {
                    let job = state.next;
                    state.next += 1;
                    if let Some(slot) = Shared::slot_mut(&mut state, job) {
                        slot.claimed = true;
                    }
                    break job;
                }
                state = shared.wait(state);
            }
        };
        let mut sender = Sender { shared, job };
        produce(job, &mut sender);
    }
}

/// Runs `jobs` jobs on up to `workers` threads, handing each job's messages to
/// `consume` on the calling thread in job order.
///
/// `produce(job, sender)` runs on a worker and sends the job's output;
/// `consume(job, receiver)` runs on the caller and takes it. A job's queue holds
/// at most `cap` bytes (by the weights given to [`Sender::send`]) before its
/// worker waits, and workers are at most `window` jobs ahead of the one being
/// consumed. When `consume` fails, every worker is stopped and the error is
/// returned once they have all exited.
///
/// Returns `None`, having run nothing, when not one worker thread could be
/// started: the caller then does the work itself.
pub(crate) fn run<M, E, P, C>(
    jobs: usize,
    workers: usize,
    window: usize,
    cap: usize,
    produce: P,
    mut consume: C,
) -> Option<Result<(), E>>
where
    M: Send,
    P: Fn(usize, &mut Sender<'_, M>) + Sync,
    C: FnMut(usize, &mut Receiver<'_, M>) -> Result<(), E>,
{
    let shared = Shared {
        state: Mutex::new(State {
            next: 0,
            current: 0,
            slots: VecDeque::new(),
            cancelled: false,
            live: 0,
        }),
        changed: Condvar::new(),
        jobs,
        window: window.max(1),
        cap: cap.max(1),
    };
    std::thread::scope(|scope| {
        let mut started = 0usize;
        for index in 0..workers.max(1).min(jobs) {
            // Counted in before it starts, so that it is never seen as gone
            // before it has begun.
            shared.lock().live += 1;
            let spawned = std::thread::Builder::new()
                .name(format!("sevenz-folder-{index}"))
                .spawn_scoped(scope, || worker(&shared, &produce));
            if spawned.is_ok() {
                started += 1;
            } else {
                shared.lock().live -= 1;
                break;
            }
        }
        if started == 0 {
            return None;
        }
        let mut outcome = Ok(());
        for job in 0..jobs {
            let mut receiver = Receiver {
                shared: &shared,
                job,
            };
            let result = consume(job, &mut receiver);
            let mut state = shared.lock();
            // Whatever the caller left unread goes with the slot, and the
            // worker still filling it is told so at its next send.
            state.slots.pop_front();
            state.current += 1;
            if result.is_err() {
                state.cancelled = true;
            }
            drop(state);
            shared.changed.notify_all();
            if let Err(error) = result {
                outcome = Err(error);
                break;
            }
        }
        Some(outcome)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn output_arrives_in_job_order() {
        let mut seen = Vec::new();
        let outcome = run::<(usize, usize), (), _, _>(
            64,
            4,
            8,
            1 << 10,
            |job, tx| {
                for piece in 0..(job % 5) + 1 {
                    if tx.send((job, piece), 100).is_err() {
                        return;
                    }
                }
            },
            |job, rx| {
                let mut piece = 0;
                while let Some((from, n)) = rx.recv() {
                    assert_eq!((from, n), (job, piece));
                    piece += 1;
                }
                assert_eq!(piece, (job % 5) + 1);
                seen.push(job);
                Ok(())
            },
        );
        assert!(matches!(outcome, Some(Ok(()))));
        assert_eq!(seen, (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn staging_never_exceeds_the_window_times_the_cap() {
        // Every job sends far more than its cap. What the workers hold at any
        // moment is bounded by the window and the cap, whatever the caller's
        // pace; the caller checks the bound each time it takes a message.
        let staged = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let (window, cap, weight, workers) = (3, 4, 1, 3);
        let outcome = run::<usize, (), _, _>(
            12,
            workers,
            window,
            cap,
            |job, tx| {
                for _ in 0..50 {
                    let now = staged.fetch_add(weight, Ordering::SeqCst) + weight;
                    peak.fetch_max(now, Ordering::SeqCst);
                    if tx.send(job, weight).is_err() {
                        staged.fetch_sub(weight, Ordering::SeqCst);
                        return;
                    }
                }
            },
            |_, rx| {
                while rx.recv().is_some() {
                    staged.fetch_sub(weight, Ordering::SeqCst);
                }
                Ok(())
            },
        );
        assert!(matches!(outcome, Some(Ok(()))));
        // The counter runs ahead of the queues by at most one message per
        // worker (counted in, its send still waiting) and one on the caller
        // (taken from its queue, not yet counted out).
        assert!(peak.load(Ordering::SeqCst) <= window * cap + workers + 1);
    }

    #[test]
    fn a_failing_consumer_stops_every_worker() {
        let produced = AtomicUsize::new(0);
        let outcome = run::<usize, &'static str, _, _>(
            10_000,
            4,
            8,
            16,
            |job, tx| {
                produced.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(job, 1);
            },
            |job, rx| {
                while rx.recv().is_some() {}
                if job == 5 { Err("stop") } else { Ok(()) }
            },
        );
        assert!(matches!(outcome, Some(Err("stop"))));
        // Nothing past the window of the failing job was claimed.
        assert!(produced.load(Ordering::SeqCst) <= 6 + 8);
    }

    #[test]
    fn a_panicking_worker_does_not_leave_the_caller_waiting() {
        let result = std::panic::catch_unwind(|| {
            run::<usize, (), _, _>(
                4,
                1,
                2,
                16,
                |job, tx| {
                    if job == 1 {
                        panic!("worker failure");
                    }
                    let _ = tx.send(job, 1);
                },
                |_, rx| {
                    while rx.recv().is_some() {}
                    Ok(())
                },
            )
        });
        // The scope re-raises the worker's panic once the caller has seen the
        // job end and moved on; what matters is that it returns at all.
        assert!(result.is_err());
    }
}
