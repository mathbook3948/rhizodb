# Load testing

Decides how RhizoDB is measured under concurrent and sustained load, beside the single-client workload suite.

## Background

The workload suite times one client at a time, which hides lock contention, oversubscribed cores, latency spikes, and growth that only shows over minutes of writes. A host that serves many requests needs those numbers before it trusts RhizoDB in production.

## Decisions

- Load scenarios live in the benchmark crate and run with `--load`, in place of the workload suite, on the same seeded graph.
- `--load` takes `readers`, `mixed`, `sustained`, a comma-separated list of them, or `all`.
- The readers scenario runs reader threads alone on one shared database, once for each count in `--threads`, which defaults to 1, 2, 4, and 8.
- The mixed scenario runs the same readers beside one more thread that commits small writes without pause.
- The sustained scenario runs the writer alone, for runs of minutes.
- `--seconds` sets how long each run lasts, and the clock covers only the run, not loading or opening.
- The read mix is 60% point lookups by property index, 30% 2-hop distinct traversals, and 10% filtered scans of every person.
- Every reader draws its reads from its own seeded sequence, so a run is repeatable.
- A write either creates a person without an age with one `KNOWS` edge out of it, or renames a person the writer created, one in four.
- Writes never change an answer that a read checks, so every read is compared with the answer computed from the dataset while the writer runs.
- After a run, the writer's nodes, names, and edges are read back and compared with what it committed, and the sustained run checks them again after closing and reopening.
- A wrong answer or an error fails the run, and the report counts both.
- A mixed run also fails when RhizoDB's median commits per second beside any count of readers fall below half of its median beside one reader, which shows readers starving the writer.
- Each run starts from a fresh copy of the loaded graph and warms up the read mix for half a second before the clock starts.
- Repetitions alternate which engine goes first, and the report shows medians over repetitions.
- Every run reports throughput and the p50, p99, and maximum latency of reads and of commits.
- A run also reports the longest time no reader finished a read and the reads slower than 100 ms, which show stalls.
- A sampler records the log's bytes, the other files' bytes, and the process's resident memory every 100 ms.
- A second in which the log shrank counts as a checkpoint, and reads in those seconds get their own p99.
- The JSON output keeps every run with a per-second timeline of latency, throughput, file sizes, and memory.
- RhizoDB and LadybugDB run by default, each through its own sessions, one per thread.
- The tests run every scenario at tiny scale on both engines for under a second each and fail on any wrong answer.

## Open

- How to measure a graph larger than the machine's memory, which takes longer to load than a session allows.
