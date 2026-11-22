# Releases

Decides how a version of `rhizodb` is cut and published.

## Background

The crate is developed and released from its own repository, and hosts depend on it through crates.io. A release must be the exact code that passed every check, so publishing is automated from a tag.

## Decisions

- Only the `rhizodb` crate is published.
- Versions follow Semantic Versioning, and they stay below 1.0 until the public API is declared stable.
- Below 1.0, a breaking change to the public API or the on-disk format raises the minor version.
- A release is a tag `v<version>` on `main` whose version equals the crate version in `Cargo.toml`.
- Pushing such a tag runs the full check on every supported platform, then publishes the crate to crates.io and creates a GitHub release.
- A release that stopped partway finishes when its workflow is run again, skipping the steps that already happened.
- `CHANGELOG.md` keeps an `Unreleased` section, and each user-visible change adds a line to it in the same pull request.
- Cutting a release renames `Unreleased` to the new version, and that section becomes the GitHub release notes.
- Every pull request checks that the crate packages and builds from its packaged files alone.
- The published crate carries both license texts.

## Open

None
