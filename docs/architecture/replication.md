# Replication

Decides how an embedded database follows the changes of a writer elsewhere and how writers hand over.

## Background

A host that runs RhizoDB on several machines needs copies of one graph that serve reads and can take over writes. Replication stays inside the embedded library, and the host owns leases, transport, and the routing of writes.

## Decisions

- Replication is embedded, with no server, no consensus protocol, and no async runtime in the library.
- One writer changes a database file at a time, and any number of replicas follow it.
- The host owns leases, transport, routing of writes to the writer, and who wins a split.
- Replication is asynchronous, and a host builds semi-synchronous commits from positions and acknowledgements.
- A position is the LSN of the last change a database holds with the term and writer id of the writer that made it.
- Term 0 has no writer id, and every later term has a writer id of 1 to 64 bytes.
- Two writers that never promoted both write term 0 and are not told apart, so a host promotes every writer before its first write.
- A position converts to versioned bytes, so a host can send it between nodes and store it.
- A database opened as a replica refuses every local write for the whole file with `ReplicaReadOnly`.
- A replica opened on a missing or empty file writes nothing, so it holds no change until it applies its writer's first one.
- A file that needs an upgrade fails to open as a replica, since the upgrade is a write.
- Data that belongs to one node lives in a database file of its own, beside its replica.
- Replication reports its own error kinds: `Fenced` for a lower term, `Diverged` for another writer at the same place, `TooFarBehind` for changes no longer kept, `Gap` for changes that do not follow the last one held, and `NotReplica` for changes given to a writer.

- Every log record carries the term of the writer that made it, and term 0 is the term before any promotion.
- A configuration record begins each term in the log and names its writer.
- The history of terms, each with its writer id and the LSN of its configuration record, lives in a table of the engine's own, so checkpoints, copies, and snapshots carry it.
- Batches of changes write only graph tables, and only configuration records write the history table.
- The engine format minor rises with the history table and configuration records, so older builds refuse such files.
- `changes_since` reads whole records from the log segments on disk after the asker's position, and fails with `TooFarBehind` when the segment that holds the next one is gone.
- `changes_since` checks the asker's position against the history and fails with `Diverged` when that LSN lies in another term or under another writer there.
- A position past the database's last change in its newest term, or in a later term, gets no changes instead of an error.
- Changes carry the position they follow, a checksum on every record, and one on the whole, and decoding them from untrusted bytes fails only as `CorruptData`.
- Decoding checks that LSNs follow one another and that each new term begins with a configuration record of a higher term.
- `apply` logs each record with its writer's LSN, term, and commit time, so a replica's log matches its writer's record for record and a replica serves changes to the next one.
- `apply` makes each record durable before readers see it, as a commit does.
- `apply` skips a record the replica holds in the same term under the same writer, so applying changes twice is safe.
- `apply` fails with `Gap` when changes follow an LSN the replica does not hold yet.
- `apply` fails with `Fenced` for a record of a term lower than the newest the replica has seen, and with `Diverged` for another writer in the newest term or for different changes where the replica holds some.
- A failed `apply` keeps every record before the one it refused.
- `apply` on a writer fails with `NotReplica`.
- `promote` with a term higher than the newest seen writes a configuration record, with the newest term and writer id writes nothing, with a lower term fails with `Fenced`, and with the newest term under another writer id fails with `Diverged`.
- The engine never picks a winner between two writers of one term, and the host settles it by resyncing the side that does not hold its lease.
- A replica promoted before it applied any change records the graph's format version and tokenizer as a new database does.
- `demote` refuses new writes at once, waits until no write transaction is open, and returns the final position.
- Whether a database is a replica is a setting of the open handle and of `promote` and `demote`, never stored in the file, so a host reopens a demoted writer as a replica.
- After an `apply` that touches index definitions, the graph reloads them before any reader begins, so no reader sees data and definitions of different commits.
- `promote` drops the cached counts of column deltas, which only writers keep.
- Committed tokens and label sets never change, so their caches need no refresh after `apply`.

## Open


None
