# Storage

Decides how RhizoDB keeps data on disk.

## Background

The graph layer needs ordered key-value tables that survive a crash. A storage trait between them keeps the graph layer independent of how the tables are stored.

## Decisions

- RhizoDB's own engine, decided in the storage engine doc, stores every database.
- The storage layer sits behind a trait so it can be swapped.
- A scan of the storage trait can leave each value the store keeps apart from its key unread and give its length instead, for a read that needs few of a range's large values.
- Values are stored as bytes in a format RhizoDB defines, and the format carries a version.
- Each format version can hold everything the older versions hold.
- Opening a path with no file, or an empty one, writes the new database under a staging name in the same directory, syncs it, and links it into place, so a power loss during creation leaves either no database or a whole empty one.
- The staging file's own lock decides which of several concurrent opens creates the database, and the others wait for it instead of replacing it.
- Opening an existing file that the engine cannot read never rewrites it, since it may be a damaged database.
- Opening a database written in an older format version as its writer upgrades it before anything else reads it, in a sequence of steps that each run in commits of bounded size, so a file of any size upgrades within bounded memory and log records.
- Each commit of an upgrade records the format version it upgrades to, the step, and the cursor it reached, so an upgrade cut off by a crash resumes there at the next open.
- A build refuses as a newer format an upgrade that records a newer target than its own, and resumes one of an older target with its own plan, since a step keeps its code and its cursor's meaning in every build.
- Resuming an upgrade of an older target starts at the first step that target lacked when that step comes before the one recorded, since every step can restart from its beginning.
- A step charges its budget with the bytes each element stages, its index rows and the growth of the posting blocks it holds, so a commit stays near its budget however many terms a text holds.
- While an upgrade is under way the format version holds a mark of the version it started from, which no build reads as a graph, so no build ever reads a graph half moved to a newer layout.
- The last commit of an upgrade removes its progress and records the current format version and tokenizer.
- A step is a function from a cursor and a byte budget to the next cursor, so a later format adds its migration as one more step.
- Opening a database written in a newer format version fails.
- Opening a database written in a format version older than the first one RhizoDB's own engine wrote fails the same way.
- Opening a database whose keyword indexes another tokenizer built rebuilds them as the last step of the upgrade, before anything else reads them.
- The rebuild clears an index by deleting its entries one key at a time in commits of bounded size, since a range delete marks every key it covers within one commit.
- Opening a database written in an older format version does not rebuild its keyword indexes for the upgrade alone.
- Opening a database written before posting blocks moves each posting into its block as a step of the upgrade, without tokenizing again.
- Adding a kind of index changes the format version, so an older build refuses the database instead of failing on an unknown definition.
- `Database::backup_to` copies every table of one read snapshot into a new database file of the same backend, so restoring a backup is opening it.
- A backup copies through the storage trait, and each backend writes the new file itself and returns once that one file holds the copy durably.
- A backup leaves out the legacy tables, which only the upgrade of an older format reads.
- A backup commits its copy in write transactions of a bounded size, so its memory does not grow with the database.
- A backup takes no write lock, so writers carry on while it runs and their commits stay out of the copy.
- A backup is written under a staging name that holds the target's file name, the process, and a counter, in the target's directory, synced, and hard-linked into place, so the target holds nothing or the whole copy.
- A backup never replaces an existing file or directory, and fails as a storage error instead.
- A backup on a file system without hard links is renamed into place with a rename that fails when the target exists, and fails as a storage error on platforms or file systems without such a rename.
- A failed backup removes its staging file, and a crashed one leaves it behind for the host to delete.
- The own engine closes its copy before linking it, which checkpoints the whole log into the file, and fails the backup when a log segment is left.
- When the own engine fails its copy, it removes the copy's log segments too, since they belong to a staging file that no later open finds.

## Open

None
