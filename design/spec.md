# Portside design

A strip in the Windows taskbar, right of agent-usage-overlay, showing the local dev servers you have running and starting the projects you saved. Every size and color lives in [`src/theme.rs`](../src/theme.rs); this page says what each state looks like and how it behaves.

## What counts as a server

A TCP port listening on localhost, or on every address (`0.0.0.0`), whose process is a dev runtime (node, bun, deno, python and similar). Each server shows its project folder name (the process's working directory) and its port, for example `portside :3000`. A server started elevated hides its folder, so it shows the runtime's name. Servers are ordered by port.

## Terms

- **Running:** a server the scanner sees now.
- **Default:** a project the user added, in an order the user sets. It may be running or stopped.
- **Past run:** a project Portside saw running earlier that is stopped now and is not a default.
- **Strip's server:** the running server the strip currently shows.

A project is one working directory. `%APPDATA%\Portside\projects.tsv` stores the defaults, the running projects and the latest 10 past runs, and it stays after uninstall.

## The strip

The strip is the label: one hover rect holding, left to right, the globe, count, name and port. With one or more servers the strip keeps the width of the two-or-more state, so it never shifts along the taskbar. With none it shrinks to the globe and the padding around it, so it covers no more of the taskbar than it shows. The strip's left edge stays put across these widths.

```
(globe) 2/3  portside :3000
```

The count, name and port are all `STRIP_SIZE`, agent-usage-overlay's text size, so they share one baseline.

- **Globe:** the Segoe Fluent Icons Globe glyph (`GLOBE`) at `ICON_SIZE` in `TEXT`, centered vertically. `GLOBE_TO_TEXT` separates it from the count, or from the name when there is no count.
- **Count:** the position and total among running servers (`2/3`) in `TEXT_DIM` with tabular digits, right-aligned in the width of the total written twice (`12/12`) so cycling never moves the name. `COUNT_TO_NAME` separates it from the name.
- **Name and port:** the name is `TEXT`, the port `TEXT_DIM`. `NAME_SLOT_W` is reserved for them, so cycling never moves anything. A long name is cut with "…"; the port never is.

## Interaction

- **Hover the label:** a rounded `HOVER_ALPHA` background that hugs the globe, count, name and port. The empty space to its right does nothing. With no servers running and at least one default, the globe alone is the label and takes this background.
- **Scroll anywhere on the strip:** scrolling down moves to the next running server, scrolling up to the previous. It wraps around.
- **Click the label:** open the server over HTTP in the default browser. An IPv4 loopback listener opens at its address; an IPv6 listener or a server listening on every address opens at `localhost`. If another server shares its port, an IPv6 or wildcard listener opens at its family's loopback address (`[::1]` or `127.0.0.1`) instead.
- **Middle-click the label:** stop the server it shows, as if you pressed Ctrl+C in its terminal, so every process in that terminal gets the Ctrl+C. A server without a terminal, or one still running two seconds later, is ended along with every process it started. Nothing shows while it stops; it leaves the strip within a second of exiting. If it can't be stopped (an elevated server), nothing happens.
- **Pressed:** the hover rect drops to `PRESSED_ALPHA` while the left or middle button is down.

## Server menu

After the pointer rests on the label for `SHOW_DELAY_MS`, the menu opens if it has a row besides the strip's server, or the strip's server has a branch to show. With one server running, no branch and no stopped defaults there is no menu, and an open menu closes if its rows drop to that. With no servers running and at least one default, hovering the globe opens it.

Its bottom edge touches the top of the label's hover rect, left edges aligned, so the pointer can move straight up into it. It fits its widest row, measured across the rows of this menu and the toggle view together, so switching views keeps one width. It is at least `MIN_W` wide and at least as wide as the label is with the widest server, so cycling never changes its width. Nothing limits it to the screen. The label keeps its hover background while the menu is open.

Above the line are running servers that aren't defaults, ordered by port. Below it are the defaults, in the user's order. The line is `LINE_H` of `MENU_BORDER`, with `LINE_MARGIN` above and below, inset by `ROW_PAD_X`, and it appears only when both groups have rows. The strip's server always has a row, in its normal place.

- **Row styles:** name left, port right, at `MENU_SIZE`. A running row's name and port are `TEXT`. A stopped row's are `STOPPED`, and it shows its last port. A row that isn't a default is italic.
- **Branch:** a running server's current Git branch follows the project name on the left, separated by ` · `, for example `Portal.Frontend · config-tab-icons`. The port stays right-aligned, with `NAME_TO_PORT_MIN` separating it from the combined label. The branch and separator use `TEXT` and the italic face at `MENU_SIZE`. Both menu views fit the full label; the taskbar strip stays name-and-port only. Stopped and starting projects show no branch or separator.
- **Branch refresh:** background Git lookups refresh the launch folders through the one-second poll. Switching branches updates the menu without restarting the server or writing project history. A branch names the current checkout, not necessarily the server's compiled output. Detached checkouts, non-Git or unreadable folders, missing Git and failed lookups keep the name and port without a branch. Each lookup times out after two seconds; branch updates wait until an active click or drag ends.
- **A default's server:** the server on its last port, or else one running its saved command on another port, which then becomes its last port. Any other server in its folder is a running row of its own.
- **Highlight:** while the pointer is on the strip, the strip's server's row has the `HOVER_ALPHA` background. While the pointer is on a row, only that row has it.
- **Click a running row:** opens that server and makes it the strip's server.
- **Click a stopped default:** it starts hidden, with no window. A spinner appears `ICON_TO_TEXT` left of the port, and the name and port stay dim and in place. A starting row ignores further clicks.
- **Spinner:** a `SPINNER_SWEEP` arc of radius `SPINNER_RADIUS` and stroke `SPINNER_STROKE` in `TEXT_DIM`, centered in `SPINNER` and turning once per `SPINNER_TURN_MS`.
- **A start finishes** when a server from its folder listens on the last port, or runs the same saved command on another port, since Vite moves ports. The row turns bright, the browser opens, and it becomes the strip's server.
- **A start times out** after 60 seconds with neither. The spinner stops, the row returns to dim, and the hidden processes are ended. Nothing else shows.
- **Start command:** the server's own executable and arguments, in its working directory, with every `node_modules\.bin` from that folder upward and the executable's folder first on `PATH`. A wrapper runtime, such as a venv's `python.exe`, is saved at the topmost parent with the same executable name and working directory. A server started by another listening server in its folder saves the outer server's command, so starting it starts both and waits for the inner server's port. Variables set in the original shell aren't saved.
- **Middle-click a running row:** stops that row's server, as on the label, without making it the strip's server. A default stays in its place, dim.
- Exit leaves a hidden server running, and the next scan finds it.
- The menu closes when the pointer leaves both the label and the menu, and on a right-click on either.
- Nothing else is in the menu: no title, hints or footer.

## Toggle view

The same panel in the same place as the menu, opened only by right-click > Defaults. It stays open when the pointer leaves. It closes by clicking outside, by right-click > Defaults, or by Close defaults. A left- or middle-click on the strip counts as outside and then acts as usual. A right-click on the strip keeps it open.

Top to bottom, with `ROW_GAP` between rows:

1. The latest 10 past runs, most recent first, then running servers that aren't defaults. Each is a `ROW_H` row with name, port and `ADD` at the right. A stopped past run has `CLEAR` at the left; a running or starting row leaves that space empty.
2. The defaults, including the strip's server when it is one. Each is a `ROW_H` row with the drag handle at the left, name, port and `REMOVE` at the right. Every project row reserves `ACTION_SIZE` at the left, with `ICON_TO_TEXT` before the name, so names align.
3. A `ROW_H` row with `Close defaults` centered at `MENU_SIZE`, in `TEXT_DIM`.

The line, as in the menu, separates these groups where both sides have rows. Names, branches and ports follow the menu's row styles.

- **Plus and minus:** `ADD` and `REMOVE` at `ACTION_SIZE`, inset by `ROW_PAD_X`, with `ICON_TO_TEXT` between the port and the icon. They apply immediately, and there is nothing to cancel.
- **Plus** moves the row to the end of the defaults. It takes that row's port and command, so each running server of a folder that isn't a default has its own plus.
- **Minus** moves a running row above the line, and a stopped row to the front of the past runs.
- **Clear:** the Segoe Fluent Icons Clear glyph (`CLEAR`), a plain × at `ACTION_SIZE`, centered in the left column. Clicking it immediately forgets that stopped past run and keeps the view open. The entry stays gone after a restart, but a future run in its folder records it again. Defaults and running or starting rows cannot be cleared.
- **Drag handle:** two drawn bars `HANDLE_W` wide and `LINE_H` high, centered at `HANDLE_BARS` in a `HANDLE_H` box, horizontally centered in the left column.
- **Dragging the handle** reorders defaults, and only within the defaults. Rows move as the pointer does. That order is the menu's order, and it survives a restart.
- Clicking the rest of a row does nothing, and a middle-click does nothing anywhere in the view.
- Hovering a row gives it `HOVER_ALPHA`, and its icons turn `TEXT`. Hovering Close defaults gives that row `HOVER_ALPHA`, and its text turns `TEXT`. At rest, every icon is `TEXT_DIM`.
- Other servers in a default's folder and elevated servers aren't listed here.

## States

| State                   | What shows                                                               |
| ----------------------- | ------------------------------------------------------------------------ |
| Two or more servers     | Globe, count, name and port                                              |
| One server              | Globe, name and port; no count, and a menu with a branch or stopped default |
| No servers, defaults    | Only the globe, in `GLOBE_EMPTY`; it hovers and opens the menu           |
| No servers, no defaults | Only the globe, in `GLOBE_EMPTY`; nothing hovers                         |

Right-click anywhere opens win-taskbar-host's menu: Portside's Defaults and Exit items, Windows' separator, then the host's Move. Defaults has a check mark while the toggle view is open, and clicking it opens or closes the view.

## App icon and name

The icon is the strip's globe (`GLOBE` in `TEXT`) with a thin `TASKBAR` outline and no tile, stored in `assets/portside.ico` at 16, 20, 24, 32, 40, 48, 64 and 256 px. The outline keeps the white globe visible on light backgrounds. At 24 px and below the outline can't fit between the globe's lines, so the globe's disk is filled `TASKBAR` instead. Windows shows it for the exe in File Explorer, the Start menu shortcut and Search, Task Manager and Settings > Apps > Startup.

Those places name the app Portside, published by Leon Björklund, instead of portside.exe. Both come from the exe's version info, whose version is `Cargo.toml`'s.

## Updates

An installed Portside checks GitHub for a newer stable release in the background and upgrades itself. Nothing shows while it does: no prompt, notification or setting. The strip leaves for a few seconds while Portside restarts, then returns in the same place. A copy that was not installed by the installer, such as a build from source, never updates.
