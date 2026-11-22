# Performance

Decides how RhizoDB's speed is measured and compared.

## Background

RhizoDB's speed is only known relative to something, and the numbers will decide which optimizations come next. A benchmark that favors one side or hides a wrong answer would send that work in the wrong direction.

## Decisions

- Benchmarks live in their own crate outside the main workspace, beside the differential crate, and run with `mise run bench`.
- The embedded engines are RhizoDB, LadybugDB, SQLite, DuckDB, and CozoDB, and they always run.
- The server engines are Neo4j, Memgraph, and FalkorDB, and each runs only when its URL is given by flag.
- A server without a URL is skipped, and servers never run in CI.
- Embedded engines build from crates.io sources at versions pinned with `=`, and LadybugDB builds from the same pinned source as in the differential crate.
- DuckDB and CozoDB sit behind cargo features that CI does not enable.
- An engine whose cargo feature is off is reported as unavailable with the feature to enable.
- CozoDB stores in RocksDB.
- LadybugDB's FTS and VECTOR extensions are built from the source of the pinned release with `mise run lbug-extensions`, and never downloaded.
- Extensions are loaded before the clock starts, and an error from loading one fails the run.
- An extension that is not built is reported as unavailable with the command that builds it.
- The report groups engines by category, embedded or server, and says per engine whether it is in-memory or file-backed.
- An engine is one module behind a small trait that opens a database, loads the data, runs a query, and reports its size on disk.
- Every engine gets one seeded generated graph that is neutral to any query language, with a CSV export for engines whose bulk loader reads files.
- The graph has persons with an optional age, a text bio, and a 64-float embedding, cities, `KNOWS` edges with a few hubs and no repeated pair, and `LIVES_IN` edges.
- A few bios hold rare words that give some keyword queries fewer than ten hits.
- The embedded engines store their databases as files in a fresh temporary directory, never in memory.
- The scale is chosen by name, from tiny for tests to large, and the operation counts per workload follow it.
- Each engine is also measured on its own best path.
- LadybugDB loads with `COPY FROM`, DuckDB loads from the CSV files, and SQLite loads in one transaction with prepared inserts and builds its secondary indexes afterwards.
- CozoDB loads in one transaction of large batches, and RhizoDB loads in one write transaction with node handles from the creating query.
- A server creates an index or constraint on the node ids before loading, and loads in large `UNWIND` batches.
- Every engine's input for its bulk path is built before the clock starts.
- The path every engine shares, a statement per batch of rows, is reported beside the tuned one as its own row.
- A path that needs an index its engine lacks loads a fixed number of edges, and the row prints how many.
- Loading one row per commit is its own workload on every engine.
- Every engine keeps its default commit durability.
- The suite covers bulk load with on-disk size, reopen, many small commits, point lookups, scans, ordering, one to three hop traversals, variable-length paths, grouped aggregation, bulk update, detach delete, keyword and vector search with their index builds, and concurrent reads.
- The suite includes a `RETURN 1` baseline that shows each engine's cost per query.
- A workload that needs a feature one engine lacks reports that engine as unsupported instead of failing.
- Reopening a database file is unsupported for a server.
- Each workload states its query once per language, GQL for RhizoDB, Cypher for the graph engines, SQL, and Datalog.
- A query that differs for one engine overrides the shared text for that engine only.
- A workload with no form in an engine's language is reported as unsupported for that engine, never as a failure.
- Each engine uses its own prepared and parameterised form of a query where it has one.
- Every run walks all returned rows inside the timed region.
- Each workload runs once unreported as a warm-up, then at least five repetitions that alternate which engine goes first.
- Fewer than five repetitions print a warning in the output and in the JSON.
- A workload reports the median and the range, not the mean.
- Every run counts its errors and the work it did, and a failed operation is not counted as work.
- Every run fingerprints its result by row count and an order-independent checksum, which also keeps which query gave which result.
- A float in a fingerprint is rounded to six decimals with the rounding boundary moved off the midpoint.
- Every engine that ran a workload must agree with RhizoDB, the reference.
- A disagreement with the reference, or between repetitions of one engine, is printed as a failure and fails the run.
- A difference that comes from an engine's semantics is removed with a per-engine query override.
- Index search results are compared with the exact answer computed from the dataset, not with each other.
- A keyword query with fewer than ten hits must return exactly those hits.
- A vector query is compared with the exact nearest ten by cosine distance.
- The report shows recall at ten for every search, flags a recall below 0.9, and fails a run in which an exact engine scores less than 1.
- RhizoDB, and any engine that scans, must score 1.
- Write workloads time closing the database as well, and report the close within the time.
- Mutating workloads start from a fresh copy of the loaded database.
- A server is cleared and loaded again, outside the timer, before each workload that changes it.
- A server's on-disk size is reported as n/a.
- A load that fails is remembered and not retried, and a half-loaded database is never reused.
- The JSON header records each engine's configuration and version, with versions read from the pins.
- A claim about a speedup needs its limiter named from a profile, per the benchmark checklist.
- The benchmark crate's tests run every workload at tiny scale on the embedded engines built by default in the differential workflow.
- A server's test runs only when the environment gives its URL.
- Reopening is measured with a warm page cache.

## Open

None
