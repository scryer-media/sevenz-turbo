# Benchmark data archive

Raw benchmark measurements, machine-readable reports, validation logs and
profiling evidence are kept outside Git. Markdown reports and benchmark
harnesses remain in this repository.

The existing evidence was moved to the operator's local Documents directory
in `turbo-benchmark-data-2026-10-15.tar.gz`. The archive contains 476 original
files from both repositories, including 319 from `sevenz-turbo`. All files
were verified byte for byte by SHA-256 before their repository copies were
removed. The archive SHA-256 is:

```text
f20052e95c0be966bdb1c06ff0a8137b88fd0df2079afdeed8563aa9994b41f2
```

`MANIFEST.json` inside the archive records the source commit for each
repository and the original relative path, byte size and SHA-256 of every
file. Archive members use `<repository>/<original-path>`, for example
`sevenz-turbo/docs/`. Historical filenames and decoded hashes in the Markdown
reports continue to identify these archived files; they no longer refer to
files shipped in this checkout. Compressed JSON can be read with
`zstd -dc` after extracting the relevant member outside the repository.

Keep future raw results and profiling captures in an operator-owned directory
outside the checkout. Commit concise findings, methodology and source
identities in Markdown, not raw data or evidence bundles. Do not rewrite Git
history to remove older copies.
