# Sync jj-backed wiki repos to Git remotes asynchronously

Status: accepted

For jj-backed wiki instances, remote sync is a background durability process rather than part of research completion. A research task is Done once the local published wiki repo is clean; a per-wiki remote sync loop wakes on completed research and on a fixed interval to fetch `main` from the configured Git remote, merge upstream divergence with the existing resolver agent when needed, and push local `main` back. Remote sync attempts for a wiki never overlap, and transient `jj git` failures retry for a bounded window; clients continue syncing from the local published wiki repo while remote sync is pending or failed, so GitHub availability does not block research answers.
