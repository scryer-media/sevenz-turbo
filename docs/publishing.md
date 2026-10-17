# Publishing

Releases are published by `.github/workflows/release.yml`, started one of two
ways:

- **From GitHub Actions** (the usual way): run the `release` workflow on
  `main`. It releases the version in `Cargo.toml`; nothing is typed in. No
  local signing or tagging is involved; the workflow creates the tag. See
  "Each release" below.
- **From a machine**, with `cargo xtask release`, which pushes a signed tag
  that starts the workflow. The task (`xtask/src/main.rs`, plain Rust with no
  dependencies, so it runs wherever `cargo` does) runs every check the
  workflow runs, so a tag that reaches GitHub is one the workflow will accept.

`sevenz-turbo` is published after the `lzma-turbo` version its lockfile was
tested with, never before: the manifest names a caret floor on `lzma-turbo`
(the oldest version this crate needs, not a pin) and reaches it through
crates.io, `Cargo.lock` fixes the exact version CI built with `--locked`, and
the release task refuses to tag while a `path` is still on that dependency.

## One-time setup, before the first release

1. **Publish the first version from a machine with a crates.io token.**
   crates.io only lets a workflow publish a crate that already exists, so the
   first release is cut with

   ```sh
   cargo xtask release --publish
   ```

   which runs the checks, creates the signed tag, runs `cargo publish` and
   then pushes the tag. The workflow sees the version on crates.io, skips its
   own upload and creates the GitHub release.
2. **Turn on trusted publishing** at
   `https://crates.io/crates/sevenz-turbo/settings/new-trusted-publisher`:
   repository owner `scryer-media`, repository `sevenz-turbo`, workflow
   `release.yml`, environment `crates-io`.
3. **Create the `crates-io` environment** in the GitHub repository settings.
   If it is restricted to refs, allow both the branch `main` (dispatched
   releases) and the tags `v*` (pushed tags). No secrets are needed, the
   workflow gets a short-lived token from crates.io through OIDC.

The release workflow needs `contents: write` to create the GitHub release,
which it requests for that job only.

## Each release

1. Make sure the `lzma-turbo` version named in `Cargo.toml` is on crates.io,
   and that the dependency line carries no `path`. While the two crates are
   developed side by side the line may carry `version` and `path` together;
   `cargo` uses the sibling checkout and `cargo publish` would use the
   version, but CI has no sibling checkout, so the path is dropped before
   the release commit.
2. On `main`:
   - set `version` in `Cargo.toml` and refresh `Cargo.lock`;
   - make sure `CHANGELOG.md` has a `## <version>` heading that is not marked
     unreleased (the `## Fork` section stays where it is; it is the record of
     the divergence from sevenz-rust2, not a release note);
   - commit, signed, and push. Wait for `ci` to go green on that commit.
3. Actions → `release` → Run workflow, branch `main`, `dry_run` left on. The
   run releases the version in `Cargo.toml`. It refuses a branch other than
   `main`, a version below the newest on crates.io, a changelog section
   missing or still marked unreleased, a `path` on `lzma-turbo`, a tag
   `v<version>` at another commit, a commit whose `ci-complete` is not green,
   a public API change larger than the version bump (`cargo semver-checks`),
   and a crate that does not package.
4. Run it again with `dry_run` off. It publishes to crates.io, then creates
   the tag `v<version>` at the commit it checked and the GitHub release with
   the changelog section as its notes. A run that fails after the upload can
   be rerun: a version already on crates.io, a tag already at that commit and
   a release that already exists are all taken as done.

From a machine instead, after step 2: `cargo xtask release --dry-run`, then
`cargo xtask release`. The dry run
works on any branch and on a dirty tree, and lists everything a real run
would refuse rather than stopping at the first. A real run refuses a dirty
tree, an unsigned HEAD, a branch other than `main`, a changelog section
missing or still marked unreleased, a README that shows neither the version
nor its major.minor, a remaining `path` on `lzma-turbo`, and a tag that
already exists. It then runs the tests in release mode (`--skip-tests`
leaves them to CI and to the workflow's `verify` job) and a
`cargo publish --dry-run`, creates the signed tag `v<version>` and pushes
it.

The workflow then verifies the tag against the manifest, tests, publishes to
crates.io and creates the GitHub release with the changelog section as its
notes.
