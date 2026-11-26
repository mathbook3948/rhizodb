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
- A damaged or truncated stream fails the install as corrupt data, and a failing reader fails it as an I/O error.
- The log keeps every segment that holds a record after the oldest position an acknowledged peer or a snapshot in progress pins.
- Peers are named by bytes the host gives, and acknowledging a peer again replaces its position.
- A pin whose next record is no longer in the log is dropped.
- When the segments kept only for peers pass `max_retained_bytes`, the peers furthest behind are released until the rest fit, and those peers then fall too far behind.
- A snapshot in progress keeps the log after its position regardless of `max_retained_bytes`, and only its time limit bounds it, so a slow transfer does not arrive too far behind.
- `max_retained_bytes` defaults to 1 GiB.
- A checkpoint starts on the log written since the last checkpoint, not on the segments kept for pins.
- Pins live in memory, so a reopened database keeps only the log its recovery needs.

## Open

- Whether pins and the segments they keep survive a clean close, so a peer behind a restarted writer can follow it without a new snapshot.
