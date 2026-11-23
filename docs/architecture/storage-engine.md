# Storage engine

Decides the design of RhizoDB's own storage engine, which replaces redb.

## Background

redb's B-tree and allocator set a floor under the cost of a put, the growth of the file, and its size, and no change to the graph layout removes it. RhizoDB therefore builds its own engine behind the storage trait, in phases, and leaves room for replication and for spreading data over several disks and servers.

## Decisions

- RhizoDB's own engine replaces redb as the storage backend.
- The engine is a module of the `rhizodb` crate.
- The graph layout changes ship on redb, developed in parallel with the engine.
- The engine implements the same storage trait as the redb backend, and the swap changes no graph-layer behavior.
- The redb backend stays as the reference for differential tests and as the reader of redb files.
- A database at rest is one file.
- The write-ahead log is a separate log made of segment files next to the database file.
- A WAL record is a logical key-value operation, the put of a value under a key of a table or the delete of a key.
- A range delete of the storage trait is logged as the delete of each key it removes.
- Every WAL record carries a log sequence number (LSN) and a checksum of its own.
- Each WAL segment starts with a header holding the database id, the segment number, and the segment's first LSN.
- A WAL segment can be shipped to another node and replayed there on its own.
- A commit returns only after its WAL records are durable.
- A checkpoint writes the committed WAL records into the database file.
- Checkpoints run inline on the committing thread, with no background thread.
- A clean close checkpoints the whole WAL and removes its segments.
- Opening a database that has WAL segments replays them before any transaction begins.
- After a crash, the database holds a prefix of the commits that includes every acknowledged commit.
- A failed fsync makes the engine refuse every later commit until the database is reopened.
- The engine does all I/O through a file-access trait that reads and writes whole fixed-size blocks by block number, never arbitrary byte ranges.
- Each file has one block size, chosen when it is created.
- A layer over several files or devices, such as a mirror, a stripe, or parity, can implement the file-access trait without the engine knowing.
- Every page the engine writes carries a checksum and the LSN that last wrote it.
- A checksum mismatch is reported as corruption, never returned as data.
- The file-access trait has an implementation over the operating system's files, one in memory, and a simulated file system for fault tests.
- A database in memory runs the same engine over the in-memory implementation.
- The simulated file system is deterministic for a given seed.
- The simulated file system holds several files and can inject faults into one of them alone.
- A simulated crash keeps every synced write and may drop, reorder, or tear at 512-byte sectors each write since the last sync.
- A simulated crash may lose a change of file length, a create, a rename, or a remove that was not synced.
- The simulated file system can fail every operation after a chosen count, once or from then on.
- A failed fsync in the simulated file system fails every later fsync of that file.
- Opening a redb database converts it by copying every table into a new file and renaming that file over the original.
- The original redb file stays unchanged until the rename, and opening deletes a leftover conversion file.
- No backup of the redb file is kept after the conversion.
- During the transition, `OpenOptions` chooses the engine and sets the cache sizes.
- `Database` gains `compact`, `stats`, and `check`.
- A vector index may read its vectors from the property column instead of keeping a copy.
- The concurrency core of the engine is checked with loom.

## Open

- The page layout of the database file, including the page size and the file header.
- Whether the database file reuses free pages in place or appends extents in a log-structured way, and how free space is reclaimed.
- The size of an extent and where extents are placed across files, disks, and servers.
- How data is replicated, erasure-coded, and partitioned across servers.
- How a checkpoint changes the database file without losing data that an older snapshot or a crash still needs.
- How a read transaction combines committed WAL records that are not checkpointed yet with the database file.
- Whether checksums use crc32c or xxHash.
