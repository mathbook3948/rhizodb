# Pattern starts

Decides which node of a `MATCH` pattern a match begins at and how it walks the pattern from there.

## Background

A match that always begins at the leftmost node reads every node of its label, or of the graph, even when a later node is already bound or an index finds it directly. Beginning at that node instead turns such a match into a walk from one node.

## Decisions

- A pattern part whose leftmost node is unbound begins at its leftmost node that an earlier clause or an earlier part of the same `MATCH` binds.
- Otherwise the match picks its start when it runs: the leftmost node that an ID lookup or an index lookup by `=` or `IN` serves, else the leftmost node that an index range serves, else the leftmost node.
- A lookup serves a node as it serves a leftmost one, by the node's label or the type of the first relationship walked from it.
- The size of a label does not choose the start.
- A part keeps its leftmost start when a property map in it reads an element that the part itself binds.
- A match walks from its start toward the leftmost node with each relationship's direction flipped, then from its start toward the rightmost node.
- A path and a variable-length relationship list hold their elements in the order the pattern writes them, whatever the start.
- Pattern predicates, pattern comprehensions, and `COUNT` patterns begin at a bound node the same way, and `MERGE` and `INSERT` patterns keep their written order.
- A match that begins right of its leftmost node does not take the column hop aggregate or split across threads, since both read the leftmost node's label.
- Without `ORDER BY`, the rows of a match come in the order its walk finds them, so a moved start may reorder them.
- A value error in a property map is raised only for the elements the walk from the start reaches.

## Open

- A pattern with no bound node and no lookup begins at its leftmost node even when a later label holds far fewer nodes, since the engine keeps no relationship counts per node to tell whether that start reads less.
- The order of pattern parts in one `MATCH` stays as written.
