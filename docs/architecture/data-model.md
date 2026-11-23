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
- A node's labels live only in node meta, and its properties live only in property columns.
- Each distinct set of labels is interned once to a `u32` label set ID in two tables, one by ID and one by labels.
- Label set ID 0 is the empty set and has no stored entry.
- Label sets are never deleted.
- A node meta chunk lists the label sets its group uses and stores each slot's position in that list, with no positions when the group uses one label set.
- A node meta chunk keeps the live count of its group and of each label in its group.
- Counting the nodes of one label, or all nodes, reads only the counts of each group.
- A count trusts each delta's state in the chunk, and every other read of a delta checks it against the chunk.
- A label scan walks the node meta chunks in group order and yields node IDs in ascending order.
- There is no table from label to node ID.
- A write transaction edits node meta in memory and writes it once, at commit.
- A write transaction decodes all slots of a group only when it rewrites the group's chunk or a read needs every slot.
- A commit that changes a few slots of a group with a stored chunk writes one small delta per changed slot beside the chunk instead of rewriting it.
- A delta holds its slot's state in the chunk and its state now, so a count applies it without reading the chunk's slots.
- A group keeps at most 64 deltas, and a commit that would leave more rewrites the chunk and deletes them.
- A node meta chunk keeps one checksum over its counts and dictionary and one over its slots, so a count checks only the first.
- A node meta chunk holds its group inside its first checksum, and a delta's checksum covers its key, so an entry read under another key is reported as corruption.
- Node meta checksums are CRC-32, which needs no dependency outside the allowed licenses.
- A write transaction reads each ID and token counter once and writes it once, at commit.
- Opening a database written before node meta moves each node's labels into node meta and empties the old label index, in the opening write transaction.
- Each row group keeps one property column per property key that a live node of the group holds, stored as segments of 64 slots.
- A segment is stored as one or more parts, keyed by the group, the key, the segment, and the part, with each number in its fewest big-endian bytes.
- A part stays small enough that the storage engine packs four or more parts with their keys into one page, so no stored value spans a page and reading one value copies little.
- A segment that does not fit one part is split into parts that each cover the same run of slots, the longest run under which every part fits, with the last part ending at the end of the segment.
- A part records the run of slots each part of its segment covers, and the first part of a split segment is stored even without values, so a reader finds the part that holds a slot with at most two reads.
- A part holds a presence bitmap of its slots and the values of the present slots in slot order.
- A part whose values all have one type stores them in that type's encoding: booleans as bits, integers and floats as eight bytes each, strings as end offsets and one byte heap.
- A part whose values differ in type, or are of another type, stores each value in its tagged value encoding.
- A value of one of the four column types in a mixed segment reads back as that type, so a segment re-encoded from its values is the same bytes.
- Only a live node's slot holds a property value, so deleting a node removes its value from every column of its group.
- A segment with no value is not stored.
- Each node with properties that differ from its group's segments keeps one delta, keyed by the group and the slot, that holds each such property's value now or that it holds none.
- Deltas sort before every segment, so a load that fills groups in order writes each segment past the end of the column table, where the storage engine packs entries into full pages, and a scan of one column's segments reads no delta.
- A commit writes the column entries of all its groups in key order.
- A commit that creates or changes a few nodes of a group writes or replaces one delta per node and leaves the segments as they are.
- A node created in a transaction holds no stored value, so its delta is written without reading the segments.
- A transaction keeps the properties of the nodes it creates encoded in one buffer per group until it commits, so a large load holds few allocations.
- A delta entry whose value is the segment's value again is dropped, with values compared bit for bit, so `-0.0` and `0.0` differ, and a delta left with no entry is deleted.
- A group keeps at most 64 deltas, and a commit that would leave more folds them into the segments they touch and deletes them with one range delete.
- A write transaction takes the number of deltas a group stores from the count the last commit that changed the group left in memory, and scans the group's delta keys only for a group no commit changed since the database opened.
- A commit that fails forgets every count, since the storage may hold either side of it.
- A read of one value reads the node's delta and then the part that holds the value, and a query that reads a second node of a group loads all of the group's deltas once.
- A query that reads one column a second time loads and checks its segments once, with one prefix scan.
- A query reads only the columns of the properties it reads.
- Listing a node's properties seeks to the first segment of each column of its group and reads the node's delta.
- A part's checksum covers its key, and a delta's checksum covers its key, so an entry read under another key is reported as corruption.
- Setting a property updates only the indexes on a label of the node that cover that property, and changing a label updates only the indexes on that label.
- Opening a database written before property columns moves each node record's properties into property columns and deletes the records, in the opening write transaction.
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
