# Security policy

RhizoDB runs inside the program that embeds it and reads queries and database files that the program hands it. A bug in RhizoDB can crash that program, hang it, or lose data it committed, so please report one privately.

## Supported versions

Below 1.0, security fixes go to the `main` branch and into the next release, and only the latest release is supported.

## Report a vulnerability

Do not open a public issue for a vulnerability.

Report it through [GitHub private vulnerability reporting](https://github.com/mathbook3948/rhizodb/security/advisories/new). Include:

- the version or commit you tested,
- the operating system,
- the query, the database file, or the code that shows the problem, and
- what an attacker can do with it.

The maintainer replies in the advisory, and you can follow the fix there.

## In scope

- A query that makes RhizoDB panic, hang, or use memory without bound.
- A database file that makes RhizoDB panic, hang, or read outside the file when it is opened.
- Committed data that is lost or changed after a crash or a power loss.
- A damaged database file that reads back as wrong data instead of failing as corrupt data.
- A query that reads or changes data outside what it names, such as a parameter that escapes into the query text.

## Out of scope

- Attacks that need control of the host program or its process.
- What a host-registered procedure does when a query calls it.
