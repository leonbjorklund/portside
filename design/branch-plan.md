# Git branches in Portside

## Completed

Menu-only checkout labels use a background reader for running and saved folders.
Leon selected live labels in every state, plain names for unique detached
matches, and `detached` when no unique match exists in the implementation plan.

## Final scope

- Running, stopped and starting rows in both menu views show `Project · branch`
  on the left and `:port` on the right, including saved past runs. The separator
  and branch are italic and follow the row's existing bright or dim color.
- Each menu's width is its widest complete row plus side padding, without a
  minimum panel width or dependency on the strip. The gap before the port is
  44 screen pixels at 150% Windows scale, including the spinner slot. Sizing
  includes each view's controls and the text of `Close defaults`.
- The single-server hover menu opens when that server has a branch to show.
- The taskbar strip retains its content and geometry.
- A detached checkout uses the name of its sole exact local branch match,
  or `detached` for zero or multiple matches. Remote references and tags do
  not count; there is no fetching or historical inference.
- Missing, non-Git or unreadable folders, missing Git and failed lookups retain
  the name and port without a branch, clearing any stale label.

## Implementation

`src/branches.rs` owns one background worker and an in-memory directory-to-branch
cache. The existing one-second UI poll requests at most one batch at a time for
unique running and saved folders. Hidden `git -C <directory> symbolic-ref --quiet
HEAD` lookups support nested folders and linked worktrees, including unborn
branches. Exit code 1 triggers `for-each-ref --points-at=HEAD --count=2` over
`refs/heads/`; other failures produce no label. Both commands preserve Unicode
and slashes and ignore inherited Git environment overrides.

Each folder lookup shares one two-second deadline across its commands and uses
bounded pipe reads. Shutdown cancels
and reaps the child before joining the worker. Removed folders lose cached
metadata and stale results cannot restore them. Branch changes update menu
geometry after an active click or drag ends, without changing server identity,
saved commands or project history.

Labels follow the current checkout while running, starting or stopped, on the
next completed background refresh. Saving or starting never freezes them.
An inferred name can change when a matching branch moves. The branch describes
the current checkout, not necessarily compiled output.
No new Rust dependency, storage format, setting or installer change is needed.
`design/spec.md` records the behavior; `src/theme.rs` owns visual values.

## Maintenance and verification

Keep code minimal and follow existing patterns. Use subagents for independent
areas and read-only review; coordinate shared-file edits through one owner.
Preserve unrelated launcher-recovery behavior.

Behavior tests use temporary Git repositories for named and unborn branches,
nested folders, linked worktrees, Unicode, branch switches, unique and ambiguous
detached matches, ignored remote references and tags, moving branches, failed
lookups, cache pruning, output bounds, cancellation and timeout handling.
Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
`cargo build --release` after implementation changes.

Verify the menu on the real taskbar with stopped, starting and running fixtures,
both menu views, branch changes and a single server showing `detached`.
