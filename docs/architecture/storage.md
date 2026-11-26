# Storage

Decides how RhizoDB keeps data on disk.

## Background

The graph layer needs ordered key-value tables that survive a crash. A storage trait between them keeps the graph layer independent of how the tables are stored.

## Decisions

- RhizoDB's own engine, decided in the storage engine doc, stores every database.
- The storage layer sits behind a trait so it can be swapped.
- Values are stored as bytes in a format RhizoDB defines, and the format carries a version.
- Each format version can hold everything the older versions hold.
- Opening a path with no file, or an empty one, writes the new database under a staging name in the same directory, syncs it, and links it into place, so a power loss during creation leaves either no database or a whole empty one.
- The staging file's own lock decides which of several concurrent opens creates the database, and the others wait for it instead of replacing it.
- Opening an existing file that the engine cannot read never rewrites it, since it may be a damaged database.
- Opening a database written in an older format version records the current version in the same write transaction, before anything else reads it.
- Opening a database written in a newer format version fails.
- Opening a database written in a format version older than the first one RhizoDB's own engine wrote fails the same way, since only redb ever stored those versions.
- Opening a database whose keyword indexes another tokenizer built rebuilds them in the same write transaction, before anything else reads them.
- Opening a database written in an older format version does not rebuild its keyword indexes for the upgrade alone.
- Opening a database written before posting blocks moves each posting into its block in the same write transaction, without tokenizing again.
- Adding a kind of index changes the format version, so an older build refuses the database instead of failing on an unknown definition.
- `Database::backup_to` copies every table of one read snapshot into a new database file of the same backend, so restoring a backup is opening it.
- A backup copies through the storage trait, and each backend writes the new file itself and returns once that one file holds the copy durably.
- A backup leaves out the legacy tables, which only the upgrade of an older format reads.
- A backup commits its copy in write transactions of a bounded size, so its memory does not grow with the database.
- A backup takes no write lock, so writers carry on while it runs and their commits stay out of the copy.
- A backup is written under a staging name that holds the target's file name, the process, and a counter, in the target's directory, synced, and hard-linked into place, so the target holds nothing or the whole copy.
- A backup never replaces an existing file or directory, and fails as a storage error instead.
- A failed backup removes its staging file, and a crashed one leaves it behind for the host to delete.
- The own engine closes its copy before linking it, which checkpoints the whole log into the file, and fails the backup when a log segment is left.

## Open

None
