# Carry operation context by construction

Status: accepted

wikidesk uses `tracing` spans at operation boundaries so operational logs inherit the same context across async and semi-parallel paths. A research task, remote sync run, merge integration, or long-running background operation should create one boundary span with the stable identifiers for that operation, then instrument spawned futures with that span.

Required boundary fields:

- research task: `wiki`, `task_id`, compact question title
- remote sync run: `wiki`, `remote`, `run_id`, trigger reason
- merge/publish integration: `wiki`, `repo`, `workspace`, and either `task_id` or `run_id`

Implementation rules:

- Put start/end/failure logs in the public operation wrapper, not scattered through low-level helpers.
- Use `FutureExt::instrument` or equivalent when spawning work so child logs keep the parent span.
- Keep raw helpers private when practical; expose the logged entrypoint instead.
- Add small context structs only when a helper needs the same fields in several places. Do not add a logging framework.
- New cross-cutting concerns should follow the same boundary-wrapper pattern before adding ad hoc calls inside branches.
