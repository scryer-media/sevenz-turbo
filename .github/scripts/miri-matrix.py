#!/usr/bin/env python3
"""Turn the live Nextest selection into one Miri job per portable unsafe test."""

import json
import sys
from pathlib import Path

listing = json.loads(Path(sys.argv[1]).read_text())
tests = [
    name
    for suite in listing["rust-suites"].values()
    for name, case in suite["testcases"].items()
    if case["filter-match"]["status"] == "matches" and not case["ignored"]
]
required = {
    "advice_precedes_initialization_and_refusal_leaves_usable_owned_storage",
    "panicking_advice_releases_the_uninitialized_allocation",
    "aligned_storage_keeps_ownership_when_moved_between_threads",
    "small_unknown_and_non_lzma_inputs_keep_the_existing_memory_bound",
    "a_lent_window_is_the_callers_memory_and_is_freed_once",
    "a_refusal_goes_to_lzma_turbo_whole",
    "memory_that_does_not_fit_is_refused_and_dropped",
    "a_lent_arena_is_the_callers_memory_and_is_freed_once",
    "a_ppmd_refusal_is_kept_for_the_builder",
}
prefixes = ("writer::input_buffer::tests::", "window_memory::tests::")
if (
    len(tests) != len(set(tests))
    or not required.issubset({name.rsplit("::", 1)[-1] for name in tests})
    or any(not name.startswith(prefixes) for name in tests)
    or any("native_advice" in name for name in tests)
):
    raise SystemExit("Miri discovery lost required unsafe tests or selected native/unrelated tests")
with Path(sys.argv[2]).open("a") as output:
    output.write("matrix=" + json.dumps({"test": sorted(tests)}) + "\n")
print(f"Scheduled {len(tests)} separate Miri jobs")
