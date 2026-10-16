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
//! A caller may also pace the workers: [`run_paced`] asks it, each time a
//! worker is free to start a job, how many may be in hand at once. That is how
//! a thread ceiling changed while a decode runs reaches the folders still to
//! be started.
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
    /// Jobs a worker has claimed and is not yet done with.
    busy: usize,
    /// The job the caller is consuming; `slots[0]` is its queue.
    current: usize,
    slots: VecDeque<Slot<M>>,
    /// The caller has stopped: every worker stops at its next send or claim.
    cancelled: bool,
    /// Workers that have not yet exited, so that a caller waiting on a job no
    /// worker is left to claim is told rather than left waiting.
    live: usize,
    /// Producers sleeping because a byte queue is full, not for admission.
    sending: usize,
}

struct Shared<M> {
    state: Mutex<State<M>>,
    changed: Condvar,
    ready: Condvar,
    admission: Condvar,
    paced: bool,
    jobs: usize,
    window: usize,
    cap: usize,
}

impl<M> Shared<M> {
    fn lock(&self) -> MutexGuard<'_, State<M>> {
        // The only caller code that runs under this lock is the pace a
        // worker asks for before it claims a job, with nothing half-changed
        // around it. So a poisoned lock is a panic there or in this module's
        // own bookkeeping, and either leaves the state consistent.
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
                self.shared.ready.notify_one();
                if self.shared.paced {
                    self.shared.admission.notify_all();
                }
                return Ok(());
            }
            state.sending += 1;
            state = self.shared.wait(state);
            state.sending -= 1;
        }
    }
}

impl<M> Drop for Sender<'_, M> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.busy -= 1;
        if let Some(slot) = Shared::slot_mut(&mut state, self.job) {
            slot.closed = true;
        }
        drop(state);
        self.shared.ready.notify_one();
        if self.shared.paced {
            self.shared.admission.notify_all();
        }
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
                let wake_workers = state.sending != 0;
                drop(state);
                if wake_workers {
                    self.shared.changed.notify_all();
                }
                if self.shared.paced {
                    self.shared.admission.notify_all();
                }
                return Some(message);
            }
            if slot.closed || (!slot.claimed && live == 0) {
                return None;
            }
            state = self
                .shared
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Stops every worker when the caller's side ends, however it ends. A panic in
/// `consume` unwinds through the scope, which then waits for every worker: one
/// still waiting for room in a queue nobody reads any more would wait for ever.
struct CancelGuard<'a, M>(&'a Shared<M>);

impl<M> Drop for CancelGuard<'_, M> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.cancelled = true;
        drop(state);
        self.0.changed.notify_all();
        self.0.ready.notify_one();
        self.0.admission.notify_all();
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
        self.0.ready.notify_one();
        self.0.admission.notify_all();
    }
}

fn worker<M, P, S>(
    shared: &Shared<M>,
    width: Option<&(dyn Fn() -> usize + Sync)>,
    produce: &P,
    local: &mut S,
) where
    P: Fn(usize, &mut S, &mut Sender<'_, M>),
{
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if state.cancelled || state.next >= shared.jobs {
                    return;
                }
                // One job is always let through, whatever the pace: a worker
                // waiting here is woken by a job ending, and with none in
                // hand there would be nothing left to wake it.
                if state.busy < width.map_or(usize::MAX, |pace| pace().max(1))
                    && state.next < state.current + shared.window
                {
                    let job = state.next;
                    state.next += 1;
                    state.busy += 1;
                    if let Some(slot) = Shared::slot_mut(&mut state, job) {
                        slot.claimed = true;
                    }
                    break job;
                }
                state = shared
                    .admission
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let mut sender = Sender { shared, job };
        produce(job, local, &mut sender);
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
#[cfg(test)]
pub(crate) fn run<M, E, P, C>(
    jobs: usize,
    workers: usize,
    window: usize,
    cap: usize,
    produce: P,
    consume: C,
) -> Option<Result<(), E>>
where
    M: Send,
    P: Fn(usize, &mut Sender<'_, M>) + Sync,
    C: FnMut(usize, &mut Receiver<'_, M>) -> Result<(), E>,
{
    run_inner(
        jobs,
        workers,
        window,
        cap,
        None,
        (|| (), |job, _, tx| produce(job, tx)),
        consume,
    )
}

/// Runs ordered jobs with one reusable state object owned by each worker.
/// State is constructed on its worker and need not be Send or Sync.
#[cfg(any(feature = "compress", test))]
pub(crate) fn run_with_state<M, E, S, I, P, C>(
    jobs: usize,
    workers: usize,
    window: usize,
    cap: usize,
    initialize: I,
    produce: P,
    consume: C,
) -> Option<Result<(), E>>
where
    M: Send,
    I: Fn() -> S + Sync,
    P: Fn(usize, &mut S, &mut Sender<'_, M>) + Sync,
    C: FnMut(usize, &mut Receiver<'_, M>) -> Result<(), E>,
{
    run_inner(
        jobs,
        workers,
        window,
        cap,
        None,
        (initialize, produce),
        consume,
    )
}

/// Runs ordered jobs with the jobs in hand held to what `width` answers.
///
/// `width` is asked each time a worker is free to start a job, so its answer
/// may change while the run goes on. A job already started is finished at any
/// width; a narrower one holds back the jobs after it, and a wider one lets
/// them start the next time anything here changes — a message sent or taken,
/// a job ended — which is what wakes a waiting worker. An answer of zero is
/// taken as one. `width` runs on the workers, under this module's lock: it
/// must not wait on anything.
pub(crate) fn run_paced<M, E, P, C>(
    jobs: usize,
    workers: usize,
    window: usize,
    cap: usize,
    width: &(dyn Fn() -> usize + Sync),
    produce: P,
    consume: C,
) -> Option<Result<(), E>>
where
    M: Send,
    P: Fn(usize, &mut Sender<'_, M>) + Sync,
    C: FnMut(usize, &mut Receiver<'_, M>) -> Result<(), E>,
{
    run_inner(
        jobs,
        workers,
        window,
        cap,
        Some(width),
        (|| (), |job, _, tx| produce(job, tx)),
        consume,
    )
}

fn run_inner<M, E, S, I, P, C>(
    jobs: usize,
    workers: usize,
    window: usize,
    cap: usize,
    width: Option<&(dyn Fn() -> usize + Sync)>,
    work: (I, P),
    mut consume: C,
) -> Option<Result<(), E>>
where
    M: Send,
    I: Fn() -> S + Sync,
    P: Fn(usize, &mut S, &mut Sender<'_, M>) + Sync,
    C: FnMut(usize, &mut Receiver<'_, M>) -> Result<(), E>,
{
    let (initialize, produce) = work;
    let shared = Shared {
        state: Mutex::new(State {
            next: 0,
            busy: 0,
            current: 0,
            slots: VecDeque::new(),
            cancelled: false,
            live: 0,
            sending: 0,
        }),
        changed: Condvar::new(),
        ready: Condvar::new(),
        admission: Condvar::new(),
        paced: width.is_some(),
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
                .spawn_scoped(scope, || {
                    // Count this worker out even if state construction panics.
                    let _live = LiveGuard(&shared);
                    let mut local = initialize();
                    worker(&shared, width, &produce, &mut local);
                });
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
        // By the end of the loop below every job has been consumed or passed
        // over, so there is nothing left for a worker to do either way.
        let _cancel = CancelGuard(&shared);
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
            if shared.paced || result.is_err() {
                shared.admission.notify_all();
            } else {
                // Advancing one job opens exactly one place in the fixed
                // window. A byte-queue waiter cannot consume this wakeup.
                shared.admission.notify_one();
            }
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
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn worker_local_state_is_reused_and_dropped_after_cancellation() {
        use std::{cell::Cell, rc::Rc};
        struct Local<'a> {
            owner: std::thread::ThreadId,
            calls: Rc<Cell<usize>>,
            dropped: &'a AtomicUsize,
        }
        impl Drop for Local<'_> {
            fn drop(&mut self) {
                assert_eq!(self.owner, std::thread::current().id());
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
        let created = AtomicUsize::new(0);
        let dropped = AtomicUsize::new(0);
        let mut seen = std::collections::HashMap::new();
        let result = run_with_state::<_, (), _, _, _, _>(
            64,
            4,
            8,
            1,
            || {
                created.fetch_add(1, Ordering::SeqCst);
                Local {
                    owner: std::thread::current().id(),
                    calls: Rc::new(Cell::new(0)),
                    dropped: &dropped,
                }
            },
            |job, local, tx| {
                assert_eq!(local.owner, std::thread::current().id());
                local.calls.set(local.calls.get() + 1);
                let _ = tx.send((job, local.owner, local.calls.get()), 1);
            },
            |job, rx| {
                let (from, owner, call) = rx.recv().unwrap();
                assert_eq!(from, job);
                let previous = seen.entry(owner).or_insert(0);
                assert_eq!(call, *previous + 1);
                *previous = call;
                if job == 15 { Err(()) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Some(Err(()))));
        assert!(seen.values().any(|&calls| calls > 1));
        assert!(created.load(Ordering::SeqCst) <= 4);
        assert_eq!(
            created.load(Ordering::SeqCst),
            dropped.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_panicking_state_constructor_does_not_leave_the_caller_waiting() {
        let result = std::panic::catch_unwind(|| {
            run_with_state::<(), (), (), _, _, _>(
                4,
                1,
                2,
                1,
                || panic!("state construction failed"),
                |_, _, _| unreachable!(),
                |_, rx| {
                    assert!(rx.recv().is_none());
                    Ok(())
                },
            )
        });
        assert!(result.is_err());
    }

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

    #[test]
    fn a_panicking_consumer_does_not_leave_its_workers_waiting() {
        // Every job sends for as long as it is let, so each worker ends up
        // waiting for room in a queue that only the caller can empty.
        let result = std::panic::catch_unwind(|| {
            run::<usize, (), _, _>(
                8,
                4,
                4,
                2,
                |job, tx| while tx.send(job, 1).is_ok() {},
                |_, rx| {
                    let _ = rx.recv();
                    panic!("consumer failure");
                },
            )
        });
        // The scope joins every worker before it lets the panic out, so
        // returning at all is the workers having been told to stop.
        assert!(result.is_err());
    }

    #[test]
    fn a_message_too_heavy_for_its_stage_holds_its_worker() {
        // Job 0 sends two light messages and then one heavier than the stage
        // has room for beside them. That one is taken only once the caller
        // has emptied the stage, so the worker cannot have begun job 1 while
        // the second light message is still unread.
        let begun = AtomicUsize::new(0);
        let outcome = run::<u32, (), _, _>(
            2,
            1,
            2,
            4,
            |job, tx| {
                begun.fetch_max(job, Ordering::SeqCst);
                if job == 0 {
                    for message in [1, 2] {
                        let _ = tx.send(message, 1);
                    }
                    let _ = tx.send(3, 100);
                }
            },
            |job, rx| {
                if job == 0 {
                    assert_eq!(rx.recv(), Some(1));
                    assert_eq!(begun.load(Ordering::SeqCst), 0);
                    assert_eq!(rx.recv(), Some(2));
                    assert_eq!(rx.recv(), Some(3));
                }
                assert_eq!(rx.recv(), None);
                Ok(())
            },
        );
        assert!(matches!(outcome, Some(Ok(()))));
    }

    #[test]
    fn the_pace_bounds_how_many_jobs_are_in_hand_at_once() {
        for width in [0, 1, 2, 3] {
            let active = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let mut seen = Vec::new();
            let outcome = run_paced::<usize, (), _, _>(
                40,
                4,
                8,
                4,
                &|| width,
                |job, tx| {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    for _ in 0..6 {
                        if tx.send(job, 1).is_err() {
                            break;
                        }
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                },
                |job, rx| {
                    let mut pieces = 0;
                    while let Some(from) = rx.recv() {
                        assert_eq!(from, job);
                        pieces += 1;
                    }
                    assert_eq!(pieces, 6);
                    seen.push(job);
                    Ok(())
                },
            );
            assert!(matches!(outcome, Some(Ok(()))));
            assert_eq!(seen, (0..40).collect::<Vec<_>>());
            // A job is counted out here before its worker gives it up, so the
            // count never runs ahead of the jobs in hand.
            assert!(peak.load(Ordering::SeqCst) <= width.max(1), "width {width}");
        }
    }

    #[test]
    fn a_pace_widened_mid_run_lets_the_jobs_after_it_start_together() {
        // The jobs from `PAIRED` on each wait for another to be in hand with
        // them before they send anything. At a width of one that never
        // happens: the first of them is alone until the caller, reaching the
        // job before it, widens the run to two.
        const PAIRED: usize = 4;
        let width = AtomicUsize::new(1);
        let together = Barrier::new(2);
        let mut seen = Vec::new();
        let outcome = run_paced::<usize, (), _, _>(
            8,
            4,
            4,
            16,
            &|| width.load(Ordering::SeqCst),
            |job, tx| {
                if job >= PAIRED {
                    together.wait();
                }
                let _ = tx.send(job, 1);
            },
            |job, rx| {
                assert_eq!(rx.recv(), Some(job));
                assert_eq!(rx.recv(), None);
                if job + 1 == PAIRED {
                    width.store(2, Ordering::SeqCst);
                }
                seen.push(job);
                Ok(())
            },
        );
        assert!(matches!(outcome, Some(Ok(()))));
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
    }

    /// A heap-owning message that counts itself out, so that a message left
    /// in a queue is seen to be freed and not leaked or freed twice.
    struct Counted<'a> {
        bytes: Vec<u8>,
        dropped: &'a AtomicUsize,
    }

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_job_the_caller_moves_past_unread_frees_what_it_queued() {
        // The caller takes one message of every job and moves on, leaving
        // the rest in the slot and its worker mid-send; the worker is told at
        // its next send. At every width, everything made is freed once.
        for workers in 2..=4 {
            let made = AtomicUsize::new(0);
            let dropped = AtomicUsize::new(0);
            let cancelled = AtomicUsize::new(0);
            let outcome = run::<Counted<'_>, (), _, _>(
                24,
                workers,
                workers * 2,
                3,
                |job, tx| {
                    for piece in 0..16u8 {
                        made.fetch_add(1, Ordering::SeqCst);
                        let message = Counted {
                            bytes: vec![piece; 64 + job],
                            dropped: &dropped,
                        };
                        if tx.send(message, 1).is_err() {
                            cancelled.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                    }
                },
                |job, rx| {
                    let first = rx.recv().unwrap();
                    assert_eq!(first.bytes, vec![0; 64 + job]);
                    Ok(())
                },
            );
            assert!(matches!(outcome, Some(Ok(()))), "{workers} workers");
            assert_eq!(
                made.load(Ordering::SeqCst),
                dropped.load(Ordering::SeqCst),
                "{workers} workers"
            );
            assert!(cancelled.load(Ordering::SeqCst) > 0, "{workers} workers");
        }
    }

    #[test]
    fn a_failure_mid_run_stops_every_width_and_frees_every_queued_message() {
        for workers in 2..=4 {
            let made = AtomicUsize::new(0);
            let dropped = AtomicUsize::new(0);
            let outcome = run::<Counted<'_>, usize, _, _>(
                64,
                workers,
                workers * 2,
                8,
                |job, tx| {
                    loop {
                        made.fetch_add(1, Ordering::SeqCst);
                        let message = Counted {
                            bytes: vec![job as u8; 256],
                            dropped: &dropped,
                        };
                        if tx.send(message, 1).is_err() {
                            return;
                        }
                        if job % 3 != 0 {
                            // Jobs off the multiple of three end on their own.
                            return;
                        }
                    }
                },
                |job, rx| {
                    // Never ends a job of the multiple of three: it fails there
                    // after taking a few, with its worker still sending.
                    for _ in 0..4 {
                        let message = rx.recv().unwrap();
                        assert_eq!(message.bytes[0], job as u8);
                        if job % 3 != 0 {
                            break;
                        }
                    }
                    if job == 9 { Err(job) } else { Ok(()) }
                },
            );
            assert!(matches!(outcome, Some(Err(9))), "{workers} workers");
            assert_eq!(
                made.load(Ordering::SeqCst),
                dropped.load(Ordering::SeqCst),
                "{workers} workers"
            );
        }
    }

    #[test]
    fn a_consumer_dropping_its_receiver_early_does_not_strand_a_worker() {
        // The caller drops every receiver without reading at all, while a
        // worker of each width sends more than the cap holds.
        for workers in 2..=4 {
            let made = AtomicUsize::new(0);
            let dropped = AtomicUsize::new(0);
            let outcome = run::<Counted<'_>, (), _, _>(
                16,
                workers,
                workers,
                2,
                |job, tx| {
                    for _ in 0..8 {
                        made.fetch_add(1, Ordering::SeqCst);
                        let message = Counted {
                            bytes: vec![job as u8; 32],
                            dropped: &dropped,
                        };
                        if tx.send(message, 1).is_err() {
                            return;
                        }
                    }
                },
                |_, _| Ok(()),
            );
            assert!(matches!(outcome, Some(Ok(()))), "{workers} workers");
            assert_eq!(
                made.load(Ordering::SeqCst),
                dropped.load(Ordering::SeqCst),
                "{workers} workers"
            );
        }
    }
}
