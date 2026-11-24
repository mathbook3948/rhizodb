# Contributing to RhizoDB

Thank you for helping. This file says how to set up, what every change must pass, and how commits and docs are written.

## Setup

The toolchain is pinned in `mise.toml`: Rust, ast-grep, and cargo-deny.

```sh
mise install
```

`mise install` also points git at `.githooks`, so the hooks below run on every commit and push. Run every cargo command through mise, for example `mise exec -- cargo test -p rhizodb`, so it uses the pinned toolchain.

## What every change passes

`mise run check` must pass before every commit. It runs `check-fast` (formatting, the ast-grep rules, the forbidden-character and docs guards, the hook guard, and license checks), then clippy with warnings denied and the whole workspace's tests.

The hooks run it for you and must never be bypassed.

- `pre-commit` runs `mise run check-fast`.
- `commit-msg` checks the commit message.
- `pre-push` runs `mise run check`.

## Test tiers

| Command | What it runs | When |
| --- | --- | --- |
| `mise run check` | Lints and every test at the case counts CI runs | Every commit |
| `mise run storage-deep` | Storage, engine, crash, dump, node meta, and property column tests at 100 times the random cases | Changes to storage, the graph layer, or the dump format |
| `mise run random-deep` | Isolation history check, kill-and-reopen, random transaction model, damaged files, concurrency, resource limits, killed backups, and shuttle schedules, at 100 times the cases | Changes to transactions, durability, or concurrency |
| `mise run engine-suite` | Every `rhizodb` test with RhizoDB's own engine in place of redb | Changes to the storage trait or the engine |
| `mise run differential` | The same queries on RhizoDB and LadybugDB, compared | Changes to query semantics; needs CMake and a C++ compiler |
| `mise run fuzz 600` | Every fuzz target for the given seconds each; needs nightly Rust and cargo-fuzz | Changes to the parser, decoders, or file handling |

The nightly workflow runs the deep tiers and the fuzzers. A failing random case is checked in so every later run replays it.

## Code

- Library code forbids `unsafe`, `unwrap`, `expect`, and `panic`.
- Library code has no comments. Doc comments are for the public API, and every public item has one with an example that runs as a doctest.
- One import per line.
- Code, docs, and commit messages are in English.

## Commits

A commit subject is `<type>(<scope>)?: <description>`, with a type out of `feat`, `fix`, `chore`, `docs`, `refactor`, `test`, `perf`, `build`, `ci`, `style`, and `revert`. A scope is lowercase letters, digits, and hyphens. A `!` before the colon marks a breaking change.

A `feat` commit or a breaking commit adds a line under `Unreleased` in `CHANGELOG.md`, and the commit-msg hook refuses one that does not. A breaking change goes under `Changed`.

The commit body says why the change is made, what it decides, and how it was verified.

## Docs

Design decisions live in `docs/`, indexed in `docs/README.md`. `docs/CLAUDE.md` holds their rules: one decision per line, no measurements or version numbers, and `mise run docs` after adding a doc or changing a title or summary. The guide for hosts is the crate documentation in `crates/rhizodb/src/guide.md`.

## Reporting

Report bugs and request features through the issue templates. Report a security problem as `SECURITY.md` describes, never in a public issue.

## License

By contributing, you agree that your contribution is licensed under both the MIT license and the Apache License 2.0, as the README states.
