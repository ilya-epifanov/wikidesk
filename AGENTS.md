# Repository instructions

## Operational context

When adding or changing research tasks, merge/publish flows, remote sync, background workers, spawned futures, or other cross-cutting concerns:

1. Add or extend a logged operation boundary with a `tracing` span.
2. Include stable fields: `wiki`; `task_id` for research; `remote` and `run_id` for remote sync; `repo` and `workspace` for jj integration.
3. Instrument spawned futures so child logs inherit the operation span.
4. Keep low-level helpers private when practical; expose the logged entrypoint.

Follow `docs/adr/0004-operation-context-by-construction.md`.
