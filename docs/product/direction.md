# Product direction

Defines what RhizoDB is.

## Background

Programs need to keep entities and their relationships in-process for years and find them fast.
No graph database is both light enough to embed and stable, so RhizoDB is built as a project of its own to fill that gap.

## Decisions

- The crate is `rhizodb`, developed and released from its own repository.
- It is a general-purpose graph database with no knowledge of the applications that use it as a dependency.
- It is embedded and runs inside the host program with no separate server.
- It is a schemaless property graph.
- The query language follows GQL.
- Vector and keyword indexes live inside the graph and are updated in the same transaction as the data they index.

## Open

None
