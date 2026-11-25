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
- A `MATCH` of one hop of fixed length from such a node pattern, with no property map on the relationship or the far node, that feeds an aggregating projection reads its start node as a scan and follows the hop from each node the scan keeps.
- Such a hop takes its filter only when the filter reads neither the relationship nor the far node, and evaluates whatever the scan cannot read on the matched row.
- Expanding consecutive nodes reads their adjacency segments from storage in one pass per window of segments, which starts at one segment and doubles up to 16 while the reads stay consecutive, so a traversal that only now and then reads the next segment reads no more than before.
- The far node of such a hop is tested against its pattern once, and when every group key reads only the far node or values fixed for the input row, each far node finds its group once.
- A plain projection right after a scan whose items, sort keys, and distinct columns are of that kind is computed on the scan.
- `ORDER BY` with `LIMIT` on a scan keeps the best `SKIP` plus `LIMIT` nodes by their sort keys in a bounded heap, ties broken by scan order, and evaluates the other items only for the nodes it keeps.
- Once that heap is full, a node's sort keys are read one at a time, and the node is dropped at the first key that ranks it behind the last node kept.
- `LIMIT` without `ORDER BY` stops the scan once it has the rows it returns.
- Grouping and `DISTINCT` find a row of scalar values by a hash seeded per query that equal integers and floats share, and keep every other row in order, so both still group exactly as the global sort order equates values.
- A scan tests stored strings as bytes and checks their UTF-8 only where it takes a string out, so a damaged segment that passes its checksum fails where its text is returned, grouped, or kept distinct.
- Tests compare every scan with the row-at-a-time path, which they can force.

## Open

None
