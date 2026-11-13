# Recover the project's launcher

## Completed implementation

`src/servers.rs` recovers an observed npm, pnpm or Yarn launcher instead of saving
only its internal server command. The motivating case was design-kit: the old
Vite command stopped serving its page after the page moved into `preview`, while
`pnpm dev` from the project root still selected the correct server.

## Recovery rules

- Preserve the existing direct-command, Python-wrapper and listening-ancestor
  fallback, including the distinction between Tina and its Astro endpoint.
- Inspect at most eight parents. Reject missing or exited processes, reused
  parent IDs detected through creation times, and cycles.
- Extend the fallback through `cmd.exe` wrappers with optional `/d` and `/s`,
  then `/c` and a nonempty command. Stop at other shells or unknown processes.
- Recognize `node.exe` runners by their first script argument, with these suffixes:

| Runner | Script path suffix |
| --- | --- |
| npm | `node_modules\npm\bin\npm-cli.js` |
| pnpm | `node_modules\pnpm\bin\pnpm.cjs` or `node_modules\pnpm\bin\pnpm.mjs` |
| Yarn | `node_modules\yarn\bin\yarn.js` or `node_modules\@yarnpkg\cli-dist\bin\yarn.js` |

Accept a runner only when its working folder equals or contains the selected
launch folder, comparing complete Windows path components without case.
Preserve the executable, arguments and folder together. Recognition normalizes
case and slash direction without rewriting the saved invocation or requiring a
particular script name. Retain the best accepted launcher at every boundary.

The recovered folder supplies the project name and existing server matching.
Saved defaults keep their commands; file format, controls and dependencies stay
unchanged. Shell history and original shell variables are not reconstructed.
`design/spec.md` records the behavior.

## Verification and remaining limits

Regression tests cover supported runners, quoted arguments, nested scripts,
folder and port changes, rejected commands, unrelated folders, reused process
IDs, missing metadata, cycles, depth limits, direct Angular commands, Python
wrappers and distinct nested endpoints. Existing default-preservation tests pass.
Independent read-only subagent reviews found no correctness issues or useful
production-code cuts.

A read-only check of the saved design-kit entry found the root `pnpm dev`
launcher already recorded. No saved-entry correction was needed or performed
in this review. Automated tests, Clippy and the release build pass.

The release app runs on the taskbar. Native start/open/stop interactions and
Portal.Frontend's live behavior were not re-tested during this review; automated
coverage does not replace those checks.

For future changes, keep shared-file edits sequential and use independent
subagents for review. Preserve existing commands and test recovery through its
production path rather than adding special cases for individual projects.
