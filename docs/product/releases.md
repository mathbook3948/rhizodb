# Releases

Decides how a version of `rhizodb` is cut and published.

## Background

The crate is developed and released from its own repository, and hosts depend on it through crates.io. A release must be the exact code that passed every check, so publishing is automated from a tag.

## Decisions

- Only the `rhizodb` crate is published, and every other crate in the workspace sets `publish = false`.
- Versions follow Semantic Versioning, and they stay below 1.0 until the public API is declared stable.
- Below 1.0, a breaking change to the public API or the on-disk format raises the minor version.
- A release is a tag `v<version>` on `main` whose version equals the crate version in `Cargo.toml`.
- Pushing such a tag runs the full check, publishes the crate to crates.io, and creates a GitHub release.
- The workflow refuses a tag whose version does not match the crate version.
- `CHANGELOG.md` keeps an `Unreleased` section, and each user-visible change adds a line to it in the same pull request.
- Cutting a release renames `Unreleased` to the new version, and that section becomes the GitHub release notes.
- Every pull request checks that the crate packages and builds from its packaged files alone.
- The crate declares the Rust version the checks run on as its minimum supported version.

## Open

None
