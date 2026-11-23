# Data model

Decides how nodes, relationships, and values are laid out inside the storage layer.

## Background

Every layer above storage, from the executor to the TCK adapter, depends on how a graph maps to keys and bytes. This layout is the most expensive thing to change later, so it is fixed before query features grow on top of it.

## Decisions

- Node and relationship IDs are separate `u64` counters that only grow.
- A deleted ID is never reused.
- Labels, relationship types, and property keys are interned to `u32` tokens in a dictionary table.
- Each kind of token has its own token space.
- The database remembers each committed token that a read transaction looks up, and a write transaction never adds to that memory.
- The storage trait is an ordered key-value store over a fixed set of tables, and the graph layer is written once on top of it.
- Keys of the node, relationship, adjacency, and label tables are fixed-width big-endian integers, so key order matches ID order and prefix scans find adjacency and label members.
- Each relationship is indexed twice, under its source node for outgoing scans and under its target node for incoming scans.
- An adjacency index entry holds the node at the other end, so a traversal reads no relationship record.
- Each node label has an index from label to node ID.
- Stored property values are null-free.
- Setting a property to null removes it.
- A stored value is a boolean, integer, float, string, byte string, temporal value, or a list of stored values.
- Maps, nodes, relationships, and paths exist only while a query runs.
- The temporal values are the openCypher ones: date, local time, time, local datetime, datetime, and duration.
- Temporal values have nanosecond precision on the proleptic Gregorian calendar, with years from -999,999,999 to 999,999,999.
- A time keeps its UTC offset.
- A datetime keeps its instant and either a UTC offset or an IANA zone name.
- The offset of a datetime in a named zone is derived from the zone's rules at its instant.
- A duration keeps months, days, seconds, and nanoseconds apart.
- The nanoseconds of a duration are always below one second, so a negative duration has negative seconds and a non-negative fraction.
- Time-zone rules come from the IANA database that the jiff crate bundles into the build, so results do not depend on the host.
- A temporal value is stored as its tag and fixed-width big-endian fields: a date as its day number from 1970-01-01, a time of day as nanoseconds since midnight, an offset as signed seconds, an instant as seconds from the Unix epoch and nanoseconds, a zone name as UTF-8 text.
- Adding a stored value type changes the format version, so an older build refuses the database instead of failing on an unknown tag.
- A non-empty list whose items are all floats that single precision holds bit for bit is stored as four bytes a float.
- Stored lists have a fixed nesting limit, enforced on write as well as on read.
- The database records one format version in a metadata table.

## Open

None
