# Replication

Decides how an embedded database follows the changes of a writer elsewhere and how writers hand over.

## Background

A host that runs RhizoDB on several machines needs copies of one graph that serve reads and can take over writes. Replication stays inside the embedded library, and the host owns leases, transport, and the routing of writes.

## Decisions

- Replication is embedded, with no server, no consensus protocol, and no async runtime in the library.
- One writer changes a database file at a time, and any number of replicas follow it.
- The host owns leases, transport, routing of writes to the writer, and who wins a split.
- Replication is asynchronous, and a host builds semi-synchronous commits from positions and acknowledgements.
- A position is the LSN of the last change a database holds with the term and writer id of the writer that made it.
- Term 0 has no writer id, and every later term has a writer id of 1 to 64 bytes.
- A position converts to versioned bytes, so a host can send it between nodes and store it.
- A database opened as a replica refuses every local write for the whole file with `ReplicaReadOnly`.
- A replica opened on a missing or empty file writes nothing, so it holds no change until it applies its writer's first one.
- A file that needs an upgrade fails to open as a replica, since the upgrade is a write.
- Data that belongs to one node lives in a database file of its own, beside its replica.
- Replication reports its own error kinds: `Fenced` for a lower term, `Diverged` for another writer at the same place, `TooFarBehind` for changes no longer kept, and `Gap` for changes that do not follow the last one held.

## Open

- The change stream, its apply, epochs and their history, fencing, promotion, and demotion.
- Snapshots, their install, and how long a writer keeps changes for its replicas.
