# Contributing to RhizoDB

Thanks for your interest in RhizoDB. One person maintains it in their spare time, so replies can take a few days.

## Before you start

- To report a bug or ask for a feature, open an issue with one of the issue forms.
- To make a large change, open an issue first and describe the change. A short talk first saves you from work that does not fit the design.
- Write issues, pull requests, code, docs, and commit messages in English.

The design decisions are in [`docs/`](../docs/README.md). Each document records the decisions for one part of RhizoDB. If your change goes against a decision there, say so in the pull request and update the document in the same pull request.

## Set up

RhizoDB uses [mise](https://mise.jdx.dev) to pin the Rust toolchain and the check tools.

1. Install mise.
2. In the repository, install the pinned tools. This also turns on the Git hooks in `.githooks`.

	```bash
	mise install
	```

The `commit-msg` hook checks the commit subject, the `pre-commit` hook runs the fast checks, and the `pre-push` hook runs every check.

## Run the checks

```bash
mise run check
```

This runs `rustfmt`, the `ast-grep` rules in `guards/`, the `xtask` checks, `cargo deny`, `clippy`, and the tests, including the openCypher TCK and the crash test. CI runs the same command on Linux, macOS, and Windows, checks that the crate packages, and checks every commit in the pull request.

`mise run differential` compares query results with LadybugDB. It needs CMake, a C++ compiler, and OpenSSL, and runs in its own CI workflow, so you can leave it to CI.

The checks enforce these rules, so you do not need to remember them:

- Code has no comments. Doc comments are only for public API contracts.
- Unit tests live in a sibling `<name>.test.rs` file.
- Files in the repository do not use en dashes, em dashes, middle dots, or CJK angle and corner brackets.
- Each file in `docs/` follows the template in `docs/CLAUDE.md`. After you add or rename a document, run `mise run docs` to rebuild the index.

## Deeper test tiers

| Command | What it runs | When |
| --- | --- | --- |
| `mise run storage-deep` | Storage, engine, crash, dump, node meta, and property column tests at 100 times the random cases | Changes to storage, the graph layer, or the dump format |
| `mise run random-deep` | Isolation history check, kill-and-reopen, random transaction model, damaged files, concurrency, resource limits, killed backups, and snapshot stress, at 100 times the cases | Changes to transactions, durability, or concurrency |
| `mise run engine-suite` | Every `rhizodb` test with RhizoDB's own engine in place of redb | Changes to the storage trait or the engine |
| `mise run fuzz 600` | Every fuzz target for the given seconds each; needs nightly Rust and cargo-fuzz | Changes to the parser, decoders, or file handling |

The nightly workflow runs these tiers and the fuzzers. A failing random case is checked in so every later run replays it.

## Write commits

- Write each commit subject as a [Conventional Commit](https://www.conventionalcommits.org), for example `fix(parser): reject a trailing comma in a property map`.
- A `feat` commit, or any commit marked breaking with `!`, adds a line under `Unreleased` in `CHANGELOG.md`.
- A change to the query results that LadybugDB disagrees with updates `crates/differential/differences.txt` with the reason.

## Open a pull request

- Keep a pull request to one change. Several small pull requests are easier to review than one large one.
- Fill in the pull request template. Say how you checked the change, and add a TCK or GQL scenario for new query behaviour.
- A pull request merges only when every check passes.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as in the [README](../README.md#license), without any additional terms or conditions.
