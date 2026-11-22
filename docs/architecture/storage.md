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
- Opening a database written in an older format version rebuilds its keyword indexes in the same write transaction.
- Index definitions and index entries live in tables of their own, so a database from before indexes upgrades with no index.

## Open

None
