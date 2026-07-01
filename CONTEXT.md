# wikidesk context

## Domain glossary

- **wiki** — the `wiki/` content root inside a wiki repo that agents read directly.
- **wiki repo** — the operator-owned repository directory named `wiki-{wiki name}` that contains LLM-wiki files such as `index.md`, `log.md`, and the `wiki/` content root.
- **client mirror** — the consumer-side local copy or read-only mount of a wiki content root, named `wiki-{wiki name}`; it does not include the full wiki repo.
- **agent setup prompt** — generated instructions that tell a coding agent how to configure a consumer repository for wikidesk.
- **wiki instance** — one independently configured wiki repo served by wikidesk, with its own queue, runner, prompt, and client-facing instructions.
- **wiki description** — operator-authored text explaining what a wiki instance covers; it is shown to clients and setup prompts so agents know when to consult or research that wiki.
- **wiki name** — the stable identifier for a wiki instance; it names the HTTP base path, source wiki repo, and local client mirror directory.
- **base path** — the URL prefix that selects a wiki instance, such as `/rlhf` for `/rlhf/mcp` and `/rlhf/api/sync`.
- **research** — a question submitted by an agent for investigation against the wiki.
- **research prompt template** — the operator-owned template used to turn a research question into instructions for the agent runner; it is not wiki content.
- **research task** — the queued lifecycle of a research question, from submission through running, completion, failure, and expiry.
- **agent runner** — the mechanism that invokes an external research agent and extracts its answer.
- **sync** — the server-to-client process that makes a local wiki mirror the server wiki.
- **wikilink** — a `[[Page]]` or `[[Page|display]]` reference in a research answer that is resolved to a wiki file path.
- **configuration** — the validated startup inputs for the server: wiki instances, bind address, runner choices, agent commands, timeouts, and exposed instructions.
