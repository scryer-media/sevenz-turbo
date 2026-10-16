# Contributing

Thanks for looking. A few things before you open a pull request:

- Read [AGENTS.md](../AGENTS.md); the fork rules there apply to humans too.
- This is a **fork** of [sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2).
  A fix that is not about the LZMA/LZMA2 codec swap or the added container API
  belongs upstream first; we rebase onto upstream and would rather carry your
  fix as an upstream commit than as fork divergence.
- Keep the diff against upstream confined to `src/decoder.rs`, `src/reader.rs`
  and new files wherever you can. `AGENTS.md` explains why and how to rebase.
- Run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`
  and `cargo nextest run --all-features --no-fail-fast`
  and `cargo test --doc --all-features --no-fail-fast` locally.
- Record every divergence from upstream in the "Fork" section of
  [CHANGELOG.md](../CHANGELOG.md), in the same change.
- Enable the hooks once per clone: `git config core.hooksPath .githooks`.

Install the native test runner with `cargo install cargo-nextest --locked --version 0.9.143`.
Nextest runs unit and integration tests; Cargo runs doctests, and Miri uses its own interpreter.

Changes to native allocation, ownership or assembly paths must preserve the
focused [unsafe validation lanes](../docs/unsafe-validation.md). Use the
pinned SDK oracle when changing archive parsing or member iteration.
