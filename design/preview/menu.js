(() => {
  "use strict";

  const glyphs = { add: "\uECC8", remove: "\uECC9", clear: "\uE894" };

  function element(tag, className, text) {
    const node = document.createElement(tag);
    node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function button(className, label) {
    const node = element("button", className);
    node.type = "button";
    node.setAttribute("aria-label", label);
    return node;
  }

  function separator() {
    const line = element("div", "ps-menu-separator");
    line.setAttribute("role", "separator");
    return line;
  }

  // Move the visible rows during a drag, then commit their final order on release.
  function dragDefault(event, row, onAction) {
    if (event.button !== 0 || !event.isPrimary) return;
    event.preventDefault();
    const panel = row.parentElement;
    const rows = () => Array.from(panel.querySelectorAll('[data-default="true"]'));
    const original = rows();
    let index = original.indexOf(row);
    if (index < 0) return;

    panel.setPointerCapture(event.pointerId);
    row.classList.add("ps-menu-dragging");
    function move(pointer) {
      if (pointer.pointerId !== event.pointerId) return;
      const visible = rows();
      const lastAbove = visible.findLastIndex(item => item.getBoundingClientRect().top <= pointer.clientY);
      const next = Math.max(0, lastAbove);
      if (index === next) return;
      const neighbor = visible[next];
      panel.insertBefore(row, next > index ? neighbor.nextSibling : neighbor);
      index = next;
    }
    function finish(pointer) {
      if (pointer.pointerId !== event.pointerId) return;
      panel.removeEventListener("pointermove", move);
      panel.removeEventListener("pointerup", finish);
      panel.removeEventListener("lostpointercapture", finish);
      row.classList.remove("ps-menu-dragging");
      if (panel.hasPointerCapture(event.pointerId)) panel.releasePointerCapture(event.pointerId);
      if (original[index] !== row) {
        onAction({ type: "reorder", id: row.dataset.id, targetId: original[index].dataset.id });
      }
    }
    panel.addEventListener("pointermove", move);
    panel.addEventListener("pointerup", finish);
    panel.addEventListener("lostpointercapture", finish);
  }

  function control(project, action, onAction) {
    const labels = {
      add: `Add ${project.name} to defaults`,
      remove: `Remove ${project.name} from defaults`,
      clear: `Clear ${project.name} from past runs`,
      reorder: `Reorder ${project.name}`,
    };
    const leading = action === "clear" || action === "reorder";
    const node = button(`ps-menu-control ps-menu-${leading ? "leading" : "trailing"}`, labels[action]);
    node.dataset.action = action;
    const icon = element("span", action === "reorder" ? "ps-menu-handle" : "ps-menu-glyph", glyphs[action]);
    icon.setAttribute("aria-hidden", "true");
    node.append(icon);
    if (action === "reorder") {
      node.addEventListener("pointerdown", event => dragDefault(event, node.parentElement, onAction));
      node.addEventListener("keydown", event => {
        if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
        event.preventDefault();
        const row = node.parentElement;
        const neighbor = event.key === "ArrowUp" ? row.previousElementSibling : row.nextElementSibling;
        if (neighbor?.dataset.default === "true") {
          onAction({ type: action, id: project.id, targetId: neighbor.dataset.id });
        }
      });
    } else {
      node.addEventListener("click", () => onAction({ type: action, id: project.id }));
    }
    return node;
  }

  function projectRow(project, defaultsView, onAction) {
    const running = project.status === "running";
    const starting = project.status === "starting";
    const row = element("div", "ps-menu-row");
    row.setAttribute("role", "group");
    row.dataset.id = project.id;
    row.dataset.status = project.status;
    row.dataset.default = String(Boolean(project.default));
    const label = element("span", "ps-menu-label", project.name);
    if (project.branch) label.append(element("i", "ps-menu-branch", ` · ${project.branch}`));
    const port = element("span", "ps-menu-port", `:${project.port}`);
    if (starting) {
      const spinner = element("span", "ps-menu-spinner");
      spinner.setAttribute("aria-hidden", "true");
      port.prepend(spinner);
    }
    const description = [project.name, project.branch, `port ${project.port}`, project.status].filter(Boolean).join(", ");
    row.setAttribute("aria-label", description);
    if (defaultsView) {
      let leading = element("span", "ps-menu-leading");
      if (project.default) leading = control(project, "reorder", onAction);
      else if (!running && !starting) leading = control(project, "clear", onAction);
      row.append(leading, label, port, control(project, project.default ? "remove" : "add", onAction));
    } else {
      const serverAction = running ? "Open" : "Start";
      const body = button("ps-menu-body", `${serverAction} ${project.name} on port ${project.port}`);
      body.append(label, port);
      body.disabled = starting;
      body.addEventListener("click", () => onAction({ type: running ? "open" : "start", id: project.id }));
      body.addEventListener("auxclick", event => {
        if (event.button === 1 && running) onAction({ type: "stop", id: project.id });
      });
      const editor = document.querySelector("#vscode-button").content.firstElementChild.cloneNode(true);
      editor.title = `Open ${project.name} in VS Code`;
      editor.setAttribute("aria-label", editor.title);
      editor.addEventListener("click", () => onAction({ type: "open-editor", id: project.id }));
      row.append(body, editor);
    }
    return row;
  }

  function createMenu({ projects, mode = "servers", onAction = () => {} }) {
    const defaultsView = mode === "defaults";
    const defaults = projects.filter(item => item.default);
    const others = projects.filter(item => !item.default && (!defaultsView || item.editable !== false));
    const running = others.filter(item => item.status === "running").sort((a, b) => a.port - b.port);
    const past = defaultsView ? others.filter(item => item.status !== "running").slice(0, 10) : [];
    const groups = [[...past, ...running], defaults].filter(group => group.length);
    if (!defaultsView) {
      const visible = [...running, ...defaults];
      if (!visible.length || (visible.length === 1 && visible[0].status === "running" && !visible[0].branch)) return null;
    }
    const panel = element("div", "ps-menu");
    panel.setAttribute("role", "group");
    panel.setAttribute("aria-label", defaultsView ? "Defaults" : "Servers");
    panel.style.setProperty("--ps-menu-device-pixel", `${1 / window.devicePixelRatio}px`);
    for (const group of groups) {
      if (panel.children.length) panel.append(separator());
      for (const project of group) panel.append(projectRow(project, defaultsView, onAction));
    }
    if (defaultsView) {
      if (panel.children.length) panel.append(separator());
      const close = button("ps-menu-close", "Close defaults");
      close.textContent = "Close defaults";
      close.addEventListener("click", () => onAction({ type: "close" }));
      panel.append(close);
    }
    // Prevent the browser's middle-button autoscroll from taking over the strip.
    panel.addEventListener("mousedown", event => { if (event.button === 1) event.preventDefault(); });
    panel.addEventListener("auxclick", event => { if (event.button === 1) event.preventDefault(); });
    return panel;
  }

  function createContextMenu({ defaultsOpen = false, onAction = () => {} } = {}) {
    const menu = element("div", "ps-context-menu");
    menu.tabIndex = -1;
    menu.setAttribute("role", "menu");
    menu.setAttribute("aria-label", "Portside");
    for (const [action, title] of [["defaults", "Defaults"], ["exit", "Exit"], ["move", "Move"]]) {
      if (action === "move") menu.append(separator());
      const item = button("ps-context-item", title);
      item.setAttribute("role", action === "defaults" ? "menuitemcheckbox" : "menuitem");
      if (action === "defaults") item.setAttribute("aria-checked", String(defaultsOpen));
      item.textContent = title;
      item.addEventListener("click", () => onAction({ type: action }));
      item.addEventListener("contextmenu", event => {
        event.preventDefault();
        if (event.button === 2) onAction({ type: action });
      });
      menu.append(item);
    }
    menu.addEventListener("keydown", event => {
      if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
      event.preventDefault();
      const items = Array.from(menu.querySelectorAll("button"));
      const index = items.indexOf(document.activeElement);
      const step = event.key === "ArrowDown" ? 1 : -1;
      const next = index < 0 ? (step > 0 ? 0 : items.length - 1) : (index + step + items.length) % items.length;
      items[next].focus();
    });
    return menu;
  }

  window.PortsideMenu = { createMenu, createContextMenu };
})();
