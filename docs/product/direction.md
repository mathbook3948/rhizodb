# Product direction

Defines what RhizoDB is.

## Background

dotori needs to store people, places, tasks, and their relationships for years and find them fast. No graph database is both light enough to embed and stable, so RhizoDB is built for it as a separate project.

## Decisions

- The crate is `rhizodb`, developed and released from its own repository.
- It is a general-purpose graph database with no knowledge of dotori or memory. dotori uses it as a dependency.
- It is embedded and runs inside the host program with no separate server.
- It is a schemaless property graph.
- The query language follows GQL.
- Vector and keyword indexes live inside the graph and are updated in the same transaction as the data they index.

## Open

None
