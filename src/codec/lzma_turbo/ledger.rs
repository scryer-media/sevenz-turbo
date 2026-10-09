//! A ledger of what a parallel LZMA2 decode held and how its runs went out.
//!
//! It is kept only for a caller that asked ([`Lzma2Handle::keep_ledger`]). A
//! reader built for anyone else carries an empty `Option`, and every hook in
//! the decode returns at its first branch.
//!
//! Nothing here steers a decode. The hooks read the gauges the decode already
//! keeps, at the points where it already stops to look at them, and none of
//! them feeds, drains, waits or changes a gauge.

use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::{Lzma2Handle, Lzma2MtReader};

/// Waves recorded one by one. A decode with more of them counts the rest in
/// [`Lzma2Ledger::wave_count`] only.
const WAVES_KEPT: usize = 256;

/// Rows of [`Lzma2Ledger::wait_nanos_by_runs_out`]: a decoder takes at most 256
/// threads, so at most that many runs are out at once.
const RUNS_OUT_ROWS: usize = 257;

/// The piece size the decoder's input budget leaves room for beside a run in
/// hand, which is what [`Lzma2Ledger::refused_at_boundary_small`] is counted
/// against.
const SMALL_PIECE_BYTES: usize = 1 << 20;

/// What the parallel LZMA2 decodes of one reader held, and how their runs went
/// out to the worker threads.
///
/// Read from [`Lzma2Handle::ledger`] after [`Lzma2Handle::keep_ledger`]. The
/// figures are summed over every block the parallel coder has decoded since:
/// a peak is the largest any of them reached, a count is their total.
///
/// # What the byte figures are
///
/// Each is the largest value seen at the points where the decode looks: after
/// every read of packed input, after every hand-over to the decoder and after
/// every drain of decoded output. A buffer taken and released between two of
/// those points is not seen, so a peak here is never above the true one.
///
/// None of them counts a dictionary. A worker decodes a run into the buffer
/// the run's output is delivered from, which [`Self::peak_held_bytes`] counts;
/// the only dictionary a parallel decode allocates is the one its calling
/// thread uses when it decodes a run itself, and
/// [`Self::chase_bytes`] says whether it did.
///
/// # What a wave is
///
/// A wave is the runs a decoder claimed between two sleeps of the delivering
/// thread: it is closed each time that thread goes to sleep for a worker with
/// runs claimed since the wave before, and it is recorded with the runs that
/// were out as the thread went to sleep. A decode that keeps its workers
/// supplied claims one run for each that comes back, so after the first its
/// waves are of a run or two with as many runs out as there are threads. A
/// decode that lets its workers run dry claims nothing while they finish, and
/// its waves have as few runs out as were claimed in them.
///
/// A run is out from the moment a decoder claims it until its last byte is
/// delivered, so a run decoded and waiting its turn behind an earlier one is
/// still out: the figure is at least the threads at work, not exactly them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Lzma2Ledger {
    /// Blocks the parallel coder has started on.
    pub blocks: u64,
    /// The largest dictionary among those blocks.
    pub dictionary_bytes: u64,
    /// The largest allowance a coder was built with for what its decoder may
    /// hold: the caller's limit less the coder's own cost, or the default for
    /// the thread count where no limit was set.
    pub budget_bytes: u64,
    /// The widest thread ceiling a coder applied.
    pub threads: u64,
    /// The most worker threads that existed.
    pub worker_threads: u64,
    /// The most the decoder held: its input pieces, the buffer of every run
    /// out with a worker, decoded runs waiting their turn, and the buffers
    /// parked for reuse. By capacity, as the memory limit counts it.
    pub peak_held_bytes: u64,
    /// The most packed input read and not yet handed to the decoder, by
    /// length. This is the figure the read-ahead allowance is measured
    /// against.
    pub peak_queue_bytes: u64,
    /// The same queue by the capacity of its pieces.
    pub peak_queue_capacity_bytes: u64,
    /// The most decoded output held between the decoder and the caller, by
    /// capacity, counting the pieces kept to be filled again.
    pub peak_spill_bytes: u64,
    /// The most all three came to at one look: [`Self::peak_held_bytes`],
    /// [`Self::peak_queue_capacity_bytes`] and [`Self::peak_spill_bytes`]
    /// peak at different moments, so this is less than their sum.
    pub peak_total_bytes: u64,
    /// The most runs that were out at once: claimed by a decoder and not yet
    /// delivered whole.
    pub peak_runs_out: u64,
    /// The most complete runs the decoder had waiting for a worker.
    pub peak_runs_pending: u64,
    /// Runs handed to a decoder, by a worker or by the calling thread.
    pub runs: u64,
    /// Output decoded on the calling thread rather than by a worker.
    pub chase_bytes: u64,
    /// Waves there were. See the type's documentation.
    pub wave_count: u64,
    /// Runs claimed in each wave, in order, for the first 256 waves.
    pub wave_runs: Vec<u64>,
    /// The runs out as the delivering thread went to sleep at the end of each
    /// of those waves. For a wave still open when the ledger is read, the
    /// most that were out at a look since it began.
    pub wave_runs_out: Vec<u64>,
    /// Pieces of input the decoder handed back for want of room.
    pub refused_feeds: u64,
    /// Of those, the ones offered with the hand-over standing at the end of a
    /// run and no complete run waiting in the decoder. The decoder is then
    /// holding a whole run it cannot give to a worker, because the header that
    /// closes it is at the front of the piece it refused.
    pub refused_at_boundary: u64,
    /// Of [`Self::refused_at_boundary`], the ones with a run out.
    pub refused_at_boundary_busy: u64,
    /// Of [`Self::refused_at_boundary`], the pieces no larger than 1 MiB.
    pub refused_at_boundary_small: u64,
    /// The bytes of the [`Self::refused_at_boundary`] pieces.
    pub refused_at_boundary_bytes: u64,
    /// Refused pieces offered part way through a run.
    pub refused_mid_run: u64,
    /// Refused pieces offered at the end of a run with a complete run already
    /// waiting in the decoder.
    pub refused_run_pending: u64,
    /// Times the reader stopped reading ahead because what the decoder held
    /// left no room for another run under the budget.
    pub gate_refusals: u64,
    /// Times the reader stopped reading ahead with room for a run, because the
    /// decoder had the backlog it was reading for.
    pub backlog_stops: u64,
    /// Times the delivering thread slept until a worker handed a run back.
    pub waits: u64,
    /// How long it slept, in nanoseconds, by the runs out as it went to sleep:
    /// element `n` is the time spent waiting with `n` runs out. Against
    /// [`Self::threads`] this is the time workers stood idle.
    pub wait_nanos_by_runs_out: Vec<u64>,
}

/// The ledger's place in a reader's shared state: whether one is kept, and
/// the figures so far.
#[derive(Debug, Default)]
pub(super) struct LedgerSlot {
    kept: AtomicBool,
    total: Mutex<Lzma2Ledger>,
}

impl LedgerSlot {
    /// A probe for a coder about to be built, carrying on from the blocks
    /// before it, or `None` when no ledger is kept.
    pub(super) fn start(&self, dictionary_bytes: u64, budget_bytes: u64) -> Option<Box<Probe>> {
        if !self.kept.load(Ordering::Relaxed) {
            return None;
        }
        let mut ledger = self.total.lock().ok()?.clone();
        ledger.blocks += 1;
        ledger.dictionary_bytes = ledger.dictionary_bytes.max(dictionary_bytes);
        ledger.budget_bytes = ledger.budget_bytes.max(budget_bytes);
        Some(Box::new(Probe {
            runs_before: ledger.runs,
            chase_before: ledger.chase_bytes,
            ledger,
            claimed: 0,
            wave_from: 0,
            wave_peak: 0,
        }))
    }

    fn publish(&self, probe: &Probe) {
        if let Ok(mut total) = self.total.lock() {
            probe.snapshot_into(&mut total);
        }
    }
}

/// What one look at a decode reads off it.
#[derive(Debug, Clone, Copy, Default)]
struct Gauges {
    threads: u64,
    workers: u64,
    held: u64,
    queue: u64,
    queue_capacity: u64,
    spill: u64,
    claimed: u64,
    delivered: u64,
    pending: u64,
    chase: u64,
}

/// One coder's side of the ledger: the figures so far, and where the wave in
/// progress stands.
#[derive(Debug)]
pub(super) struct Probe {
    ledger: Lzma2Ledger,
    /// What the blocks before this one had added up to, which this coder's
    /// own running totals are added to.
    runs_before: u64,
    chase_before: u64,
    /// Runs this coder's decoder had claimed at the last look.
    claimed: u64,
    /// Runs it had claimed when the wave in progress began.
    wave_from: u64,
    /// The most runs out at once in the wave in progress.
    wave_peak: u64,
}

impl Probe {
    fn look(&mut self, gauges: &Gauges) {
        let ledger = &mut self.ledger;
        ledger.threads = ledger.threads.max(gauges.threads);
        ledger.worker_threads = ledger.worker_threads.max(gauges.workers);
        ledger.peak_held_bytes = ledger.peak_held_bytes.max(gauges.held);
        ledger.peak_queue_bytes = ledger.peak_queue_bytes.max(gauges.queue);
        ledger.peak_queue_capacity_bytes =
            ledger.peak_queue_capacity_bytes.max(gauges.queue_capacity);
        ledger.peak_spill_bytes = ledger.peak_spill_bytes.max(gauges.spill);
        ledger.peak_total_bytes = ledger.peak_total_bytes.max(
            gauges
                .held
                .saturating_add(gauges.queue_capacity)
                .saturating_add(gauges.spill),
        );
        let out = gauges.claimed.saturating_sub(gauges.delivered);
        ledger.peak_runs_out = ledger.peak_runs_out.max(out);
        ledger.peak_runs_pending = ledger.peak_runs_pending.max(gauges.pending);
        ledger.runs = self.runs_before.saturating_add(gauges.claimed);
        ledger.chase_bytes = self.chase_before.saturating_add(gauges.chase);
        self.claimed = gauges.claimed;
        self.wave_peak = self.wave_peak.max(out);
    }

    fn refused(&mut self, len: usize, at_boundary: bool, pending: usize, out: u64) {
        let ledger = &mut self.ledger;
        ledger.refused_feeds += 1;
        if !at_boundary {
            ledger.refused_mid_run += 1;
        } else if pending > 0 {
            ledger.refused_run_pending += 1;
        } else {
            ledger.refused_at_boundary += 1;
            ledger.refused_at_boundary_bytes += len as u64;
            if out > 0 {
                ledger.refused_at_boundary_busy += 1;
            }
            if len <= SMALL_PIECE_BYTES {
                ledger.refused_at_boundary_small += 1;
            }
        }
    }

    fn stopped(&mut self, room_for_a_run: bool) {
        if room_for_a_run {
            self.ledger.backlog_stops += 1;
        } else {
            self.ledger.gate_refusals += 1;
        }
    }

    /// Files a sleep of `nanos` that began with `out` runs out and `claimed`
    /// runs claimed, and closes the wave of the runs claimed since the last.
    fn slept(&mut self, out: u64, claimed: u64, nanos: u64) {
        let ledger = &mut self.ledger;
        ledger.waits += 1;
        let row = usize::try_from(out)
            .unwrap_or(usize::MAX)
            .min(RUNS_OUT_ROWS - 1);
        if ledger.wait_nanos_by_runs_out.len() <= row {
            ledger.wait_nanos_by_runs_out.resize(row + 1, 0);
        }
        ledger.wait_nanos_by_runs_out[row] =
            ledger.wait_nanos_by_runs_out[row].saturating_add(nanos);
        if claimed > self.wave_from {
            push_wave(ledger, claimed - self.wave_from, out);
            self.wave_from = claimed;
            self.wave_peak = 0;
        }
        self.claimed = self.claimed.max(claimed);
    }

    /// Writes the figures so far over `total`, with the wave in progress
    /// counted as it stands.
    fn snapshot_into(&self, total: &mut Lzma2Ledger) {
        total.clone_from(&self.ledger);
        if self.claimed > self.wave_from {
            push_wave(total, self.claimed - self.wave_from, self.wave_peak);
        }
    }
}

fn push_wave(ledger: &mut Lzma2Ledger, runs: u64, out: u64) {
    ledger.wave_count += 1;
    if ledger.wave_runs.len() < WAVES_KEPT {
        ledger.wave_runs.push(runs);
        ledger.wave_runs_out.push(out);
    }
}

impl Lzma2Handle {
    /// Starts keeping a [`Lzma2Ledger`] of the reader's parallel LZMA2
    /// decodes, from the next block whose coder is built.
    ///
    /// A ledger is for measuring a decode, not for running one: it changes
    /// nothing about how a block is decoded, and a reader never asked for one
    /// does no work for it.
    pub fn keep_ledger(&self) {
        self.control.ledger.kept.store(true, Ordering::Relaxed);
    }

    /// The ledger so far, or `None` unless [`Lzma2Handle::keep_ledger`] was
    /// called.
    ///
    /// It can be read while a decode is running, and is then as of the
    /// decode's last look. A block decoded by the single-threaded coder, or as
    /// one of several folders decoded at once, adds nothing to it.
    #[must_use]
    pub fn ledger(&self) -> Option<Lzma2Ledger> {
        let slot = &self.control.ledger;
        if !slot.kept.load(Ordering::Relaxed) {
            return None;
        }
        slot.total.lock().ok().map(|total| total.clone())
    }
}

impl<R: Read> Lzma2MtReader<R> {
    /// Takes the gauges, when a ledger is kept.
    #[inline]
    pub(super) fn ledger_look(&mut self) {
        if self.ledger.is_some() {
            self.ledger_look_kept();
        }
    }

    #[cold]
    fn ledger_look_kept(&mut self) {
        let Some(mut probe) = self.ledger.take() else {
            return;
        };
        let spill: usize = self.out.iter().chain(&self.spare).map(Vec::capacity).sum();
        probe.look(&Gauges {
            threads: u64::from(self.applied_threads),
            workers: self.decoder.spawned_threads() as u64,
            held: self.decoder.held_bytes(),
            queue: self.held as u64,
            queue_capacity: self.segs.iter().map(Vec::capacity).sum::<usize>() as u64,
            spill: spill as u64,
            claimed: self.decoder.runs_claimed(),
            delivered: self.runs_delivered,
            pending: self.decoder.pending_runs() as u64,
            chase: self.decoder.chase_decoded_bytes(),
        });
        self.control.ledger.publish(&probe);
        self.ledger = Some(probe);
    }

    /// Notes a piece the decoder handed back, when a ledger is kept.
    #[inline]
    pub(super) fn ledger_refused(&mut self, len: usize) {
        if self.ledger.is_some() {
            let at_boundary = self.ledger_at_boundary();
            let pending = self.decoder.pending_runs();
            let out = self.busy_workers();
            if let Some(probe) = self.ledger.as_mut() {
                probe.refused(len, at_boundary, pending, out);
            }
        }
    }

    /// Notes that the read-ahead stopped with the hand-over on a run boundary,
    /// and whether it was the budget that stopped it, when a ledger is kept.
    /// A stream whose headers could not be walked is not stopped by the
    /// budget: the decoder is steering it and no run is being counted.
    #[inline]
    pub(super) fn ledger_stopped(&mut self) {
        if self.ledger.is_some() {
            let room = self.scan_broken || self.room_for_a_run();
            if let Some(probe) = self.ledger.as_mut() {
                probe.stopped(room);
            }
        }
    }

    /// The decoder's wait for a worker, timed when a ledger is kept.
    #[inline]
    pub(super) fn wait_for_worker(&mut self) -> bool {
        if self.ledger.is_none() {
            return self.decoder.wait_for_worker();
        }
        let out = self.busy_workers();
        let claimed = self.decoder.runs_claimed();
        let started = Instant::now();
        let landed = self.decoder.wait_for_worker();
        if landed {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if let Some(probe) = self.ledger.as_mut() {
                probe.slept(out, claimed, nanos);
            }
        }
        landed
    }

    /// Whether what has been handed over ends where a run does, asked without
    /// moving anything. `fed_at_boundary` retires the run ends it passes, and
    /// a ledger has to leave the decode exactly as it found it.
    fn ledger_at_boundary(&self) -> bool {
        let reached = self.run_ends.partition_point(|&end| end <= self.fed_to);
        let last = reached
            .checked_sub(1)
            .and_then(|index| self.run_ends.get(index))
            .copied()
            .unwrap_or(self.last_boundary);
        last == self.fed_to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Probe {
        let slot = LedgerSlot::default();
        slot.kept.store(true, Ordering::Relaxed);
        *slot.start(1 << 20, 1 << 30).expect("a ledger is kept")
    }

    /// One step of a decode as a probe sees it, by the runs claimed and
    /// delivered at the time: a look at the gauges, or the delivering thread
    /// going to sleep for a worker.
    #[derive(Clone, Copy)]
    enum Step {
        Look(u64, u64),
        Sleep(u64, u64),
    }
    use Step::{Look, Sleep};

    fn stepped(steps: &[Step]) -> Lzma2Ledger {
        let mut probe = probe();
        for &step in steps {
            match step {
                Look(claimed, delivered) => probe.look(&Gauges {
                    claimed,
                    delivered,
                    ..Gauges::default()
                }),
                Sleep(claimed, delivered) => probe.slept(claimed - delivered, claimed, 1),
            }
        }
        let mut total = Lzma2Ledger::default();
        probe.snapshot_into(&mut total);
        total
    }

    /// A slot nobody asked to keep hands out no probe, so a reader built from
    /// it has nothing to do at any hook.
    #[test]
    fn an_unkept_slot_starts_no_probe() {
        let slot = LedgerSlot::default();
        assert!(slot.start(1 << 20, 1 << 30).is_none());
    }

    /// Workers kept supplied claim a run for each that comes back: after the
    /// first wave every wave is one run, with a run out on every thread. A
    /// sleep with nothing claimed since the last closes no wave.
    #[test]
    fn workers_kept_supplied_have_every_thread_out_in_every_wave() {
        let ledger = stepped(&[
            Look(4, 0),
            Sleep(4, 0),
            Look(5, 1),
            Sleep(5, 1),
            Look(6, 2),
            Sleep(6, 2),
            Look(7, 3),
            Sleep(7, 3),
            Look(8, 4),
            Sleep(8, 4),
            Look(8, 6),
            Sleep(8, 6),
            Look(8, 8),
        ]);
        assert_eq!(ledger.runs, 8);
        assert_eq!(ledger.waits, 6);
        assert_eq!(ledger.wave_count, 5);
        assert_eq!(ledger.wave_runs, [4, 1, 1, 1, 1]);
        assert_eq!(ledger.wave_runs_out, [4, 4, 4, 4, 4]);
        assert_eq!(ledger.peak_runs_out, 4);
    }

    /// Workers left to run dry claim nothing while the runs out come back, so
    /// each wave has only the runs claimed in it out.
    #[test]
    fn workers_left_to_run_dry_show_in_the_runs_out() {
        let ledger = stepped(&[
            Look(4, 0),
            Sleep(4, 0),
            Look(4, 3),
            Sleep(4, 3),
            Look(4, 4),
            Look(7, 4),
            Sleep(7, 4),
            Look(7, 7),
            Look(8, 7),
            Sleep(8, 7),
            Look(8, 8),
            Look(9, 8),
            Sleep(9, 8),
            Look(9, 9),
        ]);
        assert_eq!(ledger.wave_runs, [4, 3, 1, 1]);
        assert_eq!(ledger.wave_runs_out, [4, 3, 1, 1]);
        assert_eq!(ledger.wave_count, 4);
        assert_eq!(ledger.wave_runs.iter().sum::<u64>(), ledger.runs);
    }

    /// The next run is claimed as the last byte of the one before is still on
    /// its way out, so no look ever finds nothing out. The runs out as the
    /// thread goes to sleep still say one thread was at work.
    #[test]
    fn a_run_claimed_before_the_last_is_delivered_does_not_hide_a_stall() {
        let ledger = stepped(&[
            Look(1, 0),
            Sleep(1, 0),
            Look(2, 0),
            Look(2, 1),
            Sleep(2, 1),
            Look(3, 1),
            Look(3, 2),
            Sleep(3, 2),
            Look(3, 3),
        ]);
        assert_eq!(ledger.peak_runs_out, 2);
        assert_eq!(ledger.wave_runs, [1, 1, 1]);
        assert_eq!(ledger.wave_runs_out, [1, 1, 1]);
    }

    /// A wave still open when the ledger is read is counted as it stands,
    /// with the most runs that were out at a look, and reading it does not
    /// close it: the sleep that does records the runs out as it began.
    #[test]
    fn a_wave_in_progress_is_counted_as_it_stands() {
        let mut probe = probe();
        let gauges = |claimed, delivered| Gauges {
            claimed,
            delivered,
            ..Gauges::default()
        };
        probe.look(&gauges(3, 0));
        let mut total = Lzma2Ledger::default();
        probe.snapshot_into(&mut total);
        assert_eq!(total.wave_runs, [3]);
        assert_eq!(total.wave_runs_out, [3]);
        probe.look(&gauges(4, 2));
        probe.snapshot_into(&mut total);
        assert_eq!(total.wave_runs, [4]);
        assert_eq!(total.wave_runs_out, [3]);
        assert_eq!(total.wave_count, 1);
        probe.slept(2, 4, 1);
        probe.snapshot_into(&mut total);
        assert_eq!(total.wave_runs, [4]);
        assert_eq!(total.wave_runs_out, [2]);
        assert_eq!(total.wave_count, 1);
    }

    /// A second block carries on from the first: counts add, peaks stand, and
    /// the waves of both are there in order.
    #[test]
    fn a_second_block_adds_to_the_first() {
        let slot = LedgerSlot::default();
        slot.kept.store(true, Ordering::Relaxed);
        let mut first = slot.start(1 << 20, 1 << 30).expect("kept");
        first.look(&Gauges {
            claimed: 2,
            held: 900,
            chase: 5,
            ..Gauges::default()
        });
        first.look(&Gauges {
            claimed: 2,
            delivered: 2,
            chase: 5,
            ..Gauges::default()
        });
        slot.publish(&first);
        let mut second = slot.start(4 << 20, 1 << 20).expect("kept");
        second.look(&Gauges {
            claimed: 3,
            held: 100,
            chase: 1,
            ..Gauges::default()
        });
        slot.publish(&second);
        let total = slot.total.lock().expect("lock").clone();
        assert_eq!(total.blocks, 2);
        assert_eq!(total.dictionary_bytes, 4 << 20);
        assert_eq!(total.budget_bytes, 1 << 30);
        assert_eq!(total.runs, 5);
        assert_eq!(total.chase_bytes, 6);
        assert_eq!(total.peak_held_bytes, 900);
        assert_eq!(total.wave_runs, [2, 3]);
    }

    /// The three kinds of refusal are told apart by where the hand-over stood
    /// and what the decoder had waiting, and only the first kind is broken
    /// down further.
    #[test]
    fn refusals_are_sorted_by_where_the_hand_over_stood() {
        let mut probe = probe();
        probe.refused(4 << 20, true, 0, 1);
        probe.refused(1 << 20, true, 0, 0);
        probe.refused(2 << 20, false, 0, 1);
        probe.refused(2 << 20, true, 2, 1);
        let ledger = &probe.ledger;
        assert_eq!(ledger.refused_feeds, 4);
        assert_eq!(ledger.refused_at_boundary, 2);
        assert_eq!(ledger.refused_at_boundary_busy, 1);
        assert_eq!(ledger.refused_at_boundary_small, 1);
        assert_eq!(ledger.refused_at_boundary_bytes, 5 << 20);
        assert_eq!(ledger.refused_mid_run, 1);
        assert_eq!(ledger.refused_run_pending, 1);
    }

    /// Time asleep is filed under the runs that were out, and a count past
    /// the table's last row goes in the last row.
    #[test]
    fn sleeps_are_filed_by_the_runs_out() {
        let mut probe = probe();
        probe.slept(1, 0, 10);
        probe.slept(3, 0, 5);
        probe.slept(1, 0, 7);
        probe.slept(100_000, 0, 2);
        let ledger = &probe.ledger;
        assert_eq!(ledger.waits, 4);
        assert_eq!(ledger.wait_nanos_by_runs_out[..4], [0, 17, 0, 5]);
        assert_eq!(ledger.wait_nanos_by_runs_out.len(), RUNS_OUT_ROWS);
        assert_eq!(ledger.wait_nanos_by_runs_out[RUNS_OUT_ROWS - 1], 2);
    }
}
