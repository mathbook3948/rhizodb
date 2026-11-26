# Public API

Decides how the public API of `rhizodb` is shaped so later releases can grow it without breaking hosts.

## Background

Hosts embed `rhizodb` as a library, so every public type, field, and signature is a promise. Before the first release the API is shaped so that the changes already planned, such as streaming results and new settings, add to it instead of breaking it.

## Decisions

- Every public enum is `#[non_exhaustive]`, and so is every struct with public fields.
- Result types keep their fields private and expose accessor methods, so a field can be added or stored differently.
- `Node` and `Relationship` return their labels as a `BTreeSet` and their properties as a `BTreeMap`, like `Value::Map`, so hosts look up and iterate them in sorted order, and storing them as other types is a breaking change.
- `Parameters` and `ProcedureOutput` are types of their own instead of aliases of standard types.
- A procedure body fails with the standard `Box<dyn Error + Send + Sync>`, named `ProcedureError`, so any error converts with `?` and the query keeps it as the source of its error.
- `QueryResult` holds every row in memory by contract, and its slice of rows stays.
- Streaming results will come as a separate cursor type returned by new methods, whose rows can fail one by one, while `execute` keeps returning `QueryResult`.
- New database settings go on `OpenOptions`, and `Database::open` stays the same as opening with the default options.
- Procedures register through a shared reference at any time, so a database inside an `Arc` can still gain them.
- `NodeId` and `RelationshipId` convert to and from `u64`, so a host can store an ID and compare it later.
- Functions that only the fuzz targets call exist only under `cfg(fuzzing)`, which cargo-fuzz sets, so no cargo feature exposes them and they are not part of the API.
- A storage failure that a host handles differently from the rest, such as a file already open elsewhere or a backup target that exists, has an error kind of its own, and every other storage failure stays `ErrorKind::Storage`.
- `Value`, `Node`, `Relationship`, and `Path` implement `Display` as GQL text without element IDs, so a host prints a result without writing a formatter and reads typed accessors when it needs data.
- The `Display` text of null, booleans, integers, finite floats, strings, temporals, and lists and maps of them is a GQL expression that evaluates back to an equal value in every later release, while the text of bytes, non-finite floats, and elements is for reading only.
- A datetime in a named zone reads back from its `Display` text through the zone rules of the reading build, so a changed rule can move it to another instant.
- `Error` implements `std::error::Error` and is `Send`, `Sync`, and `'static`.
- `Error::byte_offset` returns the position of a syntax error in the query text and is `Some` exactly when the kind is `ErrorKind::Syntax`.
- Every public type is `Send` and `Sync`, except that `WriteTransaction` is neither, and tests assert both.
- No public signature names a type that hosts cannot name.

## Open

- The shape of the streaming cursor and whether rows become a type of their own.
