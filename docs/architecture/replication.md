# Replication

Decides how an embedded database follows the changes of a writer elsewhere and how writers hand over.

## Background

A host that runs RhizoDB on several machines needs copies of one graph that serve reads and can take over writes. Replication stays inside the embedded library, and the host owns leases, transport, and the routing of writes.

## Decisions

- Replication is embedded, with no server, no consensus protocol, and no async runtime in the library.
- One writer changes a database file at a time, and any number of replicas follow it.
- The host owns leases, transport, routing of writes to the writer, and who wins a split.
- Replication is asynchronous, and a host builds semi-synchronous commits from positions and acknowledgements.
- A host waits for a replica to reach the writer's position read right after a commit, instead of a position returned by the commit.
- A position is the LSN of the last change a database holds, the term and writer id of the writer that made it, and a chain hash over every change up to it.
- Term 0 has no writer id, and every later term has a writer id of 1 to 64 bytes.
- A position converts to versioned bytes, so a host can send it between nodes and store it.
- Every log record carries the term of the writer that made it and the chain hash of the record before it.
- The chain hash of a record is an xxh3 hash, seeded with the chain hash before it, over every byte of the record after its length and checksum, so it is the same in every log that holds the record.
- The chain hash tells apart two histories that reused a term, a writer id, and an LSN, so positions carry no database id, since two unrelated logs differ at their first record.
- A configuration record begins each term in the log and names its writer and the incarnation of the file that began it.
- A configuration record also marks where a writer stepped down, so a demotion and the end of a term are one durable record.
- The history of terms, each with its writer id, incarnation, first LSN, and the LSN it ended at, lives in a table of the engine's own, so checkpoints, copies, and snapshots carry it.
- The history is never trimmed.
- Batches of changes write only graph tables, and only configuration records write the history table.
- The header keeps the file's incarnation, whether a file with no history is a replica, the highest term the file has heard of, and the chain hash at the checkpoint's LSN.
- A new file and an installed copy get a new random incarnation, so a copy of a file never passes as the file that began a term.
- A file's role follows from its history and header: a file with no history is a writer unless marked a replica, the incarnation that began the newest term and never stepped down is a fenced writer, and every other file is a replica.
- A writer opens fenced after every restart and takes writes again only once the host promotes it with its own newest term and writer id.
- A replica opened on a missing or empty file writes nothing and stays a replica, so it holds no change until it applies its writer's first one.
- `OpenOptions::replica` makes a file that holds no change a replica and refuses a file that holds a writer's changes with `NotReplica`.
- Every local write of a database that is not the writer fails with `NotWriter`, checked again inside the commit under the writer lock.
- A file that needs an upgrade fails to open as anything but a writer, since the upgrade is a write.
- Data that belongs to one node lives in a database file of its own, beside its replica.
- `changes_since` reads whole records from the log segments on disk after the asker's position, seeding each record with its own segment's salt, and holds no writer lock.
- `changes_since` stops before a record that would pass its byte limit, except that it always returns one whole record when there is any.
- `changes_since` fails with `TooFarBehind` when the segment that holds the next record is gone.
- `changes_since` checks the asker's term, writer id, and chain hash against its own history and fails with `Diverged` where they differ.
- A position past the database's last change in its newest term, or in a later term, gets no changes instead of an error.
- A writer that hears of a higher term from a peer's position keeps that term in its header and becomes fenced, so a host passes only positions of authenticated peers.
- Changes carry the position they follow, a checksum on every record, and one on the whole, and decoding them from untrusted bytes fails only as `CorruptData`.
- Decoding checks that LSNs follow one another, that each record follows the chain hash before it, that each new term begins with a configuration record of a higher term, and that a step down ends the term it is in.
- `apply` logs each record with its writer's LSN, term, commit time, and transaction id, so a replica's log matches its writer's record for record and a replica serves changes to the next one.
- `apply` makes each record durable before readers see it, as a commit does.
- `apply` syncs each record on its own, since one sync for several records would let a crash leave damage before a synced record, which recovery reports as corruption.
- `apply` skips a record the replica holds in the same term under the same writer, and compares the chain hash at the replica's last record, so applying changes twice is safe and a reused epoch is caught.
- `apply` fails with `Gap` when changes follow an LSN the replica does not hold yet.
- `apply` fails with `Fenced` for a record of a term lower than the highest the replica has seen, and with `Diverged` for another writer in the newest term or for different changes where the replica holds some.
- A failed `apply` keeps every record before the one it refused.
- `apply` on a writer or a fenced writer fails with `NotReplica`.
- `promote` with a term higher than every term seen writes a configuration record and makes the database the writer.
- `promote` with the newest term and writer id writes nothing and makes the file that began that term the writer again, and fails with `Diverged` on a copy of that file or after a step down.
- `promote` with a lower term fails with `Fenced`, and with the newest term under another writer id fails with `Diverged`.
- The engine never picks a winner between two writers of one term, and the host settles it by resyncing the side that does not hold its lease and promoting the winner with a higher term.
- A replica promoted before it holds the graph's format version records it and the tokenizer as a new database does.
- `demote` refuses new writes and the commit of an open write transaction at once, waits up to a deadline for that transaction to end, records the step down, and returns the final position.
- A `demote` that runs out of time fails with `WriterBusy` and leaves the database fenced, and a later `demote` finishes it.
- `compare_lineage` finds the first LSN where two histories differ and tells whether this database is the same as, behind, ahead of, or diverged from another one.
- Errors of the replication kinds carry the positions and LSNs behind them, which `Error::replication_failure` returns.
- After an `apply` that touches index definitions, the graph reloads them before any reader begins, so no reader sees data and definitions of different commits.
- `apply` and `promote` drop the cached counts of column deltas, which only writers keep.
- Committed tokens, label sets, the next IDs, and query plans need no refresh after `apply`, since tokens and label sets never change and the rest are read per transaction or bound without data.
- Replication reports its own error kinds: `Fenced`, `Diverged`, `TooFarBehind`, `Gap`, `NotReplica`, `NotWriter`, and `WriterBusy`.

## Open

- Snapshots, their install, and how long a writer keeps changes for its replicas.
- How a writer keeps its log for replicas across a clean close, which today removes every segment.
- How a replica of a newer build follows a writer that has not upgraded the graph's format yet, since a replica cannot write the upgrade.
