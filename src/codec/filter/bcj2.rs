//! BCJ2, the four-stream branch converter for x86 executables, read from a 7z
//! folder's four coder outputs and, behind `compress`, written into them.
//!
//! The conversion itself is `lzma_turbo::filters::bcj2`, the SDK's `Bcj2.c`
//! ported and held bit-exact against it. That decoder is slice-driven: it is
//! handed whatever the caller has of each stream and says how much it took.
//! This reader is the `Read` around it, which is what the block decoder
//! wants: it keeps a buffer per stream, refills whichever one the decoder ran
//! dry, and stops at the size the folder recorded.

use std::io::Read;

use lzma_turbo::filters::bcj2::{
    Bcj2Dec, Bcj2DecStreams, NUM_STREAMS, STREAM_CALL, STREAM_JUMP, STREAM_MAIN, STREAM_RC,
};
#[cfg(feature = "compress")]
use lzma_turbo::filters::bcj2::{Bcj2Enc, Bcj2EncFinishMode, Bcj2EncOut};

use super::error_invalid_data;

/// How much of a stream is read at a time.
const BUF_SIZE: usize = 1 << 18;

/// Reader for BCJ2-filtered data with multiple input streams.
pub struct Bcj2Reader<R> {
    inputs: Vec<R>,
    /// A window per stream, allocated once: read into up to `filled` and
    /// consumed from `pos`.
    bufs: [Box<[u8]>; NUM_STREAMS],
    pos: [usize; NUM_STREAMS],
    filled: [usize; NUM_STREAMS],
    /// Which inputs have returned end-of-stream.
    eof: [bool; NUM_STREAMS],
    decoder: Bcj2Dec,
    remaining: u64,
}

impl<R> Bcj2Reader<R> {
    /// Creates a new BCJ2 reader with the given input streams and expected output size.
    ///
    /// `inputs` are the main, call, jump and range-coded streams, in the
    /// order [`STREAM_MAIN`], [`STREAM_CALL`], [`STREAM_JUMP`], [`STREAM_RC`],
    /// which is the order a 7z folder declares them.
    pub fn new(inputs: Vec<R>, uncompressed_size: u64) -> Self {
        Self {
            inputs,
            bufs: std::array::from_fn(|_| vec![0u8; BUF_SIZE].into_boxed_slice()),
            pos: [0; NUM_STREAMS],
            filled: [0; NUM_STREAMS],
            eof: [false; NUM_STREAMS],
            decoder: Bcj2Dec::new(),
            remaining: uncompressed_size,
        }
    }
}

/// The unconsumed part of a stream, trimmed to whole words for the two
/// streams the decoder reads four bytes at a time.
fn pending(buf: &[u8], pos: usize, filled: usize, stream: usize) -> &[u8] {
    let pending = &buf[pos..filled];
    if stream == STREAM_CALL || stream == STREAM_JUMP {
        &pending[..pending.len() & !3]
    } else {
        pending
    }
}

impl<R: Read> Read for Bcj2Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let window = usize::try_from(self.remaining).map_or(buf.len(), |r| r.min(buf.len()));
        let dest = &mut buf[..window];
        if dest.is_empty() {
            return Ok(0);
        }
        let Self {
            inputs,
            bufs,
            pos,
            filled,
            eof,
            decoder,
            remaining,
        } = self;
        let mut written = 0;
        loop {
            let produced = {
                let [main, call, jump, rc] = [STREAM_MAIN, STREAM_CALL, STREAM_JUMP, STREAM_RC]
                    .map(|s| pending(&bufs[s], pos[s], filled[s], s));
                let mut streams = Bcj2DecStreams::new(main, call, jump, rc, &mut dest[written..]);
                let before = streams.bufs.map(<[u8]>::len);
                decoder
                    .decode(&mut streams)
                    .map_err(|_| error_invalid_data("bcj2 decode error"))?;
                let after = streams.bufs.map(<[u8]>::len);
                for stream in 0..NUM_STREAMS {
                    pos[stream] += before[stream] - after[stream];
                }
                streams.dest_pos
            };
            written += produced;
            *remaining -= produced as u64;
            if written == dest.len() {
                break;
            }

            // The decoder stopped short of the window, so a stream it needs
            // is dry: an empty buffer, or a word stream holding fewer than
            // the four bytes it is read in. A stream that still has a whole
            // read's worth was not the one that stopped it, so it is left.
            let mut refilled = false;
            for stream in 0..NUM_STREAMS {
                if eof[stream]
                    || !pending(&bufs[stream], pos[stream], filled[stream], stream).is_empty()
                {
                    continue;
                }
                // Move what is left — at most the three bytes of a partial
                // word — to the front and read on top of it. The window is
                // allocated once and never resized: a stream that is handed
                // over a few bytes at a time is refilled once per few bytes,
                // and growing a buffer back to its full length each time
                // would spend the whole read zeroing it.
                bufs[stream].copy_within(pos[stream]..filled[stream], 0);
                filled[stream] -= pos[stream];
                pos[stream] = 0;
                let n = inputs[stream].read(&mut bufs[stream][filled[stream]..])?;
                filled[stream] += n;
                if n == 0 {
                    eof[stream] = true;
                } else {
                    refilled = true;
                }
            }
            if !refilled {
                break;
            }
        }

        if *remaining == 0 && !decoder.is_maybe_finished_code() {
            return Err(error_invalid_data(
                "bcj2 decode error: range coder did not end",
            ));
        }
        if written == 0 && *remaining > 0 {
            return Err(error_invalid_data(
                "bcj2 decode error: streams ended before the recorded size",
            ));
        }
        Ok(written)
    }
}

/// How much of each stream one encoder call may write before its window is
/// handed on to the stream's sink. A multiple of four, as `Bcj2.h` asks of
/// the call and jump windows.
#[cfg(feature = "compress")]
const ENC_WINDOW: usize = 1 << 16;

/// What a [`Bcj2Writer`] hands back when it is finished: the main stream's
/// sink, the call and jump streams' sinks, the raw range-coded stream, and
/// how many bytes went into the call and jump sinks.
#[cfg(feature = "compress")]
pub(crate) struct Bcj2Finished<W, S> {
    pub(crate) main: W,
    pub(crate) call: S,
    pub(crate) jump: S,
    pub(crate) rc: Vec<u8>,
    pub(crate) call_len: u64,
    pub(crate) jump_len: u64,
}

/// BCJ2, written: the `Write` around `lzma_turbo::filters::bcj2::Bcj2Enc`.
///
/// Whatever is written is split into the four streams as it arrives. The
/// main, call and jump streams go straight on to their sinks — in a 7z
/// folder, each is the input of its own coder — and the range-coded stream,
/// which 7-Zip stores without a coder, is kept here until
/// [`finish`](Self::finish).
///
/// The encoder runs with `Bcj2Enc_Init`'s settings: virtual address zero, the
/// default relative limit and no file-size limit. 7-Zip's own encoder also
/// narrows the limit to each member's size as the block crosses file
/// boundaries (`Bcj2Coder.cpp`), which only changes which branches are
/// converted, never what a decoder makes of them.
#[cfg(feature = "compress")]
pub(crate) struct Bcj2Writer<W, S> {
    encoder: Bcj2Enc,
    main: W,
    call: S,
    jump: S,
    rc: Vec<u8>,
    call_len: u64,
    jump_len: u64,
    windows: [Box<[u8]>; NUM_STREAMS],
}

#[cfg(feature = "compress")]
impl<W: std::io::Write, S: std::io::Write> Bcj2Writer<W, S> {
    pub(crate) fn new(main: W, call: S, jump: S) -> Self {
        Self {
            encoder: Bcj2Enc::new(),
            main,
            call,
            jump,
            rc: Vec::new(),
            call_len: 0,
            jump_len: 0,
            windows: std::array::from_fn(|_| vec![0u8; ENC_WINDOW].into_boxed_slice()),
        }
    }

    /// Runs the encoder over `src` until it has taken all of it — or, when
    /// `finishing`, until the range coder is flushed — handing each window on
    /// to its sink after every call.
    fn pump(&mut self, src: &[u8], finishing: bool) -> std::io::Result<()> {
        let mut src_pos = 0;
        loop {
            let pos = {
                let [main, call, jump, rc] = &mut self.windows;
                let mut out = Bcj2EncOut::new(main, call, jump, rc);
                self.encoder.encode(&mut out, src, &mut src_pos);
                out.pos
            };
            self.main
                .write_all(&self.windows[STREAM_MAIN][..pos[STREAM_MAIN]])?;
            self.call
                .write_all(&self.windows[STREAM_CALL][..pos[STREAM_CALL]])?;
            self.jump
                .write_all(&self.windows[STREAM_JUMP][..pos[STREAM_JUMP]])?;
            self.rc
                .extend_from_slice(&self.windows[STREAM_RC][..pos[STREAM_RC]]);
            self.call_len += pos[STREAM_CALL] as u64;
            self.jump_len += pos[STREAM_JUMP] as u64;

            if finishing && self.encoder.is_finished() {
                return Ok(());
            }
            if self.encoder.full_stream().is_some() {
                // A window filled up; it has been drained, so go again.
                continue;
            }
            if finishing {
                // Under `EndStream` the encoder stops only for a full window
                // or a flushed range coder; anything else would loop forever.
                return Err(std::io::Error::other(
                    "bcj2 encoder stopped before flushing its range coder",
                ));
            }
            // It wants more source, and it has taken all of this: what it
            // could not decide about yet is held back inside it.
            debug_assert_eq!(src_pos, src.len());
            return Ok(());
        }
    }

    /// Flushes the encoder and hands back the sinks and the range-coded
    /// stream. The sinks are not finished: that is the caller's.
    pub(crate) fn finish(mut self) -> std::io::Result<Bcj2Finished<W, S>> {
        self.encoder.set_finish_mode(Bcj2EncFinishMode::EndStream);
        self.pump(&[], true)?;
        Ok(Bcj2Finished {
            main: self.main,
            call: self.call,
            jump: self.jump,
            rc: self.rc,
            call_len: self.call_len,
            jump_len: self.jump_len,
        })
    }
}

#[cfg(feature = "compress")]
impl<W: std::io::Write, S: std::io::Write> std::io::Write for Bcj2Writer<W, S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !buf.is_empty() {
            self.pump(buf, false)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // The encoder holds back up to four bytes it cannot decide about
        // until it sees what follows; those are not flushed here, only by
        // `finish`, because writing them out now would change the stream.
        self.main.flush()?;
        self.call.flush()?;
        self.jump.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use lzma_turbo::filters::bcj2::{Bcj2Enc, Bcj2EncFinishMode, Bcj2EncOut};

    use super::*;

    /// Bytes with the branch opcodes BCJ2 converts, dense enough that a
    /// megabyte of them puts more than `BUF_SIZE` on the call stream.
    fn pseudo_x86(len: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut out: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        // A branch every seven bytes, its offset small enough to be within
        // the encoder's relative limit so it is actually converted. Four in
        // five are calls and the fifth a jump, so both of the word streams
        // are used and the call one is the long one.
        let mut i = 0;
        let mut n = 0;
        while i + 5 <= out.len() {
            out[i] = if n % 5 == 4 { 0xE9 } else { 0xE8 };
            out[i + 1..i + 5].copy_from_slice(&((i as u32 * 7) % 0x10000).to_le_bytes());
            i += 7;
            n += 1;
        }
        out
    }

    /// The four streams `lzma-turbo`'s encoder makes of `data`, in one call
    /// with windows large enough to hold them.
    fn encode(data: &[u8]) -> [Vec<u8>; NUM_STREAMS] {
        let mut enc = Bcj2Enc::new();
        enc.set_finish_mode(Bcj2EncFinishMode::EndStream);
        let mut bufs: [Vec<u8>; NUM_STREAMS] = std::array::from_fn(|_| vec![0u8; data.len() + 64]);
        let [m, c, j, r] = &mut bufs;
        let mut out = Bcj2EncOut::new(m, c, j, r);
        let mut src_pos = 0;
        enc.encode(&mut out, data, &mut src_pos);
        assert!(enc.full_stream().is_none() && enc.is_finished());
        let pos = out.pos;
        for (buf, len) in bufs.iter_mut().zip(pos) {
            buf.truncate(len);
        }
        bufs
    }

    /// A reader that hands out at most `step` bytes per call, so the reader
    /// under test sees partial words on the call and jump streams.
    struct Trickle {
        data: Vec<u8>,
        at: usize,
        step: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.step).min(self.data.len() - self.at);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    #[test]
    fn trickled_streams_and_small_windows_round_trip() {
        // A megabyte in one read and in 4 KiB windows, for the streams that
        // outrun `BUF_SIZE` and have to be refilled; then a small one read a
        // byte at a time from inputs that hand over three bytes at a time,
        // for the partial words that leaves on the call and jump streams.
        for (len, step, window) in [
            (1usize << 20, usize::MAX, 1usize << 20),
            (1 << 20, 5, 4096),
            (48 * 1024, 3, 1),
        ] {
            let data = pseudo_x86(len);
            let streams = encode(&data);
            if len >= 1 << 20 {
                assert!(
                    streams[STREAM_CALL].len() > BUF_SIZE,
                    "the call stream must need refilling"
                );
            }
            let inputs = streams
                .into_iter()
                .map(|data| Trickle { data, at: 0, step })
                .collect();
            let mut reader = Bcj2Reader::new(inputs, data.len() as u64);
            let mut out = Vec::with_capacity(data.len());
            let mut buf = vec![0u8; window];
            loop {
                let n = reader.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            assert!(out == data, "len {len} step {step} window {window}");
        }
    }

    /// The writer, fed in pieces of every awkward size, makes exactly the
    /// streams the encoder makes of the whole buffer in one call - so where
    /// the archive writer's reads happen to split the data cannot change a
    /// byte - and the reader turns them back into the input.
    #[cfg(feature = "compress")]
    #[test]
    fn the_writer_matches_a_one_shot_encode_however_it_is_fed() {
        use std::io::Write;

        for len in [0usize, 1, 4, 5, 6, 7, 64 * 1024, 300 * 1024 + 3] {
            let data = pseudo_x86(len);
            let whole = encode(&data);
            for step in [1usize, 3, 4, 5, 4096, usize::MAX] {
                let mut writer = Bcj2Writer::new(Vec::new(), Vec::new(), Vec::new());
                for piece in data.chunks(step.min(data.len().max(1))) {
                    writer.write_all(piece).unwrap();
                }
                let done = writer.finish().unwrap();
                assert_eq!(done.call_len, done.call.len() as u64);
                assert_eq!(done.jump_len, done.jump.len() as u64);
                let streams = [done.main, done.call, done.jump, done.rc];
                assert!(streams == whole, "len {len} step {step}");
            }
            if len >= 64 * 1024 {
                assert!(!whole[STREAM_CALL].is_empty() && !whole[STREAM_JUMP].is_empty());
            }
            let inputs = whole
                .into_iter()
                .map(|data| Trickle {
                    data,
                    at: 0,
                    step: usize::MAX,
                })
                .collect();
            let mut out = Vec::new();
            Bcj2Reader::new(inputs, data.len() as u64)
                .read_to_end(&mut out)
                .unwrap();
            assert!(out == data, "len {len}");
        }
    }

    #[test]
    fn a_short_stream_is_an_error_not_a_short_read() {
        let data = pseudo_x86(64 * 1024);
        let mut streams = encode(&data);
        streams[STREAM_MAIN].truncate(streams[STREAM_MAIN].len() / 2);
        let inputs = streams
            .into_iter()
            .map(|data| Trickle {
                data,
                at: 0,
                step: usize::MAX,
            })
            .collect();
        let mut reader = Bcj2Reader::new(inputs, data.len() as u64);
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
    }
}
