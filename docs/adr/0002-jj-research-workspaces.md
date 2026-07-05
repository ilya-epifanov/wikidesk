# Use jj workspaces for isolated research updates

Status: accepted

wikidesk can optionally protect wiki research with a fixed `jj` workflow. In this mode, only the `main` bookmark is publishable to clients. Research agents never edit the published workspace directly; they run in wikidesk-owned temporary `jj` workspaces and their changes are merged into `main` before a task is marked done.

## Why jj, not Git

Use either no VCS workflow or `jj`; do not add a parallel Git implementation in this feature. Git worktrees can isolate working trees, but Git requires named topic branches and conflict resolution leaves a repository in an in-progress merge state. `jj` workspaces provide multiple working copies for one repo, anonymous commits are normal, and conflicts are first-class commit states that can be resolved later or in another workspace. See the official `jj` docs on working copies/workspaces, first-class conflicts, and Git compatibility caveats:

- <https://jj-vcs.github.io/jj/latest/working-copy/>
- <https://jj-vcs.github.io/jj/latest/conflicts/>
- <https://jj-vcs.github.io/jj/latest/git-compatibility/>

## Configuration

```toml
[[wikis]]
name = "rlhf"
vcs_workflow = "none" # default; or "jj"
```

`jj` mode fails fast at startup if:

- `wiki_repo` is not a `jj` workspace/repo
- the `main` bookmark is missing or conflicted
- the published workspace is dirty

`main` is fixed, not configurable.

## Published workspace invariant

The configured `wiki_repo` remains the published workspace. `/api/sync` reads only `wiki_repo/wiki`.

In `jj` mode, wikidesk keeps this workspace as a clean empty working-copy commit on top of `main`:

```text
@ has no diff
parent(@) == main
clients sync wiki_repo/wiki from @
```

If the published workspace is clean but stale, wikidesk updates it to `main`. If it is dirty, wikidesk fails closed and tells the operator to commit, move, or discard the local edits. wikidesk never auto-commits user edits from the published workspace.

## Workspace ownership

wikidesk-created temporary workspaces use only wikidesk-owned names and paths:

```text
workspace names: wikidesk-research-<task_id>, wikidesk-merge-<task_id>, wikidesk-remote-sync-<run_id>
paths:           <parent-of-wiki_repo>/.wikidesk-<wiki_repo-name>-workspaces/<kind>-<id>
```

Startup cleanup may forget/delete only those prefixed workspaces and paths. User workspaces are never touched. Keep these directories outside `wiki_repo` so the published workspace does not become dirty just because temporary workspaces exist.

## Iteration 1 flow: one request at a time

Keep one active research request per wiki for the first implementation. This proves the lifecycle before introducing merge concurrency.

1. Check the published workspace invariant.
2. Create a temporary research workspace from `main`.
3. Run the configured research agent in that workspace using the normal research prompt.
4. Snapshot and detect changes with `jj util snapshot` and `jj diff --summary -r @`.
5. If no repo changes exist, return the agent answer and delete/forget the temp workspace.
6. If changes exist, describe the research commit:

   ```text
   wikidesk research: <compacted first line of question>

   Task: <task_id>

   <full question>
   ```

7. Merge into `main`:
   - if `main` is still the research commit parent, move `main` to the research commit
   - otherwise create a merge commit with parents `main` and the research commit
8. If the merge has conflicts, run the same configured agent command with a merge-resolution prompt.
9. Fail the task unless conflicts are fully resolved.
10. Move `main` to the successful merge/research commit.
11. Update the published workspace to a clean empty working-copy commit on `main`.
12. Rewrite wikilinks against the final published workspace and mark the task `Done`.
13. Forget/delete the temp workspace.

A task is `Done` only after `main` is updated and the published workspace is clean. If merge/resolution fails, the task is `Failed` and `main` remains at the last good revision.

## Merge commits and resolver notes

When `main` moved while research ran, the merge commit records the integration point:

```text
wikidesk merge: <task_id>
```

If a conflict resolver ran and produced notes, append them:

```text
wikidesk merge: <task_id>

Resolution:
<resolver notes>
```

The merge commit may contain real content changes: conflict resolutions or adaptations needed to make both parents coherent. The requester still receives the original research answer; resolver notes stay in history.

## Iteration 2 flow: parallel research, serialized merges

After iteration 1, allow multiple research workspaces per wiki. Research may run concurrently. Only automatic integration into `main` is serialized by a per-wiki merge queue.

Queue policy: merge completed research in finish order, not submit order. A slow older request must not block a newer request whose research already finished.

A request returns as soon as its own research has merged into `main` and the published workspace is clean.

## Sync semantics

Clients can sync only from the published `main` state. During research or conflict resolution, clients keep seeing the last clean `main`; they never see temp workspace files, unresolved conflict markers, or partially merged state.

## Failure cases

`jj` workflow fails the task or startup cleanly when:

- the research or resolver agent exits nonzero or times out
- conflicts remain after resolver execution
- the published workspace is dirty
- `main` is missing or conflicted
- a required `jj` command fails

## Implementation notes

Shell out to the `jj` CLI. Do not add a Rust `jj` library dependency. The CLI is sufficient for workspace creation, snapshotting, diff detection, describing commits, creating merge commits, moving bookmarks, and cleanup. Use machine-stable command output where available and keep wrappers small.
