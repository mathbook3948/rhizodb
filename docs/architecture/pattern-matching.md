# Pattern matching

Decides where a match of a pattern starts, when its filter is tested, and how it reads relationship records.

## Background

A pattern part was always matched from its first node and its `WHERE` tested only on complete matches. Queries whose selective node is the last one, or whose filter rejects most partial matches early, read far more of the graph than their answer needs.

## Decisions

- A pattern part whose start node has neither an ID lookup nor an index matches from its end node when that node is bound or has one.
- A pattern part written from its end keeps the relationship directions it matches, so the rows it gives are the same and only their order may differ.
- A pattern part bound at its end and without any lookup at its start is written from its end when the query text is bound to a plan, and any other part keeps both forms and chooses when it runs, once it knows which indexes exist.
- A pattern part is never written from its end when it has a path variable, a variable-length relationship, a variable it repeats, or a property map that reads a variable it binds.
- A `MATCH` filter tests each of its conditions as soon as the elements the condition reads are bound, and the whole filter again on the complete match.
- A filter is tested early only when every condition reads only elements its pattern part binds and is a comparison, `IS NULL`, `IN` a literal list, a string test, or `AND`, `OR`, `XOR`, and `NOT` of those, so no early test can hide an error another condition would raise.
- A query reads a relationship chunk in place the first time and holds it from the second time, so a scan reads each chunk from storage once and a traversal that touches a chunk once copies nothing.
- A query forgets the relationship chunks it holds when it stores a relationship, and when they pass a size set in code.
- A query holds the relationships it read at a node, by direction and type, from the second time it expands that node the same way, so a traversal that reaches a node again reads its adjacency from storage at most twice.
- A query forgets the adjacency it holds when it changes adjacency, and when it passes a size set in code.

## Open

None
