# Transactions

Decides how queries group into transactions and what each transaction sees.

## Background

A host often needs several queries to read one state of the graph or to take effect together. The storage engine gives snapshot reads and one writer at a time, so the public transactions follow its model.

## Decisions

- `Database::execute` runs each query in its own transaction, which commits when the query succeeds.
- The host can also begin a read transaction or a write transaction and run several queries in it.
- A read transaction sees the graph as it was committed when the transaction began, whatever commits after.
- A write transaction sees its own writes, and other transactions see them only once it commits.
- One write transaction is open at a time, and beginning another waits until the open one ends.
- A write transaction that is dropped without a commit is rolled back.
- An updating query in a read transaction fails with its own error kind before it reads data.
- A query that fails in a read transaction leaves the transaction usable.
- A query that fails in a write transaction rolls back the whole transaction.
- A write transaction that a failed query rolled back refuses every later query and its commit with its own error kind.

## Open

- Whether the `transaction` clock reads one instant per transaction instead of one per query
