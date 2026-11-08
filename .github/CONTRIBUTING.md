# Contributing

Thanks for looking. A few things before you open a pull request:

- Read [AGENTS.md](../AGENTS.md) and
  [docs/design-rules.md](../docs/design-rules.md); the rules there apply to
  humans too.
- This is a hard **fork** of
  [sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2). It is not
  rebased onto upstream and nothing is sent back, so a fix lands here on its
  own terms.
- Run `cargo fmt --all`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  `cargo clippy --locked --all-targets --no-default-features -- -D warnings`,
  `cargo nextest run --locked --all-features --no-fail-fast`
  and `cargo test --doc --locked --all-features --no-fail-fast` locally.
  For a change to BZip2 code, also run
  `cargo clippy --locked --all-targets --no-default-features --features aes256,native-crypto,bzip2-permissive,compress,deflate,ppmd,util -- -D warnings`:
  under `--all-features` the faster `bzip2-turbo` is the one selected.
- Record every change under its version heading in
  [CHANGELOG.md](../CHANGELOG.md), in the same change. The `## Fork`
  section is closed and takes no new entries.
- Enable the hooks once per clone: `git config core.hooksPath .githooks`.

Install the native test runner with `cargo install cargo-nextest --locked --version 0.9.143`.
Nextest runs unit and integration tests; Cargo runs doctests, and Miri uses its own interpreter.

Changes to native allocation, ownership or assembly paths must preserve the
focused [unsafe validation lanes](../docs/unsafe-validation.md). Use the
pinned SDK oracle when changing archive parsing or member iteration.
