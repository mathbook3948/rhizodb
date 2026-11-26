# Path search

Decides how RhizoDB finds the paths of a pattern part with a selector, a path mode, or a quantified path pattern.

## Background

Such a part can match unboundedly many walks, and a selector keeps only a few of them per pair of end nodes. Enumerating every match before choosing would make a shortest path cost as much as all paths.

## Decisions

- A searched pattern part is compiled to a sequence of single hops and groups, where a quantified edge or a variable-length relationship is a group of one hop.
- The part is matched from each of its start nodes, and its matches for that start node are found before the parts after it run.
- The end nodes come from a bound end variable or from the seeks the end node pattern and the `WHERE` of the `MATCH` give, so a search can stop once every end node has its paths.
- A search that is one group of one hop, with a selector of one path or one group, runs breadth first on the graph, under every path mode.
- That search runs from both ends at once when its end node is known, a whole layer at a time from the side with the smaller frontier.
- Its closed paths back to the start node are found apart, with one breadth-first search per first relationship when hops go either way under `TRAIL`.
- Each layer of a breadth-first search is read in node ID order, so adjacency reads move forward through storage.
- Any other selective `WALK` search runs breadth first over pairs of a node and a place in the pattern, each pair kept for no more lengths or paths than the selector can use, which bounds the search.
- Any other selective search under `TRAIL`, `SIMPLE`, or `ACYCLIC` walks the paths depth first one length at a time, longer each round, until every end node has its paths or no path grows.
- A search without a selector walks the paths depth first.
- Only the paths a selector keeps are built, and a breadth-first search keeps one parent per node unless it must return every shortest path.

## Open

- Whether a search whose start node is unknown and whose end node is known should run backward from the end node.
