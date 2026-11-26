An embedded, schemaless property graph database queried with GQL.

RhizoDB runs inside the host program, with no server. A [`Database`] is one file and its log,
or memory, that holds nodes with labels, relationships with a type, and properties on both. Queries are GQL,
and Cypher's `CREATE` is accepted as GQL `INSERT`.

```
use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;

let db = Database::in_memory()?;
db.execute(
    "INSERT (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})",
    &Parameters::new(),
)?;

let mut parameters = Parameters::new();
parameters.insert("name".to_owned(), Value::String("Ada".to_owned()));
let result = db.execute(
    "MATCH (a:Person {name: $name})-[:KNOWS]->(b) RETURN b.name AS friend",
    &parameters,
)?;
assert_eq!(result.columns(), ["friend"]);
assert_eq!(result.rows(), [vec![Value::String("Grace".to_owned())]]);
# Ok::<(), rhizodb::Error>(())
```

# Opening a database

[`Database::open`] opens a file and creates it when there is none. A new file is written under a
staging name beside it, synced, and linked into place, so a power loss during creation leaves
either no file or a whole empty database. A file that is already open, in this process or
another, fails to open with [`ErrorKind::DatabaseAlreadyOpen`], so two processes never write one
file. The check needs file locks, so on a file system without them, such as some network
mounts, a second open succeeds and the host must keep to one `Database` per file.
[`OpenOptions`] opens with other settings, such as failing with
[`ErrorKind::DatabaseNotFound`] instead of creating a missing file.
[`Database::in_memory`] makes a database that is gone when dropped.

A damaged file fails with [`ErrorKind::CorruptData`] when it is opened or when an operation
reads the damage, since every page of the file carries a checksum.

# Queries and parameters

[`Database::execute`] runs one query in its own transaction and returns a [`QueryResult`] that
holds every row. Pass values as parameters, written `$name` in the query, rather than building
query text, so a value is never read as syntax and a query text is bound once and reused for
other values. A [`Value::Node`] or [`Value::Relationship`] passed as a parameter refers to the
stored element by its ID.

Node and relationship IDs, [`NodeId`] and [`RelationshipId`], only grow and are never reused
after a delete, so a host may keep them as references. `id(n)` reads an ID in a query, and
`MATCH (n) WHERE id(n) = $id` reads one node by ID without a scan.

Stored property values are booleans, integers, floats, strings, byte strings, the temporal
values of [`Temporal`], and lists of those. Setting a property to null removes it, and maps,
nodes, relationships, and paths cannot be stored.

# Transactions and threads

A `Database` is `Send` and `Sync`. Share one, for example in an `Arc`, across every thread of
the host, and open each file once.

```
fn shared<T: Send + Sync>() {}
shared::<rhizodb::Database>();
```

Each `execute` call on a `Database` is its own transaction. A query without updating clauses
reads a snapshot, and one with updating clauses commits when it succeeds and leaves no trace
when it fails.

[`Database::begin_read`] begins a [`ReadTransaction`], a snapshot that sees the graph as it was
committed when it began, whatever commits after. Read transactions run alongside each other and
alongside the writer, and a query that fails in one leaves it usable.

[`Database::begin_write`] begins a [`WriteTransaction`], whose queries take effect together when
it commits. Its queries see its own writes, and other transactions see them only after
[`WriteTransaction::commit`]. Dropping it without a commit rolls it back. A query that fails in
it rolls the whole transaction back, and every later call on it fails with
[`ErrorKind::TransactionAborted`].

```
use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;

let db = Database::in_memory()?;
db.execute("INSERT (:Person {name: 'Ada'})", &Parameters::new())?;

let mut tx = db.begin_write()?;
tx.execute("INSERT (:Person {name: 'Alan'})", &Parameters::new())?;
tx.execute(
    "MATCH (a:Person {name: 'Alan'}), (b:Person {name: 'Ada'}) INSERT (a)-[:KNOWS]->(b)",
    &Parameters::new(),
)?;
tx.commit()?;

let snapshot = db.begin_read()?;
let result = snapshot.execute(
    "MATCH (:Person)-[k:KNOWS]->(:Person) RETURN count(k) AS friendships",
    &Parameters::new(),
)?;
assert_eq!(result.rows(), [vec![Value::Integer(1)]]);
# Ok::<(), rhizodb::Error>(())
```

One write transaction is open at a time, and a writing `execute` holds it for its query. Beginning
another waits until the open one ends. On the thread that holds the open write transaction, a
second write fails with [`ErrorKind::WriteTransactionOnThisThread`] instead of waiting for
itself. A `WriteTransaction` is not `Send`, so it stays on the thread that began it. An async
host runs a write transaction on one thread, for example inside `spawn_blocking`, rather than
holding it across an `.await`. Keep write transactions short, since every other writer waits for
them, and readers never wait.

# Errors

Every failure is an [`Error`], and [`Error::kind`] gives its [`ErrorKind`]. Branch on the kind,
never on the message, which may change. `ErrorKind` is `#[non_exhaustive]`, so a match on it
needs a catch-all arm.

Every method that reads or writes the database can fail with [`ErrorKind::Storage`] or
[`ErrorKind::CorruptData`], described below, and every call that creates data, such as a writing
query, an index creation, or an import, can fail with [`ErrorKind::IdSpaceExhausted`]. A method's
own docs leave these out and name only the kinds it adds, and a method whose docs name none fails
only with these.

Most kinds are decided by the query and the data, and running the same query on the same data
fails the same way, so retrying does not help. These are the syntax and binding errors, type
errors, value errors such as [`ErrorKind::IntegerOverflow`], and misuse such as
[`ErrorKind::ReadOnlyTransaction`] or [`ErrorKind::WriteTransactionOnThisThread`].

The kinds a host may retry are these.

- [`ErrorKind::Storage`]: the file system failed, such as a full disk or an I/O error. After a
  write or sync of the database fails, that `Database` keeps serving reads but refuses every
  later write until the file is opened again. Once the cause is gone, drop it, open the file
  again, and retry the whole transaction. A failed commit may or may not have taken effect, so
  retry only a transaction that is safe to run twice, such as one built with `MERGE`, or read
  the graph first to find out.
- [`ErrorKind::TransactionAborted`]: a query failed earlier in the same write transaction. Begin
  a new transaction and run all of its queries again, after fixing what made the first query
  fail.
- [`ErrorKind::Io`]: the reader or writer of an export or import failed.
- [`ErrorKind::DatabaseAlreadyOpen`]: another `Database` holds the file. Open it again once
  that one is dropped.

[`ErrorKind::CorruptData`] means a page of the file fails its checksum or breaks an invariant
the file records. Restore from a backup when opening the file again does not clear it.
[`ErrorKind::UnsupportedFormatVersion`] means a newer build wrote the file, or a pre-release
build wrote it in a format older than any this build upgrades.

# Indexes

Every index lives in the database and changes in the same transaction as the data it covers,
so a search never disagrees with committed data. An index covers the nodes of one label or the
relationships of one type, named by an [`IndexTarget`]. Creating an index fills it from the data
already there, and creating one that exists with the same definition changes nothing.

- [`Database::create_property_index`] indexes one property for lookups by value. Queries never
  name it: `MATCH (p:Person {id: $id})` and `WHERE p.age >= 18` use it whenever they can.
- [`Database::create_keyword_index`] indexes the words of string properties, searched with
  `CALL db.index.keyword.queryNodes(name, text, limit)` or `queryRelationships`, ranked by BM25.
- [`Database::create_vector_index`] indexes a list of numbers of a fixed dimension, searched with
  `CALL db.index.vector.queryNodes(name, vector, limit)` or `queryRelationships`. Vector search
  is exact and reads every vector of the index on each query.

```
use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::VectorMetric;

let db = Database::in_memory()?;
db.create_keyword_index("bios", IndexTarget::Label("Person"), &["bio"])?;
db.create_vector_index("faces", IndexTarget::Label("Person"), "face", 2, VectorMetric::Cosine)?;
db.execute(
    "INSERT (:Person {name: 'Ada', bio: 'wrote the first program', face: [1.0, 0.0]}), \
            (:Person {name: 'Grace', bio: 'wrote a compiler', face: [0.0, 1.0]})",
    &Parameters::new(),
)?;
let words = db.execute(
    "CALL db.index.keyword.queryNodes('bios', 'compiler', 10) YIELD node RETURN node.name",
    &Parameters::new(),
)?;
assert_eq!(words.rows().len(), 1);
let nearest = db.execute(
    "CALL db.index.vector.queryNodes('faces', [0.9, 0.1], 1) YIELD node RETURN node.name",
    &Parameters::new(),
)?;
assert_eq!(nearest.rows(), [vec![rhizodb::Value::String("Ada".to_owned())]]);
assert!(db.drop_index("faces")?);
# Ok::<(), rhizodb::Error>(())
```

# Backup and restore

[`Database::backup_to`] copies the committed database into a new file while readers and writers
carry on, from one snapshot, so the copy holds exactly the commits that finished before it
began. The file is written under a staging name and linked into place, so its path holds either
nothing or the whole copy, and an existing file is never replaced. Restoring a backup is opening
the copy with [`Database::open`]. A backup opens only in a build that reads its storage format,
which is the format of the build that wrote it or a newer one.

```
use rhizodb::Database;
use rhizodb::Parameters;

let directory = tempfile::tempdir()?;
let db = Database::open(directory.path().join("live.rhizodb"))?;
db.execute("INSERT (:Note {text: 'keep me'})", &Parameters::new())?;
db.backup_to(directory.path().join("nightly.rhizodb"))?;
drop(db);

let restored = Database::open(directory.path().join("nightly.rhizodb"))?;
let result = restored.execute("MATCH (n:Note) RETURN count(n)", &Parameters::new())?;
assert_eq!(result.rows(), [vec![rhizodb::Value::Integer(1)]]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

# Dumps

[`Database::export`] writes a dump, UTF-8 text with one JSON object per line, and
[`Database::import`] reads it into a new database. A dump does not depend on the storage format,
so it moves data across versions and into other tools. It keeps node and relationship IDs, the
next ID of each, labels, index definitions, and the exact type of every value.

The first line is a header that names the format and its version, then come index lines, node
lines in ID order, relationship lines in ID order, and an end line that counts them, so a cut
dump fails to import.

```text
{"type":"header","format":"rhizodb-dump","version":1,"next_node_id":2,"next_relationship_id":1}
{"type":"index","name":"by_name","index":"property","label":"Person","property":"name"}
{"type":"node","id":0,"labels":["Person"],"properties":{"born":1815,"height":1.65,"name":"Ada"}}
{"type":"node","id":1,"labels":["Person"],"properties":{"name":"Grace"}}
{"type":"relationship","id":0,"relationship_type":"KNOWS","start":0,"end":1,"properties":{}}
{"type":"end","nodes":2,"relationships":1}
```

Booleans, strings, and lists are plain JSON. An integer is a JSON number without a fraction or
exponent, and a finite float always has one, so `1` and `1.0` stay apart and `-0.0` keeps its
sign. Every other value is an object with one tagged field.

| Value | Form |
| --- | --- |
| NaN or an infinity | `{"$float":"7ff8000000000000"}`, the bits in hex |
| Byte string | `{"$bytes":"00ff"}` |
| Date | `{"$date":"2024-03-15"}`, with a `-` before a negative year |
| Local time | `{"$localtime":"12:30:00.000000000"}` |
| Time | `{"$time":"12:30:00.000000000+01:00:00"}` |
| Local datetime | `{"$localdatetime":"2024-03-15T12:30:00.000000000"}` |
| Datetime | `{"$datetime":"2024-03-15T12:30:00.000000000+01:00:00[Europe/Paris]"}`, the zone only for a named zone |
| Duration | `{"$duration":{"months":1,"days":2,"seconds":3,"nanoseconds":4}}` |

An import needs a database that holds no index and has never held a node or relationship, and
fails with [`ErrorKind::DatabaseNotEmpty`] otherwise. It commits a batch of lines at a time, so
its memory stays bounded, and a failed import leaves the batches before the failure behind.
Import into a new file and delete it when the import fails.

The version counts up when the format changes. A build reads every version up to its own,
refuses a newer one with [`ErrorKind::UnsupportedDumpVersion`], and writes only its own. A new
version only adds, such as a value tag, so an older dump keeps its meaning.

# Replication

One database is the writer and any number of replicas follow it. The host moves changes between
them and decides which node writes.

- A replica opens with [`OpenOptions::replica`] and refuses every local write.
- [`Database::position`] names the last change a database holds. A replica sends its position to
  another node, which answers with [`Database::changes_since`]. The replica then hands the
  [`Changes`] to [`Database::apply`]. A replica serves changes too, so they can be relayed.
- [`Database::promote`] makes a node the writer of a term the host chose, higher than every term
  before it. [`Database::demote`] stops a writer and returns its final position, which the next
  writer applies before it promotes, so a planned handover loses no write.
- Changes of an older term fail with [`ErrorKind::Fenced`]. Two databases with other changes at
  one place fail with [`ErrorKind::Diverged`], and the host keeps only the side of the node that
  holds its lease.

The host routes every write to the current writer and keeps data that belongs to one node in a
database file of its own. Replication is asynchronous, so a write that no replica applied yet can
be lost with its writer. A host that must not lose one waits until a replica reports a position
at or past the write before it answers.

```
use rhizodb::Database;
use rhizodb::OpenOptions;
use rhizodb::Parameters;

let writer = Database::in_memory()?;
writer.promote(1, b"node-a")?;
writer.execute("INSERT (:Order {id: 1})", &Parameters::new())?;
let replica = OpenOptions::new().replica(true).in_memory()?;
let changes = writer.changes_since(&replica.position(), 1 << 20)?;
replica.apply(&changes)?;
let last = writer.demote(std::time::Duration::from_secs(1))?;
let rest = writer.changes_since(&replica.position(), 1 << 20)?;
assert_eq!(replica.apply(&rest)?, last);
replica.promote(2, b"node-b")?;
replica.execute("INSERT (:Order {id: 2})", &Parameters::new())?;
# Ok::<(), rhizodb::Error>(())
```

# Upgrades

The file records a format version. Opening a file written by an older version of RhizoDB
upgrades it in place, in one write transaction before anything else reads it, and opening one
written by a newer version, or by a pre-release build in a format older than any this build
upgrades, fails with [`ErrorKind::UnsupportedFormatVersion`]. An upgraded file
cannot be opened by the older build again, so take a backup before upgrading the crate. Below
1.0, a release that changes the API or the format raises the minor version, and the changelog
lists the change.

# Durability

A commit returns once its writes are synced to the write-ahead log, a series of files beside the
database file named after it with `-wal-` and a number, and a crash or power loss after that
keeps them. A crash while it runs leaves the database as it was either before or after the
transaction, never with part of it. These guarantees hold for a file system that keeps synced
writes, and no database can keep them on a disk that lies about syncing. A backup is synced
before it is linked into place.

Checkpoints write the log into the database file in the background and remove the log files
they no longer need, and dropping the `Database` checkpoints the rest, which leaves the file
alone. Until then the file holds only part of the commits, so copy a database that is open, or
that closed without dropping its `Database`, with [`Database::backup_to`] rather than by
copying its file.

# Disk space

Writes go to new places in the file, never over the data they replace, and the space they free
is reused by later checkpoints. When the file holds more than twice its live data and another
4 MiB, a checkpoint, or closing the database, also compacts it: it moves the live data to the
start of the file and shortens the file. Reads and writes go on meanwhile, and a writer waits only when its commits
outrun the compaction. [`Database::compact`] compacts at once, [`Database::stats`] reports the
file's size, its live bytes, and its log's size, and [`Database::check`] reads every page and
fails with [`ErrorKind::CorruptData`] on the first that does not hold.

# Limits

A query runs within 1 MiB of thread stack, whatever its input, so a host thread with a small
stack still runs any query. A list or map value nests at most 128 levels inside a query and 64
levels in a stored property, `range()` has a length cap, and a regular expression has a size
cap, each failing with an error kind of its own instead of exhausting the host.
