# Column scans

Decides how a query that reads every node of a label tests, sorts, groups, and returns its properties.

## Background

Node properties are stored as typed columns per row group, but the executor evaluated each node as a row of decoded values. A scan over many nodes spent most of its time building rows and values for nodes it then discarded.

## Decisions

- A `MATCH` of one node pattern, with at most a label and a property map, reads the nodes of the label or every node in ID order and tests them on their stored values in place.
- Such a scan takes only conditions that cannot fail: comparisons, null tests, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, and `IN` over a fixed list, each over the node, its properties, literals, parameters, and earlier variables, joined by `NOT`, `AND`, `OR`, and `XOR`.
- A condition outside that set, an optional match, or a pattern a property index or an ID lookup serves leaves the clause to the row-at-a-time path.
- Because every condition it takes cannot fail, a scan answers and fails exactly as testing each row in turn does, whichever columns it reads first.
- A scan reads each row group's columns for the properties it tests whole, once, and builds a row only for a node that passes.
- A value read by a scan is the value the transaction sees, its own uncommitted changes and the stored deltas included.
- An aggregating projection right after a scan reads its keys and arguments from the scan when each is the node, one of its properties, or a value fixed for the input row.
- A plain projection right after a scan whose items, sort keys, and distinct columns are of that kind is computed on the scan.
- `ORDER BY` with `LIMIT` on a scan keeps the best `SKIP` plus `LIMIT` nodes by their sort keys in a bounded heap, ties broken by scan order, and evaluates the other items only for the nodes it keeps.
- `LIMIT` without `ORDER BY` stops the scan once it has the rows it returns.
- Grouping and `DISTINCT` find a row of scalar values by a hash seeded per query that equal integers and floats share, and keep every other row in order, so both still group exactly as the global sort order equates values.
- Tests compare every scan with the row-at-a-time path, which they can force.

## Open

None
