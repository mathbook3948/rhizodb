# Check tiers

Decides which tests run on pull requests, on every push to `main`, nightly, and before a release.

## Background

The randomized, crash, and stress tests took most of the time of the check that every pull request and every push waits for. Pull requests are reviewed statically in full, so a merge does not need to wait for tests that sample random cases.

## Decisions

- Every pull request runs `mise run check` on Linux, macOS, and Windows, and merges only when it passes.
- `mise run check` runs the formatting, guard, license, and lint checks, every test not marked ignored, the doctests, the TCK, and the examples, and it compiles every test.
- A test is marked ignored, with the reason that `mise run random` runs it, when it draws its cases from proptest or a random seed, scales its cases with `RHIZODB_TEST_SCALE`, fails or crashes at every point of an operation, kills a process, races threads to stress them, asserts on elapsed time, or measures heap growth.
- A deterministic test stays in `mise run check` however long it takes.
- `mise run random` runs every ignored test outside the TCK crate except the engine benchmarks, at the case counts that a scale of one gives.
- The child process entry points of the kill tests are ignored too, and they do nothing without the environment variable their parent sets.
- Every push to `main` runs `mise run random` on Linux, macOS, and Windows.
- The nightly workflow runs the ignored storage and integration tests at a hundred times the cases, beside the soak, advisory, and fuzz jobs.
- A failed run of the random or nightly workflow on `main` opens an issue labelled `bug` for each failing test, or comments on the open issue of the same title, with the seed it ran and a link to the run.
- An issue title names the test binary with the test, and more than five failing tests in one run share one issue whose title names no count.
- A test binary that exits without naming a failed test, as a crash or abort does, gets an issue of its own, and a failed job whose log names no test binary at all opens an issue for the job.
- An issue body is cut to fit GitHub's limit, and it says how much output it left out.
- A failure found on `main` is fixed in a new pull request.
- Nothing is published unless a `v*` tag is pushed, and a release runs `mise run random` before it publishes.

## Open

None
