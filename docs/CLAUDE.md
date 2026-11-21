# Doc rules

- Paths are `docs/<domain>/<detailed-title>.md`. File and folder names use only lowercase English letters and hyphens.
- Every doc follows the template below.
- Docs record decisions only. Background is 1 to 3 sentences, and each line under Decisions holds one decision.
- Leave out measurements, version numbers, the research process, and anything already in code or config. Put the reasoning behind a decision in the commit message.
- Delete a doc once it no longer holds. Past content stays in git history.
- After adding or deleting a doc, or changing a title or summary, run `mise run docs` to regenerate the list in `docs/README.md`.

## Template

```markdown
# <Title>

<One sentence on what this doc decides>

## Background

<Why it is needed, in 1 to 3 sentences>

## Decisions

<What was decided. One per line>

## Open

<What is not decided yet. "None" if nothing>
```
