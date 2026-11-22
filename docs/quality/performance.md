# Performance

Decides how RhizoDB's speed is measured and compared.

## Background

RhizoDB's speed is only known relative to something, and the numbers will decide which optimizations come next. A benchmark that favors one side or hides a wrong answer would send that work in the wrong direction.

## Decisions

- Benchmarks live in their own crate outside the main workspace, beside the differential crate, and run with `mise run bench`.
- LadybugDB is the first engine compared with, built from the same pinned source as in the differential crate.
- Every engine gets one seeded generated graph that is neutral to any query language, with a CSV export for engines whose bulk loader reads files.
- The graph has persons with an optional age, a text bio, and a 64-float embedding, cities, `KNOWS` edges with a few hubs, and `LIVES_IN` edges.
- Databases are files in a fresh temporary directory, never in memory, so disk costs count.
- The scale is chosen by name, from tiny for tests to large, and the operation counts per workload follow it.
- Each engine is also measured on its own best path: LadybugDB loads with `COPY FROM`, and RhizoDB loads in one write transaction with node handles from the creating query.
- The path both engines share, `UNWIND` with `CREATE`, is reported beside the tuned one as its own row.
- A path that cannot finish at a scale because an engine lacks an index loads a fixed number of edges, and the row says how many.
- Loading one row per commit is its own workload on both engines.
- The suite covers bulk load with on-disk size, reopen, many small commits, point lookups, scans, ordering, one to three hop traversals, variable-length paths, grouped aggregation, bulk update, detach delete, keyword and vector search with their index builds, and concurrent reads.
- A workload that needs a feature one engine lacks reports that engine as unsupported instead of failing.
- An engine is one module behind a small trait that opens a database, loads the data, runs a query, and reports its size on disk.
- Each workload states its query once per language, GQL for RhizoDB and Cypher for LadybugDB, with SQL and Datalog forms added by the engines that need them.
- A workload with no form in an engine's language is reported as unsupported for that engine, never as a failure.
- Each engine uses its own prepared and parameterised form of a query where it has one.
- Every run walks all returned rows, so lazy work is inside the timed region.
- Each workload runs once unreported as a warm-up, then at least five repetitions that alternate which engine goes first.
- A workload reports the median and the range, not the mean.
- Every run counts its errors and the work it did.
- Every run fingerprints its result by row count and an order-independent checksum, and every engine that ran the workload must agree with RhizoDB, the reference.
- A disagreement with the reference, or between repetitions of one engine, is printed as a failure and fails the run.
- Index search results compare by row count only, since the engines rank differently.
- Mutating workloads start from a fresh copy of the loaded database, so repetitions do not build on each other.
- A claim about a speedup needs its limiter named from a profile, per the benchmark checklist.
- The benchmark crate's tests run every workload at tiny scale on both engines in the differential workflow.
- Reopening is measured with a warm page cache, since an embedded host reopens a file it just used.

## Open

None
