# Public API

Decides how the public API of `rhizodb` is shaped so later releases can grow it without breaking hosts.

## Background

Hosts embed `rhizodb` as a library, so every public type, field, and signature is a promise. Before the first release the API is shaped so that the changes already planned, such as streaming results and new settings, add to it instead of breaking it.

## Decisions

- Every public enum is `#[non_exhaustive]`, and so is every struct with public fields.
- Result types keep their fields private and expose accessor methods, so a field can be added or stored differently.
- `Parameters` and `ProcedureOutput` are types of their own instead of aliases of standard types.
- A procedure body fails with the standard `Box<dyn Error + Send + Sync>`, named `ProcedureError`, so any error converts with `?` and the query keeps it as the source of its error.
- `QueryResult` holds every row in memory by contract, and its slice of rows stays.
- Streaming results will come as a separate cursor type returned by new methods, whose rows can fail one by one, while `execute` keeps returning `QueryResult`.
- New database settings go on `OpenOptions`, and `Database::open` stays the same as opening with the default options.
- Procedures register through a shared reference at any time, so a database inside an `Arc` can still gain them.
- `NodeId` and `RelationshipId` convert to and from `u64`, so a host can store an ID and compare it later.
- Functions that only the fuzz targets call exist only with the non-default `fuzzing` feature and are not part of the API.
- `Error` implements `std::error::Error` and is `Send`, `Sync`, and `'static`.
- Every public type is `Send` and `Sync`, except that `ReadTransaction` is only `Send` and `WriteTransaction` is neither, and a test asserts it.
- No public signature names a type that hosts cannot name.

## Open

- The shape of the streaming cursor and whether rows become a type of their own.
