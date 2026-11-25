(() => {
  function mountStrip(host, options = {}) {
    const strip = document.createElement("div");
    strip.className = "portside-strip";
    strip.innerHTML = `
      <button type="button" class="portside-strip-label">
        <span class="portside-strip-globe" aria-hidden="true">&#xe774;</span>
        <span class="portside-strip-count"><span></span></span>
        <span class="portside-strip-server">
          <span class="portside-strip-name"></span><span class="portside-strip-port"></span>
        </span>
      </button>`;
    host.append(strip);

    const label = strip.firstElementChild;
    const count = label.querySelector(".portside-strip-count");
    const serverLabel = label.querySelector(".portside-strip-server");
    const [name, port] = serverLabel.children;
    const state = { servers: [], selectedIndex: 0, ...options };
    let inside = false;
    let pressedButton = null;
    let wheelRemainder = 0;

    function paintHighlight() {
      let appearance = "rest";
      if (!label.disabled) {
        if (pressedButton !== null || state.pressed) appearance = "pressed";
        else if (inside || state.hovered || state.panelOpen) appearance = "hovered";
      }
      label.dataset.state = appearance;
    }

    function update(patch = {}) {
      Object.assign(state, patch);
      const total = state.servers.length;
      state.selectedIndex = Math.max(0, Math.min(Math.trunc(state.selectedIndex) || 0, total - 1));
      const server = state.servers[state.selectedIndex];
      strip.toggleAttribute("data-empty", !server);
      label.disabled = !server && !state.hasDefaults;
      count.hidden = total < 2;
      count.dataset.capacity = `${total}/${total}`;
      count.firstElementChild.textContent = `${state.selectedIndex + 1}/${total}`;
      serverLabel.hidden = !server;
      name.textContent = server?.name ?? "";
      port.textContent = server ? `:${server.port}` : "";
      label.setAttribute("aria-label", server ? `Open ${server.name} on port ${server.port}`
        : state.hasDefaults ? "Saved projects" : "No running servers");
      paintHighlight();
    }

    label.addEventListener("pointerenter", event => {
      inside = true;
      paintHighlight();
      state.onHover?.(true, event);
    });
    label.addEventListener("pointerleave", event => {
      inside = false;
      pressedButton = null;
      paintHighlight();
      state.onHover?.(false, event);
    });

    label.addEventListener("pointerdown", event => {
      if (event.button !== 0 && event.button !== 1) return;
      if (event.button === 1) event.preventDefault();
      pressedButton = event.button;
      paintHighlight();
    });
    label.addEventListener("pointerup", event => {
      if (event.button !== pressedButton) return;
      pressedButton = null;
      paintHighlight();
      const server = state.servers[state.selectedIndex];
      if (!server) return;
      if (event.button === 0) state.onOpen?.(server, state.selectedIndex);
      else state.onStop?.(server, state.selectedIndex);
    });
    label.addEventListener("pointercancel", () => {
      pressedButton = null;
      paintHighlight();
    });
    label.addEventListener("auxclick", event => {
      if (event.button === 1) event.preventDefault();
    });
    label.addEventListener("click", event => {
      // Keyboard and assistive technology activate a button without pointer events.
      const server = state.servers[state.selectedIndex];
      if (event.detail === 0 && server) state.onOpen?.(server, state.selectedIndex);
    });

    strip.addEventListener("wheel", event => {
      if (event.ctrlKey || !event.deltaY) return;
      event.preventDefault();
      const total = state.servers.length;
      if (total < 2) return;
      // Windows Chromium reports wheel notches separately from the configured scroll distance.
      const notch = 120 / window.devicePixelRatio;
      const notches = Math.round(-event.wheelDeltaY / notch);
      const isDiscrete = notches && Math.abs(event.wheelDeltaY + notches * notch) <= 1;
      const unitsPerStep = event.deltaMode === 1 ? 3 : event.deltaMode === 2 ? 1 : 100;
      const delta = event.deltaMode === 0 && isDiscrete ? notches : event.deltaY / unitsPerStep;
      if (Math.sign(delta) !== Math.sign(wheelRemainder)) wheelRemainder = 0;
      wheelRemainder += delta;
      const steps = Math.trunc(wheelRemainder);
      if (!steps) return;
      wheelRemainder -= steps;
      const index = ((state.selectedIndex + steps) % total + total) % total;
      update({ selectedIndex: index });
      state.onSelect?.(index, state.servers[index]);
    }, { passive: false });
    strip.addEventListener("pointerleave", () => { wheelRemainder = 0; });
    strip.addEventListener("contextmenu", event => {
      event.preventDefault();
      state.onContextMenu?.(event);
    });

    update();
    return { update, label, destroy: () => strip.remove() };
  }

  window.PortsideStrip = { mountStrip };
})();
