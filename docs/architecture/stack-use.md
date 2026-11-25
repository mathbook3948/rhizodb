# Stack use

Decides how RhizoDB keeps every query within a bounded thread stack, whatever its input.

## Background

A host calls RhizoDB on threads of its own, often with the 2 MiB stack Rust gives a spawned thread, and a stack overflow aborts the whole host process. Query text, parameters, procedure results, and the shape of the graph all come from outside RhizoDB.

## Decisions

- A query, from parsing its text to returning its last value, fits in 1 MiB of thread stack in both debug and release builds, so a thread with the 2 MiB default keeps half of it for the host.
- No input can overflow the stack: each recursion is either bounded by a limit that fails with an error kind or replaced by a loop over a stack on the heap.
- The parser bounds how deeply query text nests and fails past that with `SyntaxErrorKind::NestingTooDeep`.
- Every pass after parsing, from binding and type checks to evaluation and dropping the plan, recurses no deeper than the parser allowed.
- A function that recurses over the query keeps its frame small, so a large `match` hands each case to a function of its own and large syntax tree variants are boxed.
- A value in a query nests lists and maps at most 128 levels deep.
- A parameter, a procedure result, or a list or map a query builds past that depth fails with `ErrorKind::NestingTooDeep`.
- A parameter's depth is checked with a loop before it is converted, and a procedure result that is too deep is dropped with a loop, so a host value of any depth is refused without recursing over it.
- Pattern matching keeps its search on a stack on the heap, so neither the number of steps in a pattern nor the length of a variable-length path deepens the call stack.
- A regular expression nests at most 64 groups deep and fails past that with `ErrorKind::InvalidRegex`.
- Stored property values keep their own, lower nesting limit.

## Open

None
