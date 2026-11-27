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
- A position is the LSN of the last change a database holds, the term and writer id of the writer that made it, a chain hash over every change up to it, and the id of the database the history belongs to.
- Term 0 has no writer id, and every later term has a writer id of 1 to 64 bytes.
- A position converts to versioned bytes, so a host can send it between nodes and store it.
- Every log record carries the term of the writer that made it and the chain hash of the record before it.
- The chain hash of a record is an xxh3 hash, seeded with the chain hash before it, over every byte of the record after its length and checksum, so it is the same in every log that holds the record.
- The chain hash tells apart two histories that reused a term, a writer id, and an LSN.
- The database id tells a wiring mistake from a divergence, since the answer to `Diverged` is to install the lease holder's snapshot, which would replace an unrelated database's data.
- A writer's history belongs to its own file's database id, and a file that holds no change takes the database id of the first changes it applies or the snapshot it installs, kept in the header.
- A position at LSN 0 has database id 0, so it follows any database.
- `changes_since` and `apply` fail with `DifferentDatabase` when the two sides belong to different databases, and `compare_lineage` reports it.
- A configuration record begins each term in the log and names its writer and the incarnation of the file that began it.
- A configuration record also marks where a writer stepped down, so a demotion and the end of a term are one durable record.
- The history of terms, each with its writer id, incarnation, first LSN, and the LSN it ended at, lives in a table of the engine's own, so checkpoints, copies, and snapshots carry it.
- The history is never trimmed.
- Batches of changes write only graph tables, and only configuration records write the history table.
- The header keeps the file's incarnation, whether a file with no history is a replica, the highest term the file has heard of, the chain hash at the checkpoint's LSN, and the database id the file follows.
- These header fields, the log kept for peers, and the replication history come with engine format minor 4, and a file of an earlier minor opens as a writer with no history and no peers and draws its incarnation when it starts its next log.
- Every header that holds any of these fields records minor 4, so a build before it refuses a file this build only opened as an unsupported format instead of reading the fields as a damaged reserved area.
- A new file and an installed copy get a new random incarnation, so a copy of a file never passes as the file that began a term.
- A file's role follows from its history and header: a file with no history is a writer unless marked a replica, the incarnation that began the newest term and never stepped down is a fenced writer unless it stepped aside for a higher term it heard of, and every other file is a replica.
- A writer opens fenced after every restart and takes writes again only once the host promotes it with its own newest term and writer id.
- A replica opened on a missing or empty file writes nothing and stays a replica, so it holds no change until it applies its writer's first one.
- `OpenOptions::replica` makes a file that holds no change a replica and refuses a file that holds a writer's changes with `NotReplica`.
- Every local write of a database that is not the writer fails with `NotWriter`, checked again inside the commit under the writer lock.
- A file of an older graph format or tokenizer that opens as anything but its writer opens with its upgrade pending, since the upgrade is a write only the writer makes.
- A file with its upgrade pending refuses graph reads with `FormatUpgradePending`, naming the stored format and this build's, so a host routes reads elsewhere, and writes with `NotWriter` as any file that is not the writer does, and it still applies, serves, and relays changes, reports its position and history, takes and installs snapshots, backs up, and runs the engine's check.
- A read checks that the graph is ready both before and after it begins its snapshot, since an `apply` marks the graph pending before it publishes an older format, so no read begins over that format.
- After each `apply` a file with its upgrade pending reads the format and tokenizer again, and once its writer's upgrade arrived it reloads the index definitions before it serves any read, so no reader sees the graph between the old and the new format.
- A writer's upgrade writes the format version in the same commit as the last step of its migration, so a replica sees the new version only over a fully migrated graph, and every commit before it marks the format version as upgrading, which a replica waits through.
- `promote` of a file with its upgrade pending runs the upgrade as the new writer, checkpoints it, and serves reads after, and the upgrade reaches its replicas as changes.
- A rolling upgrade restarts the replicas on the new build first, which wait for the upgrade without serving reads, hands writing over to one of them, whose promotion upgrades the graph, and restarts the old writer last, which rejoins as a replica.
- A snapshot of a file with its upgrade pending holds the older format, which an installer of the new build opens with its upgrade pending and one of the old build reads as it is.
- A writer that opens a file of an older graph format or tokenizer checkpoints once after it commits the upgrade, so a snapshot taken right after holds the format a replica opens, and opening a file of the current format, a new file, or a replica checkpoints nothing.
- Data that belongs to one node lives in a database file of its own, beside its replica.
- `changes_since` reads whole records from the log segments on disk after the asker's position, seeding each record with its own segment's salt, and holds no writer lock.
- `changes_since` stops before a record that would pass its byte limit, except that it always returns one whole record when there is any.
- `changes_since` fails with `TooFarBehind` when the segment that holds the next record is gone, including one removed while it reads.
- `position`, `changes_since`, and `apply` read the database id and replication fields from a copy beside the durable state, so they never wait on a checkpoint.
- An install compares the snapshot's term with the highest term the replaced file's header, history, and log segments hold, so a term begun after the last checkpoint counts after a crash.
- An install over a file whose history cannot be read fails as `CorruptData` unless it is forced, so it fails closed and a forced install resyncs a damaged replica.
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
- `apply` takes the first record of a term equal to the highest term the replica has heard of, since a file hears of a term from a peer before it receives that term's first record, and refuses one of a term no higher than its newest term.
- A failed `apply` keeps every record before the one it refused.
- `apply` refuses with `UnsupportedFormatVersion` the record that writes a graph format newer than this build reads, after it applies the records before it, so a replica of an older build never logs a format it would misread.
- `apply` on a writer or a fenced writer fails with `NotReplica`.
- `promote` with a term higher than every term seen writes a configuration record and makes the database the writer.
- `promote` with the newest term and writer id writes nothing and makes the file that began that term the writer again, and fails with `Diverged` on a copy of that file or after a step down.
- `promote` with a lower term fails with `Fenced`, and with the newest term under another writer id fails with `Diverged`.
- The engine never picks a winner between two writers of one term, and the host settles it by resyncing the side that does not hold its lease and promoting the winner with a higher term.
- A replica promoted before it holds the graph's format version records it and the tokenizer as a new database does.
- `demote` refuses new writes and the commit of an open write transaction at once, waits up to a deadline for that transaction to end, records the step down, and returns the final position.
- A `demote` of a writer that has heard of a term above its own writes no step down, since a step down at the next LSN would conflict with the new writer's first record there, and instead marks the header so the file opens as a replica.
- A host passes the new writer's position to `changes_since` or `acknowledge` before it demotes a writer that lost its lease without a handover, so the old writer has heard of the new term, and `acknowledge` also keeps the log for that position as a peer's.
- An old writer that stepped aside holds a prefix of the new writer's history exactly when the new writer holds the same term, writer id, and chain hash at the old writer's last LSN, which `apply` and `changes_since` already compare, so a caught-up old writer applies the new writer's changes and one with changes the new writer never received fails with `Diverged`.
- An old writer rejoins without a snapshot only when the highest term it heard of is the first term after its own, since one that heard of a later term refuses the first records of the term between as `Fenced` and needs a snapshot.
- `promote` clears the mark of a file that stepped aside once its configuration record is durable, and the mark counts only while the file has heard of a term above its newest one, so a crash between the two leaves a fenced writer that the same promotion finishes, until the file hears of a higher term, after which it opens as a replica, as a writer that stepped aside does.
- A `demote` that runs out of time fails with `WriterBusy` and leaves the database fenced, and a later `demote` finishes it.
- `compare_lineage` finds the first LSN where two histories differ, comparing each term's writer id, first LSN, and the incarnation that began it, and tells whether this database is the same as, behind, ahead of, or diverged from another one.
- Errors of the replication kinds carry the positions and LSNs behind them, which `Error::replication_failure` returns.
- After an `apply` that touches index definitions, the graph reloads them before any reader begins, so no reader sees data and definitions of different commits.
- `apply` and `promote` drop the cached counts of column deltas, which only writers keep.
- Committed tokens, label sets, the next IDs, and query plans need no refresh after `apply`, since tokens and label sets never change and the rest are read per transaction or bound without data.
- Replication reports its own error kinds: `Fenced`, `Diverged`, `DifferentDatabase`, `TooFarBehind`, `Gap`, `NotReplica`, `NotWriter`, and `WriterBusy`.

## Open

None
