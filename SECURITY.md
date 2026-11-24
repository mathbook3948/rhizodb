# Security

## Supported versions

RhizoDB is below 1.0. Fixes go into the next release, and only the latest release is supported.

## Reporting a problem

Report a vulnerability privately through GitHub's private vulnerability reporting: open the repository's Security tab and choose "Report a vulnerability". Do not open a public issue, pull request, or discussion for it.

Include what is affected, how to reproduce it, and what an attacker gains. A database file or a query that triggers the problem helps most.

## What counts

RhizoDB embeds in a host and trusts the host. Treat these as security problems:

- a query text, parameter, dump, or database file that makes RhizoDB panic out of its API, overflow the stack, hang, or exhaust memory past its documented limits;
- a damaged database file that reads back as wrong data instead of failing as corrupt data;
- a commit that is lost after `commit` returned, or a transaction that is seen in part.

Bugs that need the host to misuse the API on purpose, such as building query text from untrusted input instead of passing parameters, are ordinary bugs.
