# Storage

Decides how RhizoDB keeps data on disk.

## Background

Writing a storage engine from scratch is the riskiest part of a database. Delegating it keeps the first versions stable while leaving room to replace it later.

## Decisions

- redb handles disk storage until RhizoDB's own engine, decided in the storage engine doc, ships.
- The storage layer sits behind a trait so it can be swapped.
- Values are stored as bytes in a format RhizoDB defines, and the format carries a version.
- Each format version can hold everything the older versions hold.
- Opening a path with no file, or an empty one, writes the new database under a staging name in the same directory, syncs it, and links it into place, so a power loss during creation leaves either no database or a whole empty one.
- The staging file's own lock decides which of several concurrent opens creates the database, and the others wait for it instead of replacing it.
- A staging file that an interrupted creation left behind is reused when redb can open it and removed when it cannot.
- Opening an existing file that redb cannot read never rewrites it, since it may be a damaged database.
- Opening a database written in an older format version records the current version in the same write transaction, before anything else reads it.
- Opening a database written in a newer format version fails.
- Opening a database whose keyword indexes another tokenizer built rebuilds them in the same write transaction, before anything else reads them.
- Opening a database written before postings kept term positions rebuilds its keyword indexes in the same write transaction.
- Opening a database written in an older format version whose postings already kept positions does not rebuild its keyword indexes for the upgrade alone.
- Index definitions and index entries live in tables of their own, so a database from before indexes upgrades with no index.
- Opening a database written before indexes could cover relationships rewrites each index definition as one over a label, in the same write transaction.
- Adding a kind of index changes the format version, so an older build refuses the database instead of failing on an unknown definition.
- Every call into redb runs under a panic guard until RhizoDB's own engine, which checks page checksums, replaces redb.
- The guard covers opening, every read, iteration step, write, and commit, and the drop of every redb value, and returns a caught panic as corrupt data.
- Only the redb backend names redb, which a guard rule checks, so no call can skip the guard.
- redb's own corruption errors are reported as corrupt data too.
- A caught panic poisons the database, and every later operation on it fails as corrupt data until the file is opened again.
- A poisoned database drops its redb values while an unwind is in progress, so redb writes nothing more and the next open recovers the file from its last commit.
- The guard relies on `panic = "unwind"`, and a host built with `panic = "abort"` still dies on a damaged file.
- RhizoDB installs no panic hook, so the host's hook still reports each caught panic.

## Open

None
