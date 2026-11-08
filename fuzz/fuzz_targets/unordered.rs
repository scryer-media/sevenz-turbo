//! The unordered walk, over arbitrary header bytes.
//!
//! The same bytes as the `header` target, read through
//! `for_each_entries_unordered` with a positional source and four threads, so
//! the plan that decides which folders decode side by side, and how the memory
//! limit is shared among them, is sized by numbers the fuzzer chooses: no
//! panic, no allocation above the ceiling, no run that does not end.
#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;

#[global_allocator]
static ALLOC: common::CountingAllocator = common::CountingAllocator;

fuzz_target!(|data: &[u8]| {
    common::exercise_unordered(common::wrap(data));
});
