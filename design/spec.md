# Portside design

A strip in the Windows taskbar, right of agent-usage-overlay, showing the local dev servers you have running. Every size and color lives in [`src/theme.rs`](../src/theme.rs); this page says what each state looks like and how it behaves.

## What counts as a server

A TCP port listening on localhost, or on every address (`0.0.0.0`), whose process is a dev runtime (node, bun, deno, python and similar). Each server shows its project folder name (the process's working directory) and its port, for example `portside :3000`. A server started elevated hides its folder, so it shows the runtime's name. Servers are ordered by port.

## The strip

The strip is the label: one hover rect holding, left to right, the globe, count, name and port. With one or more servers the strip keeps the width of the two-or-more state, so it never shifts along the taskbar. With none it shrinks to the globe and the padding around it, so it covers no more of the taskbar than it shows. The strip's left edge stays put across these widths.

```
(globe) 2/3  portside :3000
```

The count, name and port are all `STRIP_SIZE`, agent-usage-overlay's text size, so they share one baseline.

- **Globe:** the Segoe Fluent Icons Globe glyph (`GLOBE`) at `ICON_SIZE` in `TEXT`, centered vertically. `GLOBE_TO_TEXT` separates it from the count, or from the name when there is no count.
- **Count:** the position and total (`2/3`) in `TEXT_DIM` with tabular digits, right-aligned in the width of the total written twice (`12/12`) so cycling never moves the name. `COUNT_TO_NAME` separates it from the name.
- **Name and port:** the name is `TEXT`, the port `TEXT_DIM`. `NAME_SLOT_W` is reserved for them, so cycling never moves anything. A long name is cut with "…"; the port never is.

## Interaction

- **Hover the label:** a rounded `HOVER_ALPHA` background that hugs the globe, count, name and port. The empty space to its right does nothing.
- **Scroll anywhere on the strip:** scrolling down moves to the next server, scrolling up to the previous. It wraps around.
- **Click the label:** open `http://localhost:<port>` in the default browser. When two servers share a port, one on IPv4 and one on IPv6, `localhost` reaches only one of them, so each opens at its own address instead: `127.0.0.1` or `[::1]`.
- **Middle-click the label:** stop the server it shows, as if you pressed Ctrl+C in its terminal, so every process in that terminal gets the Ctrl+C. A server without a terminal, or one still running two seconds later, is ended along with every process it started. Nothing shows while it stops; it leaves the strip within a second of exiting. If it can't be stopped (an elevated server), nothing happens.
- **Pressed:** the hover rect drops to `PRESSED_ALPHA` while the left or middle button is down.

## Server menu

With two or more servers, after the pointer rests on the label for `SHOW_DELAY_MS`, a list of the servers the strip isn't showing opens. It closes if the servers drop to one. Its bottom edge touches the top of the label's hover rect, left edges aligned, and it is at least as wide as the label is with the widest server, so the pointer can move straight up into it and cycling never changes its width. The label keeps its hover background while the menu is open.

- One row per server except the current one: name left in `MENU_ROW`, port right in `TEXT_DIM`. Everything is Regular weight.
- Hovering a row gives it the same `HOVER_ALPHA` background.
- Clicking a row opens that server and makes it the current one in the strip; the previous current server takes its place in the menu.
- Middle-clicking a row stops that row's server, as on the label, without making it the current one.
- The menu closes when the pointer leaves both the label and the menu.
- Nothing else is in the menu: no title, hints or footer.

## States

| State               | What shows                                       |
| ------------------- | ------------------------------------------------ |
| Two or more servers | Globe, count, name and port                      |
| One server          | Globe, name and port; no count or menu           |
| No servers          | Only the globe, in `GLOBE_EMPTY`; nothing hovers |

Right-click anywhere opens win-taskbar-host's menu: Portside's Exit item, then the host's Move.

## App icon and name

The icon is the strip's globe (`GLOBE` in `TEXT`) with a thin `TASKBAR` outline and no tile, stored in `assets/portside.ico` at 16, 20, 24, 32, 40, 48, 64 and 256 px. The outline keeps the white globe visible on light backgrounds. At 24 px and below the outline can't fit between the globe's lines, so the globe's disk is filled `TASKBAR` instead. Windows shows it for the exe in File Explorer, the Start menu shortcut and Search, Task Manager and Settings > Apps > Startup.

Those places name the app Portside, published by Leon Björklund, instead of portside.exe. Both come from the exe's version info, whose version is `Cargo.toml`'s.
