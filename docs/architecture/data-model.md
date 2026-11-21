# Data model

Decides how nodes, relationships, and values are laid out inside the storage layer.

## Background

Every layer above storage, from the executor to the TCK adapter, depends on how a graph maps to keys and bytes. This layout is the most expensive thing to change later, so it is fixed before query features grow on top of it.

## Decisions

- Node and relationship IDs are separate `u64` counters that only grow. A deleted ID is never reused.
- Labels, relationship types, and property keys are interned to `u32` tokens in a dictionary table. Each kind has its own token space.
- The storage trait is an ordered key-value store over a fixed set of tables, and the graph layer is written once on top of it.
- Keys are fixed-width big-endian integers, so key order matches ID order and prefix scans find adjacency and label members.
- Each relationship is indexed twice, under its source node for outgoing scans and under its target node for incoming scans. The index entry holds the node at the other end, so a traversal reads no relationship record.
- Each node label has an index from label to node ID.
- Stored property values are null-free. Setting a property to null removes it.
- A stored value is a boolean, integer, float, string, byte string, or a list of stored values. Maps, nodes, relationships, and paths exist only while a query runs.
- Stored lists have a fixed nesting limit, enforced on write as well as on read.
- The database records one format version in a metadata table, and opening a database with an unknown version fails.
- Each query runs in its own transaction. redb allows one writer and many concurrent readers.

## Open

- Temporal and duration values
- Explicit multi-statement transactions in the public API
