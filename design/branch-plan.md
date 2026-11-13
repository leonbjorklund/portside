# Git branches in Portside

## Completed

Implemented menu-only branch labels with a background reader and independent
subagent review. Leon approved the white italic branch beside the project name
with “Looks good!” after requesting “make the branch itallic”.

## Final scope

- Running rows in both menu views show `Project · branch` on the left and
  `:port` on the right. The separator and branch use the existing white text
  color and italic face. Project names and ports retain their existing styles.
- Menu sizing measures the full label and keeps the existing minimum gap before
  the port. Both views share one width and preserve action and spinner spacing.
- The single-server hover menu opens when that server has a branch to show.
- The taskbar strip retains its content and geometry. Stopped and starting
  projects show no branch or separator.
- Detached checkouts, non-Git or unreadable folders, missing Git and failed
  lookups retain the name and port without a branch.

## Implementation

`src/branches.rs` owns one background worker and an in-memory directory-to-branch
cache. The existing one-second UI poll requests at most one batch at a time for
unique launch folders. Hidden `git -C <directory> symbolic-ref --quiet HEAD`
lookups support nested folders and linked worktrees, preserve Unicode and
slashes, and ignore inherited Git environment overrides.

Each lookup has a two-second deadline and bounded pipe reads. Shutdown cancels
and reaps the child before joining the worker. Removed folders lose cached
metadata and stale results cannot restore them. Branch changes update menu
geometry after an active click or drag ends, without changing server identity,
saved commands or project history.

The branch describes the current checkout, not necessarily compiled output.
No new Rust dependency, storage format, setting or installer change is needed.
`design/spec.md` records the behavior; `src/theme.rs` owns visual values.

## Maintenance and verification

Keep code minimal and follow existing patterns. Use subagents for independent
areas and read-only review; coordinate shared-file edits through one owner.
Preserve unrelated launcher-recovery work in `src/servers.rs` and `PLAN.md`.

Behavior tests use temporary Git repositories for named and unborn branches,
nested folders, linked worktrees, Unicode, branch switches, detached checkouts,
failed lookups, cache pruning, output bounds, cancellation and timeout handling.
Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
`cargo build --release` after implementation changes.

Leon visually approved the running menu. Full native interaction coverage and
another Windows scaling level have not been independently verified.
