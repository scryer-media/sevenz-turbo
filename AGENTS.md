# Agent and contributor rules for sevenz-turbo

These rules apply to every automated agent and every human contributor.

## What this repository is

A 7z archive library: a hard fork of
[sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2) whose LZMA and
LZMA2 coding is [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo),
and whose container API carries what a streaming consumer needs: memory
limits enforced before allocation, per-member CRCs, pack-stream byte ranges,
typed corruption errors and a per-block completion hook. Every 7z header is
treated as hostile input.

Read before changing code:

- `docs/design-rules.md`: the fork, codec seam, threading, CRC and
  hostile-archive rules, and the public API and fixture rules.
- `docs/security.md` and `SECURITY.md`: the limits and the threat model.
- `docs/unsafe-validation.md`: every unsafe surface and what checks it.
- `docs/benchmarking.md`: how speed is measured.
- `docs/failed-experiments.md`: what has been tried and did not pay. Do not
  retry an entry without new evidence, and add to it when an experiment fails.
- `docs/lzma-turbo-requests.md`: what this crate needs from `lzma-turbo`.

## Working on a task

- Carry a request through the edits, the checks that show it works, and the
  handoff. Settle routine, reversible choices from the code and docs rather
  than asking; ask when the answer changes correctness or what is authorised.
- Read the code you are changing, and its tests, before changing it. Extend
  what is there rather than adding a parallel path.
- Other agents and people work in the same checkouts. Inspect the working
  tree before editing, change only what the task needs, and treat someone
  else's uncommitted work or failing check as something to report, not fix.
- Keep plans, reports and handoffs out of the repository. Do not create,
  edit or delete Markdown files outside the task at hand.
- At handoff, say what changed, which checks ran and passed, which were not
  run, and what is still open. Do not claim a speed-up without a measurement.

## Correctness

- 7-Zip is the oracle. Archives this crate writes open in `7zz`, archives
  `7zz` writes decode here to the same bytes, and the differential matrix is
  what says so. A difference is a bug whatever the reason.
- No input panics. A malformed, truncated or hostile archive returns a typed
  error; it never panics, reads or writes out of bounds, or does work or
  allocates memory that its size and `ArchiveLimits` do not allow.
- Every `unsafe` block carries a `// SAFETY:` comment naming the invariant it
  relies on and the check that established it. Native guards, Memcheck and
  sanitizers cover FFI and assembly paths; Miri runs only portable ownership
  invariants, one discovered test per job. Do not suppress failures or
  broaden Miri to the full archive or codec suite. `docs/unsafe-validation.md`
  is updated in the same change.
- A new public entry point that reads archive bytes gets a fuzz target under
  `fuzz/` in the same change.

## Tests

A test gives the same result on a slow, loaded or shared runner as on an idle
workstation.

- Wait for the condition the assertion depends on, never for elapsed time.
  No fixed sleeps, no deadlines, no elapsed-time assertions, no timeouts as
  a stand-in for a condition.
- Do not assume one concurrent operation finishes before another unless the
  code guarantees that order.
- Never fix a flaky test with a longer timeout, a sleep or a retry. Find the
  race and remove it.
- Committed fixtures follow the corpus rule in `docs/design-rules.md`.

## Performance

- A claimed speed-up shows the median of interleaved rounds against the
  previous release, on the hardware it targets, with outputs compared before
  timing. A change to one path shows the others unchanged.
- Multi-threaded changes are measured at several thread counts, not only at
  the machine's full width, where some regressions are invisible.
- Raw benchmark data, machine-readable reports and profiling bundles stay
  outside the repository. Commit Markdown summaries and source identities;
  existing evidence is kept as `docs/benchmark-data.md` describes.

## Dependencies

- `Cargo.lock` is the source of truth: pass `--locked` to Cargo.
- A new library dependency is the maintainer's decision, not an agent's.
- Before upgrading dependencies, read `renovate.json` and honour every
  exception in it.

## Versions and pull requests

- Any code change to the crate bumps its version, `Cargo.lock` and
  `CHANGELOG.md` in the same change. The bump is what `cargo semver-checks`
  says it is: the release workflow runs it against the published version and
  refuses a patch bump that carries a breaking change. A documentation- or
  CI-only change says so instead.
- The version in a pull request is the latest one published on crates.io
  plus one increment, never more. An unpublished version heading is not a
  baseline: fold new work into it rather than bumping again.
- Releases go through the repository's release workflow
  (`docs/publishing.md`), never by hand.

## Repository hygiene

- Commits are SSH-signed. Never pass `--no-gpg-sign` or set
  `commit.gpgsign=false`; if signing fails, leave the change uncommitted.
- No `Co-Authored-By` or other attribution trailers in commit messages.
- Never run destructive git working-tree operations (`checkout --`,
  `restore`, `reset --hard`, `clean`, `stash`) in a shared checkout.
- Branch names use gitflow prefixes: `feature/…`, `bugfix/…`, `hotfix/…`.
  Worktrees go under `.worktrees/`.
- Agents do not push unless told to. The maintainer pushes.
- The pre-commit hook (`.githooks/pre-commit`) rejects home paths, the local
  username and secrets. Keep it enabled: `git config core.hooksPath .githooks`.
- Before a commit is proposed: `cargo fmt --all`, `cargo clippy --all-targets
  --all-features -- -D warnings`, `cargo nextest run --all-features
  --no-fail-fast` and `cargo test --doc --all-features --no-fail-fast`.
  Install `cargo-nextest` 0.9.143 as in CI, keep `cargo miri test` for
  interpreter checks, and exclude `wasm-conformance` from ordinary workspace
  sweeps; its own CI job runs it.

## Code review

Read `SECURITY.md` before reviewing, and judge findings against the threat
model it states.
