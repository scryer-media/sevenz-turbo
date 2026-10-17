---
name: open-pr
description: Open, push to, or report on a pull request in sevenz-turbo. Runs the preflight a mergeable PR needs (one PR per version, the version bump, a dated changelog heading, README dependency lines, the lzma-turbo dependency and publish order, lockfile, fmt, clippy, tests, semver-checks, package) and writes the PR body. Use whenever a PR is to be opened, pushed, updated or called ready.
---

# Opening a pull request in sevenz-turbo

Write every PR as if it were already released. The moment it merges, `main`
must release by dispatching the `release` workflow with nothing else to edit:
the workflow releases the version in `Cargo.toml` and refuses anything below.
Each step here exists because skipping it has already failed a release.

## 1. One PR per version

- `gh pr list --repo scryer-media/sevenz-turbo --state open`. If a PR for the
  next version is open, this work goes onto its branch as more commits. Never
  open a second PR for the same version, and never open a PR whose base is
  another PR's branch.
- Branches use `feature/…`, `bugfix/…` or `hotfix/…`, cut from `main`, in a
  worktree under `.worktrees/`.
- Open the PR only when the maintainer asked for one. Agents do not push
  otherwise.

## 2. The version

- Published is what crates.io says, nothing else: not a changelog heading, not
  a tag, not a review bot.
  `curl -s https://crates.io/api/v1/crates/sevenz-turbo | jq -r .crate.max_version`
- The PR's version is published plus one step. If `main` already carries an
  unpublished version above published, use it; never bump twice before a
  release.
- Which step: run `cargo semver-checks -p sevenz-turbo` against the published
  baseline. A breaking change on 0.x is a minor bump (0.28.0 → 0.29.0);
  anything else is a patch bump. Every public enum stays `#[non_exhaustive]`.
- Any code change needs the bump. Set `version` in `Cargo.toml`, then
  `cargo update -p sevenz-turbo --offline` and confirm
  `cargo metadata --locked --format-version 1 >/dev/null` passes.

## 3. The lzma-turbo dependency

- At merge the `lzma-turbo` line carries a version only: no `path`, and no
  `[patch.crates-io]` entry for it or for any other crate. The `release`
  workflow refuses a `path`, and `cargo package` fails on a version crates.io
  does not have yet.
- The version named is one crates.io already has:
  `curl -s https://crates.io/api/v1/crates/lzma-turbo | jq -r .crate.max_version`.
- When this PR needs an lzma-turbo release that has not happened, a git
  `[patch.crates-io]` pin may carry the branch meanwhile. Say so under
  **Merge order** in the body, expect `package` and `ci-complete` to fail
  until lzma-turbo publishes, then remove the pin, refresh `Cargo.lock` and
  rerun CI. The PR is not ready while the pin is there.

## 4. Changelog

- The section heading is `## <version> - <YYYY-MM-DD>`, dated the day the PR
  is opened. Never `Unreleased`: the release workflow refuses a heading that
  says it.
- `grep -n -i unreleased CHANGELOG.md` must print nothing.
- Every change in the PR has a bullet under that one heading. A later commit on
  the same PR adds bullets; it does not add a heading.
- The `## Fork` section stays at the top. A change that widens the difference
  from sevenz-rust2 is noted there in the same commit.

## 5. README

- Dependency lines name the new major.minor:
  `grep -n 'sevenz-turbo = \|version = "' README.md`.
- A new or renamed feature, default, limit or public type is reflected in the
  README and in `docs/` in the same PR. A new `ArchiveLimits` field is also a
  row in the table in `docs/security.md`.

## 6. Local gates

Run what CI runs, with `--locked`, before pushing:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo clippy --locked --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo nextest run --locked --all-features --no-fail-fast
cargo test --doc --locked --all-features --no-fail-fast
cargo nextest run --locked --no-default-features --no-fail-fast
cargo semver-checks -p sevenz-turbo
cargo package --locked
```

- A change behind a feature also runs the `rustcrypto` lane from the `test`
  job in `.github/workflows/ci.yml`.
- Never add `wasm-conformance` to a workspace-wide sweep; its own CI job runs
  it.
- A change to `unsafe` code runs its selectors from
  `docs/unsafe-validation.md`, and that inventory is kept current.
- Keep the pre-commit hook on: `git config core.hooksPath .githooks`.

## 7. Commit

- Every commit is SSH-signed. If signing fails, stop and leave the change
  staged; never `--no-gpg-sign`.
- No `Co-Authored-By` or other attribution trailers.
- Conventional subject: `feat(scope): …`, `fix(scope): …`, `!` for breaking.

## 8. PR title and body

Title: the version and what it does, for example
`0.29.0: keep a block error's cause typed`.

Body sections, in this order, leaving out any that would be empty:

- **What**: one paragraph.
- **Why**: the problem it fixes or the consumer that needs it.
- **Changes**: grouped by area.
- **Breaking changes**: what a consumer must change, if anything.
- **Merge order**: the lzma-turbo (or other) release this waits on.
- **Verification**: each command from step 6 and its result.
- **Follow-ups**: work left for later. Follow-ups live in the body; do not
  open GitHub issues without the maintainer's approval.

## 9. After every push

- Do not enable auto-merge unless the maintainer asks.
- Read the review comments:
  `gh api repos/scryer-media/sevenz-turbo/pulls/<n>/comments --paginate`.
  Fix each finding on this branch, or reply with the reason it is wrong, then
  resolve the thread.
- A code-scanning alert on the PR is fixed, or dismissed with a written reason
  when it is a false positive.
- The PR is ready only when every job, `ci-complete` included, is green on the
  head and every correctness, safety or version finding is fixed and replied
  to. Pure wording nits may be deferred with a reply.

## 10. After merge

`ci-complete` must be green on the merge commit on `main`. Then Actions →
`release` → Run workflow on `main`, `dry_run` on, then again with it off. See
`docs/publishing.md`.
