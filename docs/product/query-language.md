# Query language

Decides which query syntax RhizoDB accepts and in what order features arrive.

## Background

GQL is the reference language, but the only public conformance suite is the openCypher TCK, and its setup queries use Cypher syntax. The accepted syntax decides how much of that suite can run unchanged.

## Decisions

- Where GQL and Cypher differ, the GQL form is canonical.
- Cypher's `CREATE` is accepted as an extension with the meaning of GQL `INSERT`, so TCK setup queries run unchanged.
- Keywords are case-insensitive, and identifiers are case-sensitive.
- The first features are `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `ORDER BY`, `SKIP` or `OFFSET`, `LIMIT`, `INSERT`, and the core expressions over the stored value types.
- Further features are ordered by how many TCK scenarios they unlock.

## Open

None
