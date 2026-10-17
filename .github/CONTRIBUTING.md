# Contributing

Thanks for looking. A few things before you open a pull request:

- Read [AGENTS.md](../AGENTS.md); the fork rules there apply to humans too.
- This is a hard **fork** of
  [sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2). It is not
  rebased onto upstream and nothing is sent back, so a fix lands here on its
  own terms.
- Run `cargo fmt --all`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  `cargo clippy --locked --all-targets --no-default-features -- -D warnings`,
  `cargo nextest run --locked --all-features --no-fail-fast`
  and `cargo test --doc --locked --all-features --no-fail-fast` locally.
- Record every divergence from upstream in the "Fork" section of
  [CHANGELOG.md](../CHANGELOG.md), in the same change.
- Enable the hooks once per clone: `git config core.hooksPath .githooks`.

Install the native test runner with `cargo install cargo-nextest --locked --version 0.9.143`.
Nextest runs unit and integration tests; Cargo runs doctests, and Miri uses its own interpreter.

Changes to native allocation, ownership or assembly paths must preserve the
focused [unsafe validation lanes](../docs/unsafe-validation.md). Use the
pinned SDK oracle when changing archive parsing or member iteration.
