# Snapshots and log retention

Decides how a database is copied whole to another node and how long its log is kept for peers that read it.

## Background

A replica starts from a copy of the database file and then follows the log from the position that copy holds. The copy must be consistent while the source keeps writing, and the log after that position must still exist when the replica asks for it.

## Decisions

- A snapshot is the last durable checkpoint of the file, and its position is that checkpoint's LSN.
- A snapshot holds the checkpoint's tree as a reader does, so no block it reaches is reused or cut off the file while it lives.
- A snapshot never forces a checkpoint, so taking one does not stall the writer.
- A snapshot pins only a whole checkpoint, never a stage of one, since a stage's tree holds part of the commits after the checkpoint's LSN.
- A snapshot's pin lasts at most `snapshot_time_limit`, one hour by default, after which the engine drops its tree and reading it fails as expired, so a forgotten snapshot cannot stop the file from reusing space forever.
- A read of a snapshot holds the pinned tree for as long as it reads, so a pin that expires meanwhile frees no block the read is copying.
- `DatabaseStats::held_bytes` reports the space that readers and snapshots keep the file from reusing.
- A snapshot copies only the blocks the checkpoint's extent table marks live, and the receiver's file holds zeros elsewhere.
- A snapshot reads blocks from the file past the block cache, so an export does not evict the pages readers use.
- A snapshot carries the checkpoint's header marked clean, so the installed file opens without a log.
- An installed database keeps the source's database id, since every page checksum is seeded with it.
- The snapshot stream is a manifest followed by the live blocks, cut into chunks of 1 MiB, each followed by an xxh3 checksum seeded with the snapshot's random nonce and the chunk's index.
- A chunk from another offset or another snapshot fails its checksum.
- The stream is read at any offset and gives the same bytes every time, so a cut transfer resumes from the offset it reached.
- The manifest records the stream's length, the position, the history the layers above carry, the file's length, the live block ranges, and the header.
- An unknown snapshot format version refuses the stream.
- Installing writes the stream into the file's creation staging name under its lock, syncs it, checks every page with `check`, renames it over the database file, and opens it.
- Installing locks an existing database file first, so it fails while another open holds that file.
- Until the rename, the existing database and its log stay as they were, so a crash or a failure leaves the old database or none.
- An installed database removes the old log's segments as it opens, since its header is clean.
- The installed image's header names a replica with a fresh incarnation, keeps the checkpoint's chain hash, and records as the highest term seen the higher of the replaced database's and the snapshot's, so a stale writer's changes are fenced after any install.
- Install refuses a snapshot whose newest term is older than the term the replaced database has seen, unless the host forces it.
- Install refuses to replace a database that holds the changes of another database than the snapshot's, unless the host forces it, and a database whose last checkpoint holds no change counts as none.
- A snapshot's position names the term and writer that the pinned tree's history records for its LSN, and the chain hash its header records.
- Acknowledging a position of a newer term fences a writer as a promotion elsewhere would.
- On Windows, which renames no open file, install closes the old database and the staging file before the rename and opens the database again after it, so another open can take the file in between.
- Off Unix the engine cannot sync a directory, so whether a crash keeps a finished install's rename rests on the file system's own journal of names.
- A damaged or truncated stream fails the install as corrupt data, and a failing reader fails it as an I/O error.
- The log keeps every segment that holds a record after the oldest position an acknowledged peer or a snapshot in progress pins.
- Peers are named by bytes the host gives, and acknowledging a peer again replaces its position.
- A pin whose next record is no longer in the log is dropped.
- When the segments kept only for peers pass `max_retained_bytes`, the peers furthest behind are released until the rest fit, and those peers then fall too far behind.
- A snapshot in progress keeps the log after its position regardless of `max_retained_bytes`, and only its time limit bounds it, so a slow transfer does not arrive too far behind.
- `max_retained_bytes` defaults to 1 GiB.
- A checkpoint starts on the log written since the last checkpoint, not on the segments kept for pins.
- Each header records the acknowledged position of every peer and the first log segment kept for them, so a restart or a crash keeps following peers without a new snapshot.
- Acknowledgements reach the file with the next header a checkpoint or a clean open or close writes, never with a sync of their own.
- A header records at most 32 peers, and acknowledging another one fails.
- A clean close keeps the segments peers still need, and a clean open keeps them before the new log it starts.
- A kept segment keeps the salt of the log that wrote it, and a reader seeds each record with the salt in its own segment's header.
- Recovery does not replay kept segments before the checkpoint's log, and removes a missing or damaged one with every segment before it, so damage there only shortens what peers can read.
- A snapshot's header and an installed image record no peers and no kept log, so an install leaves none of the old database's log to serve.
- Snapshot pins live in memory and end with the database that holds them.

## Open

None
