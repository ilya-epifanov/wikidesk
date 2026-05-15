# wikidesk context

## Domain glossary

- **wiki** — the local knowledge base directory (`wiki/`) that agents read directly.
- **research** — a question submitted by an agent for investigation against the wiki.
- **research task** — the queued lifecycle of a research question, from submission through running, completion, failure, and expiry.
- **agent runner** — the mechanism that invokes an external research agent and extracts its answer.
- **sync** — the server-to-client process that makes a local wiki mirror the server wiki.
- **wikilink** — a `[[Page]]` or `[[Page|display]]` reference in a research answer that is resolved to a wiki file path.
- **configuration** — the validated startup inputs for the server: wiki location, prompt template, runner choice, agent command, timeouts, and exposed instructions.
