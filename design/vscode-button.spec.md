# VS Code button

- Place an icon-only button at the right end of each server-menu row, directly after the port.
- Center the 12 × 12 DIP ribbon in a full-height button with 8 DIPs on each side. [Exact canvas SVG](preview/index.html#vscode-button) · [Microsoft icon pack, `vscode-alt.svg`](https://code.visualstudio.com/assets/branding/visual-studio-code-icons.zip)
- On hover or keyboard focus, brighten the icon from `TEXT_DIM` to `TEXT`. Give the button its own background of 10% white over the row, with a 3-DIP corner radius.
- Inset the button background by 3 DIPs horizontally and 2 DIPs vertically. A press uses 12% black over the row. Keyboard focus outlines the button with a 1-DIP line.
- Left-click opens the Git worktree containing the running server’s launch folder or the stopped default’s saved folder. If Git cannot find a root, open that folder. The ribbon stays available while a default starts.
- If the folder is unreadable or VS Code is unavailable, keep the ribbon dim and disabled. A launch failure is silent and leaves the selected server alone.
- Keep the button’s clicks separate from the row’s. Only the row body opens the server URL, starts a stopped server, or stops a running server on middle-click.
- Open Portside from Windows Start to focus the first available action in an eligible server menu. Tab and Shift+Tab move between each row body and its enabled editor button. Enter or Space activates the focused action. Escape or switching apps closes the menu. Name each editor button `Open <project> in VS Code`.
