# Storage

Decides how RhizoDB keeps data on disk.

## Background

Writing a storage engine from scratch is the riskiest part of a database. Delegating it keeps the first versions stable while leaving room to replace it later.

## Decisions

- redb handles disk storage.
- The storage layer sits behind a trait so it can be swapped.
- Values are stored as bytes in a format RhizoDB defines, and the format carries a version.
- Each format version can hold everything the older versions hold.
- Opening a database written in an older format version records the current version in the same write transaction, before anything else reads it.
- Opening a database written in a newer format version fails.
- Opening a database whose keyword indexes another tokenizer built rebuilds them in the same write transaction, before anything else reads them.
- Opening a database written before postings kept term positions rebuilds its keyword indexes in the same write transaction.
- Opening a database written in an older format version whose postings already kept positions does not rebuild its keyword indexes for the upgrade alone.
- Index definitions and index entries live in tables of their own, so a database from before indexes upgrades with no index.
- Opening a database written before indexes could cover relationships rewrites each index definition as one over a label, in the same write transaction.
- Adding a kind of index changes the format version, so an older build refuses the database instead of failing on an unknown definition.

## Open

None
