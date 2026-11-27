# Multi-process access

Decides how several processes on one machine open one database file at once, so that any number of them read it and one write transaction at a time, from any of them, writes it.

## Background

The engine locks its file exclusively, keeps the commits since the last checkpoint in the memtable of the process that made them, and decides alone which blocks and log segments a checkpoint may reuse or remove. Hosts that run several processes over one file need each process to read the newest commits and to write in turn, as SQLite in WAL mode and LMDB allow.

## Decisions

- Any number of processes on one machine read a database file at once, and one write transaction at a time, in any of them, writes it.
- A read-write open takes part in multi-process access only when its `OpenOptions` ask for it, and an open that does not ask keeps the exclusive lock, creates no side file, and runs none of the code below.
- The processes that have a file open with multi-process access form a session, which begins with the first of them and ends when the last one closes.
- From phase 3 on, every read-only open joins a session, so a read-write process that joins later finds it.
- An open that locks the file exclusively fails while a session is open, and a session's open fails while an exclusive open holds the file.
- A database in memory cannot take part in a session.
- A process opens a file at most once, and a second open in the same process fails as it does today.
- A session keeps its shared state in a side file named after the database file with `-shm`, which the first process of each session creates or resets in place and never truncates.
- The side file holds the session's identity, a copy of the newest durable header, the end of the log, a journal of reused blocks, the acknowledged peers, and one slot per process.
- The session's identity is the side file's layout version, the engine format minor, the graph format version, the tokenizer fingerprint, the database id, and the replication role, and a process whose build differs in any of them fails to join.
- Every process maps the side file into memory and reads and writes it with atomic operations, as SQLite maps its wal-index and LMDB its lock file.
- A record of several words sits behind a sequence number that is odd while it changes, so a reader retries a torn read instead of taking a lock.
- The side file is never synced and never trusted across sessions, since the first process of a session rebuilds it from the database file and the log.
- Multi-process access is refused on a file system the engine finds to be remote, since processes on other machines share neither memory nor reliable locks, and SQLite and LMDB refuse the same.
- Multi-process access is refused on a file system that refuses byte-range locks or shared mappings, where a single-process open still goes on unlocked.
- Every lock of a session is an OS byte-range lock, which the OS releases when its holder dies, and no lock is a mutex in shared memory, since macOS has no robust mutexes and a robust mutex leaves its protected state for the next holder to repair.
- Each process of a session holds a shared lock on the database file while it is open, so an older build or an exclusive open fails while a session is open.
- On Windows that lock covers a byte range past any block the file can hold, since a shared lock there denies writes to the range it covers, the holder's own included.
- The side file carries the session lock, the write lock, the checkpoint lock, and one liveness lock per slot, each on a byte past the mapped state, since Windows enforces locks on the bytes they cover.
- Every process holds the session lock shared while it is open, and an open that gets it exclusively is the first of its session.
- The write lock is held exclusively from the start of a write transaction to its commit or rollback, and a write that finds it held waits, as a write waits for another in one process today.
- The checkpoint lock is held exclusively by a process while it writes a checkpoint, a compaction, or any other header.
- A process holds its slot's liveness lock exclusively while it is open, so another process that gets that lock knows the slot's owner is dead.
- On Linux every lock of a session is an open-file-description lock, so closing another descriptor of the file drops none of them.
- Where only classic `fcntl` locks exist, a process opens the side file once and never closes a descriptor of it while it is open, since closing any descriptor of a file drops every `fcntl` lock the process holds on it.
- The database file is locked only with the standard library's whole-file lock or a range lock past its blocks, and the side file only with range locks, so no file mixes `flock` and `fcntl`.
- A write transaction takes the process's writer mutex, then the write lock, then catches up with the log, so it begins on the newest commit of every process.
- A commit appends its record at the end of the log that the side file names, syncs it, publishes the new end in the side file, and only then joins its versions to its own memtable, so no process reads a record before it is durable.
- A process publishes a commit's end and joins its versions under the lock its catch-up takes, so a reader in the same process never replays that commit from the log.
- A writer appends at the end the side file names and writes over whatever a dead writer left past it, since no record past that end was published, and so none was acknowledged.
- A writer that finds a log segment after the one the side file names removes it, as recovery removes an unfinished rollover.
- Before a read or write transaction begins, a process compares the log end the side file publishes with the last LSN it holds, and begins at once when they match.
- When records are new, the process reads them from the log segments up to the published end, checks each record's checksum and chain hash, and applies it to its memtable, as an open replays the log.
- A process never reads a record past the published end, so it never sees a record that is still being appended or that a dead writer tore.
- `changes_since` in any process serves only the records that process has caught up with, since it reads no further than its own published view.
- A process that finds a newer durable header in the side file adopts it before it reads the log, by loading that header's catalog and extent table and pointing new readers at its tree.
- Adopting a whole checkpoint's header gives new readers a memtable that holds only the commits after the checkpoint's LSN, and readers that began before keep the old memtable and tree until they end.
- Adopting a stage's header replaces only the tree, since a stage keeps the LSN the log replays from and the memtable still holds every commit after it, as the frozen memtable over a stage does in one process.
- A log segment that is gone when a process opens it was released by a checkpoint, so the process adopts the newest header and goes on from its LSN.
- On Windows, a segment that another process is removing refuses access, and the process treats that refusal as a released segment.
- In a session, a process keeps every tombstone in its memtable until it adopts a checkpoint that covers it, since a checkpoint another process writes from its own frozen memtable may hold the key's older value.
- After catching up with records that change index definitions, the graph reloads them before any reader begins, as after `apply`.
- Catching up with any record another process wrote drops the graph's cached counts of column deltas.
- Tokens and label sets need no refresh, since a committed one never changes and a miss reads it from the table.
- A process with no transaction open adopts each new checkpoint from a thread of its own within a bounded time, so an idle process does not hold an old tree.
- Each slot records the oldest checkpoint generation that any tree of its process reaches, which includes the trees of its readers, snapshots, pins, and published view.
- A checkpoint in any process takes the lowest generation of every live slot as its oldest tree, so it reuses no block or extent slot, and cuts nothing off the file, that a tree in another process still reaches.
- A process raises its slot's generation only once its last tree of the lower generation is gone, and never lowers it.
- A process joins by writing the newest durable generation into a free slot and checking that it is still the newest, retrying otherwise, as an LMDB reader does, so a checkpoint either sees the slot or the process loads a newer header.
- Beginning a read writes nothing to the side file and takes no lock, since the process's slot already covers the tree its published view holds.
- A slot also records the oldest LSN that its process's snapshot pins keep, and a checkpoint keeps the log after the oldest such LSN of every slot, as it keeps the log for a pin of its own.
- Acknowledged peers live in the side file, so an `acknowledge` in any process keeps the log for that peer, and every header a checkpoint writes records them.
- A checkpoint test-locks the liveness lock of each slot in use, and clears the slot of a dead process before it reads the generations.
- The log needs no entry in the slots, since a process that misses a segment adopts the checkpoint that released it.
- The side file holds a fixed number of slots, and an open that finds none free fails with its own error kind.
- `DatabaseStats::held_bytes` counts the space that the trees of every process keep from reuse.
- A commit that pushes the memtable or the log past its limit starts a checkpoint in its own process when it gets the checkpoint lock without waiting, and otherwise leaves the work to the checkpoint already running.
- That checkpoint freezes the process's memtable under the write lock, so it covers every commit of every process up to the frozen LSN, and runs on that process's checkpoint thread while every process goes on committing.
- A process that takes the checkpoint lock first adopts any header newer than the one it holds.
- A commit whose memtable passes twice the limit while a checkpoint runs in another process waits for the checkpoint lock.
- A checkpoint publishes each header in the side file once that header is durable, and processes adopt only the headers the side file publishes.
- A process that takes the checkpoint lock and finds a valid header in the file newer than the one the side file publishes syncs the file and then publishes that header, since a dead process wrote it and the page cache may hold it alone.
- The checkpointing process removes the segments a durable header released and publishes the log's new first segment in the side file.
- Each process records which blocks and extent slots each generation freed by comparing the extent table of every header it adopts with the one before, so a checkpoint in any process holds freed space as long as the rules of one process require.
- A process that skipped generations records the blocks they freed under the newest of them, which holds those blocks at least as long as needed.
- A header written outside a checkpoint, such as one that records a higher term, takes the checkpoint lock and is published the same way.
- A process that closes while others have the file open finishes a checkpoint it is running and then writes nothing to the file or the log.
- The last process to close gets the session lock exclusively and closes as a single process does today, checkpointing the whole log, writing a clean header, and removing the segments no peer needs.
- Each process keeps its own block cache.
- A block that a durable header reaches is never written again, and a freed block is reused only once no slot holds a tree that reaches it, so a process's cache is right for every tree that process holds.
- An extent in a reused slot has a new id, so the cached pages of the extent before it are never looked up again.
- A checkpoint that writes into a free range of an extent records that extent's id in the side file's journal under its generation before it publishes its header.
- A process that adopts a header first drops its cached pages and values in the extents the journal names for every generation after the last one it adopted, and drops its whole cache when the journal no longer reaches back that far.
- The first process of a session resets the side file under the session lock, held exclusively, and then opens the file as an open does today.
- A first process that opened read-write recovers the log, or starts a new log on a clean file, before it lets another process join.
- A first process that opened read-only replays the log into memory, and the first read-write process that joins later recovers the log under the write and checkpoint locks.
- The other processes adopt the header that such a late recovery writes as they adopt any checkpoint, since recovery's checkpoint places nothing in free blocks and cuts nothing off the file.
- A process that dies leaves nothing to replay: the next writer writes over what it left past the published end, the next checkpoint publishes a header it left only after syncing it, and the next checkpoint clears its slot.
- After a power loss, or once every process of a session has died, the next session's first process recovers the log as an open does today.
- A session's first process decides the replication role as an open decides it today, and every process of the session shares that role through the side file.
- A writer therefore opens fenced after a restart of its whole session, and a process that joins a running session takes the session's role.
- Every local commit checks the role in the side file under the write lock.
- `apply`, `promote`, and `demote` take the write lock, and `promote`, `demote`, and an observed higher term change the role in the side file, so every process takes or refuses local writes from its next commit on.
- `demote` marks the role fenced in the side file before it waits for the write lock, so a write transaction open in any process fails at its commit.
- `install_snapshot` locks the database file exclusively, as it does today, so it fails while a session is open.
- A graph format or tokenizer upgrade runs only in the first process of a session, before another process joins, since a process of another build fails to join.
- An index definition is created or dropped in a write transaction like any other, so it takes the write lock, and the other processes reload definitions when they catch up with it.
- An automatic compaction waits until every slot reaches the durable generation, as it waits for older trees in one process today, and `compact` shrinks the file only as far as the trees of other processes allow.
- `backup_to`, `export`, `check`, and snapshot exports run in any process, on that process's read snapshot or durable header.
- A read transaction in a session costs one atomic load from the side file when nothing was committed since the last one began, and otherwise the replay of the new records.
- A write transaction in a session adds the write lock, its release, and a few atomic stores after the sync, which is small beside the sync itself.
- Each process of a session holds its own memtable and block cache, so a session's memory grows with its processes.
- Phase 1, a read-only open while no writer has the file open, takes a shared lock on the database file and uses no side file.
- Phase 3 lets read-only processes join a session whose one read-write process writes and checkpoints, and it builds the side file, catch-up, the slots, the cache journal, and recovery for the first process of a session.
- In phase 3 the read-write process holds the write lock for as long as it is open, so a second read-write open of the session fails.
- Phase 4 takes the write lock for each write transaction, lets the committing process checkpoint, and moves the replication role, peers, and pins into the side file.
- The side file and its locks are reached through a trait, with an implementation over the operating system and a simulated one, as files are.
- The simulation runs each process of a session as an engine in one test process, on the simulated file system and a simulated side file, and is deterministic for a seed.
- A simulated kill drops a process's locks, memory, and slot owner, and keeps the files, the page cache, and the side file, while a simulated power loss also drops the side file and every write since the last sync.
- Shuttle models in the `interleavings` crate explore the join, catch-up, slot, and reuse protocols across interleavings of processes.
- Kill tests spawn the test binary as child processes that write numbered commits and read snapshots, kill them at random points, and check that each snapshot every survivor read is a prefix of the reported commits, that the next writer goes on, that a dead process's slot is reclaimed, and that the file reopened alone passes `check` with every reported commit.
- Phase 3's kill tests kill readers and the writer, and phase 4's add writers in several processes racing for the write lock, checked by the isolation history check across processes.
- A differential test runs one commit sequence in one process and across several and requires the same final tables and, for every snapshot, the tables of the one-process run at that snapshot's LSN.
- The engine benchmarks of a single-process open must not regress at any phase.

## Open

- Whether the code that maps the side file and takes byte-range locks lives in a crate of its own that allows unsafe code, since `rhizodb` forbids unsafe code and the standard library offers neither, or the side file is read with positioned reads and locked through one lock file per lock with the standard library's whole-file locks, which costs a system call per transaction and leaves Windows without a lock on the database file that lets the session write; the owner's policy on unsafe code decides.
- What `OpenOptions` calls the setting, and whether multi-process access becomes the default once its cost on one process is measured to be nothing.
- How a read-only open of a phase 1 build is kept out of a session, since it shares the database file's lock with the session's processes; a header flag that every header of a session carries refuses it once the session writes a header, or phase 1 ships only beside phase 3, and the release plan decides.
- Whether macOS gives programs open-file-description locks, which a run on the macOS CI runner decides.
- How a remote file system is told apart, including FUSE mounts, which tests on the file systems of each CI platform decide.
- Whether adopting a whole checkpoint copies the memtable's versions after its LSN or replays the log after it, which a benchmark of adoption under write load decides.
- How many slots the side file holds, how many generations its journal covers, and how often an idle process looks for a new checkpoint, which tests with many processes and frequent checkpoints decide.
- Whether a checkpoint may ignore a slot older than a time limit, as a snapshot pin expires, so a stopped process cannot keep the file from reusing space forever, at the cost of that process failing its reads as expired.
- How the block cache budget is shared among the processes of a session, since each takes the default for itself.
- Whether the last process of a session removes the side file, as SQLite does, which needs an open that raced with the removal to find that its mapping names a removed file.
- Whether writers waiting on the write lock are served in order, since neither `fcntl` nor `LockFileEx` promises it.
