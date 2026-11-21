# Storage

Decides how RhizoDB keeps data on disk.

## Background

Writing a storage engine from scratch is the riskiest part of a database. Delegating it keeps the first versions stable while leaving room to replace it later.

## Decisions

- redb handles disk storage.
- The storage layer sits behind a trait so it can be swapped.
- Values are stored as bytes in a format RhizoDB defines, and the format carries a version.

## Open

None
