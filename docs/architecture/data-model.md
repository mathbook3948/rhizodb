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
- Keys of the node, relationship, and adjacency tables are fixed-width big-endian integers, so key order matches ID order and prefix scans find adjacency members.
- Each relationship is indexed twice, under its source node for outgoing scans and under its target node for incoming scans.
- An adjacency index entry holds the node at the other end, so a traversal reads no relationship record.
- Nodes fall into row groups of 4096 consecutive IDs.
- Each row group with a live node has one node meta chunk, keyed by the group, that holds a live bitmap and each live node's label set.
- A node record holds the node's properties, and the node's labels live only in node meta.
- Each distinct set of labels is interned once to a `u32` label set ID in two tables, one by ID and one by labels.
- Label set ID 0 is the empty set and has no stored entry.
- Label sets are never deleted.
- A node meta chunk lists the label sets its group uses and stores each slot's position in that list, with no positions when the group uses one label set.
- A node meta chunk keeps the live count of each label in its group.
- Counting the nodes of one label reads only the label counts of each group.
- A label scan walks the node meta chunks in group order and yields node IDs in ascending order.
- There is no table from label to node ID.
- A write transaction edits node meta in memory and writes each changed chunk once, at commit.
- A write transaction reads each ID and token counter once and writes it once, at commit.
- Opening a database written before node meta moves each node's labels into node meta and empties the old label index, in the opening write transaction.
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
