# Dumps

Decides the logical dump format that `Database::export` writes and `Database::import` reads, and how its versions stay compatible.

## Background

A backup file only opens in a build that reads its storage format, and a host that keeps data for years also needs a copy that outlives formats and that other tools can read. A dump is that copy: text that names every node, relationship, value, and index definition, independent of how the storage lays them out.

## Decisions

- A dump is UTF-8 text with one JSON object per line, each line ended by a line feed.
- The first line is a header with `"type":"header"`, `"format":"rhizodb-dump"`, the format `version`, and the `next_node_id` and `next_relationship_id` of the exported database.
- Index lines follow the header in name order, then node lines in ascending ID order, then relationship lines in ascending ID order.
- The last line is `{"type":"end","nodes":<count>,"relationships":<count>}`, so a dump cut anywhere fails to import instead of importing in part unnoticed.
- An index line holds the `name`, the `index` kind `keyword`, `vector`, or `property`, exactly one of `label` and `relationship_type`, and the kind's fields: `properties` in name order, or `property`, plus `dimension` and `metric` for a vector index.
- A vector metric is written `cosine`, `euclidean`, or `dot_product`.
- A node line holds its `id`, its `labels` in name order, and its `properties` as an object with keys in name order.
- A relationship line holds its `id`, `relationship_type`, `start` and `end` node IDs, and `properties`.
- A dump keeps node and relationship IDs, and an import gives them back, so a host's references by ID survive a dump and an import.
- Token, label set, and index IDs are internal and stay out of a dump.
- A boolean, a string, and a list are their JSON forms.
- An integer is a JSON number without a fraction or exponent.
- A finite float is the shortest JSON number that reads back as the same bits, always written with a fraction or an exponent, so `1.0` stays a float and `-0.0` keeps its sign.
- A NaN or an infinity is `{"$float":"<16 lowercase hex digits of its bits>"}`, so a NaN keeps its payload.
- A byte string is `{"$bytes":"<lowercase hex>"}`.
- A date is `{"$date":"YYYY-MM-DD"}`, with a year of at least four digits and a `-` before a negative one.
- A local time is `{"$localtime":"HH:MM:SS.nnnnnnnnn"}`, always with nine fraction digits.
- A time is `{"$time":...}`, its local time followed by its offset as `+HH:MM:SS` or `-HH:MM:SS`.
- A local datetime is `{"$localdatetime":...}`, its date and local time joined by `T`.
- A datetime is `{"$datetime":...}`, its local date and time, its offset, and `[zone]` for a datetime in a named zone.
- An import takes the instant of a datetime from its local time and offset, and the zone name from the brackets, so a later change of the zone's rules keeps the instant.
- A duration is `{"$duration":{"months":..,"days":..,"seconds":..,"nanoseconds":..}}`, with the parts as stored and nanoseconds below one second.
- Every JSON object in a value position is one of these tagged values, since a property never holds a map.
- Export writes one canonical form, so exporting an imported dump gives its bytes back.
- Import reads only that grammar, refuses an unknown or repeated field, and accepts a carriage return before each line feed and a missing final line feed.
- An import goes only into a database that holds no index and has never held a node or relationship, so every dumped ID is still free.
- An import creates the indexes first and fills them as elements arrive.
- An import commits a batch of lines at a time, so its memory is bounded by the batch and the longest line.
- A host can cap the bytes of one dump line, and a longer line fails as an invalid dump before it is read whole.
- No line cap is set by default, so every export imports.
- Write transactions on other threads wait until an import ends, so no other write lands between its batches.
- A failed import leaves its committed batches behind, so a host imports into a new file and deletes it on failure.
- Export reads one snapshot and takes no write lock.
- A malformed or cut dump fails as an invalid dump, a newer version as an unsupported dump version, a used database as not empty, and a failing reader or writer as an I/O error.
- The version is a positive integer that counts up from one whenever the format changes.
- A build reads every dump version up to its own and refuses a newer one.
- A new version only adds, such as a value tag or a line type, so every older dump keeps its meaning.
- A build writes only its own version.

## Open

None
