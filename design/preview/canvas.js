(() => {
  // Names, branches and ports come from the snapshot.
  const designKit = {
    id: 'design-kit', name: 'design-kit', port: 8000, branch: 'main', default: true, status: 'running',
  };
  const history = [
    ['unlist-me', 5173, 'main'],
    ['Portal.Frontend', 4200, 'replace-syncfusion-toolbar'],
    ['preview', 8017, 'main'],
    ['Leon', 62783, ''],
    ['FA.Website', 9000, 'main'],
    ['design-kit-css-124c3aac', 18081, ''],
    ['unlist-cleanup-ui-nov23', 45872, ''],
    ['design-kit-cleanup-57e95eef', 49916, ''],
    ['preview', 8002, ''],
    ['preview', 8001, ''],
  ].map(([name, port, branch]) => ({
    id: name + '-' + port, name, port, branch, default: false, status: 'stopped',
  }));
  const [unlist, portal, preview] = history;
  const examples = {
    snapshot: [designKit, ...history],
    mixed: [
      designKit,
      { ...portal, default: true },
      { ...preview, branch: '', default: true },
      { ...unlist, status: 'running' }, history[4],
    ],
    single: [{ ...preview, branch: '', status: 'running' }],
    stopped: [{ ...designKit, status: 'stopped' }, { ...portal, default: true }],
    empty: [],
  };

  const desktop = document.querySelector('#desktop');
  const shelf = document.querySelector('#panel-shelf');
  const host = document.querySelector('#strip-host');
  const feedback = document.querySelector('#feedback');
  const example = document.querySelector('#example');
  const zoom = document.querySelector('#zoom');
  const viewButtons = [...document.querySelectorAll('[data-view]')];
  const starts = new Map();
  let view = 'servers';
  let projects = [];
  let selectedId;
  let strip;
  let contextMenu;
  let hoverTimer;
  let hoverOpen = false;
  let exited = false;

  function hint() {
    if (exited) return 'Portside has exited in this example. Reset the example to bring it back.';
    if (view === 'defaults') return 'Use the row controls to edit the example defaults.';
    if (view === 'strip') return 'Hover the strip to open the menu. Scroll to cycle through running servers.';
    if (!shelf.firstElementChild) return 'Portside has no server menu for this example.';
    return '';
  }

  function render() {
    const focused = document.activeElement;
    const rowId = focused?.closest('.ps-menu-row')?.dataset.id;
    const running = projects.filter(p => p.status === 'running').sort((a, b) => a.port - b.port);
    const selectedIndex = Math.max(0, running.findIndex(p => p.id === selectedId));
    selectedId = running[selectedIndex]?.id;
    const showPanel = !exited && (view !== 'strip' || hoverOpen);
    const panel = showPanel && window.PortsideMenu.createMenu({
      projects,
      mode: view === 'defaults' ? 'defaults' : 'servers',
      onAction: act,
    });
    shelf.replaceChildren(...(panel ? [panel] : []));
    if (rowId && panel) {
      const row = panel.querySelector(`[data-id="${CSS.escape(rowId)}"]`);
      const actions = Array.from(row?.querySelectorAll('button:enabled') ?? []);
      const next = actions.find(button => button.className === focused.className)
        ?? actions[0] ?? panel.querySelector('button:enabled');
      next?.focus({ preventScroll: true });
    }
    host.hidden = exited;
    strip.update({
      servers: running, selectedIndex, hasDefaults: projects.some(p => p.default), panelOpen: Boolean(panel),
    });
    for (const button of viewButtons) button.setAttribute('aria-pressed', String(button.dataset.view === view));
  }

  function dismissContext() {
    contextMenu?.remove();
    contextMenu = undefined;
  }

  function setView(next) {
    dismissContext();
    clearTimeout(hoverTimer);
    view = next;
    hoverOpen = false;
    render();
    feedback.textContent = hint();
  }

  function loadExample() {
    for (const timer of starts.values()) clearTimeout(timer);
    starts.clear();
    projects = examples[example.value].map(p => ({ ...p }));
    selectedId = projects.find(p => p.status === 'running')?.id;
    exited = false;
    setView(view);
  }

  function select(project) {
    selectedId = project.id;
    render();
  }

  function open(project) {
    select(project);
    feedback.textContent = 'This entry would open http://localhost:' + project.port + ' in your browser.';
  }

  function insertAfterDefaults(project) {
    projects = projects.filter(p => p !== project);
    const boundary = projects.filter(p => p.default).length;
    projects.splice(boundary, 0, project);
  }

  function act({ type, id, targetId }) {
    if (type === 'close') return setView('strip');
    const project = projects.find(p => p.id === id);
    if (!project) return;
    switch (type) {
      case 'open-editor':
        feedback.textContent = project.name + ' would open in VS Code.';
        return;
      case 'open':
        return open(project);
      case 'start':
        if (project.status !== 'stopped') return;
        project.status = 'starting';
        feedback.textContent = 'Starting ' + project.name + ' in the example.';
        starts.set(id, setTimeout(() => {
          starts.delete(id);
          project.status = 'running';
          open(project);
        }, 1600));
        break;
      case 'stop':
        project.status = 'stopped';
        if (!project.default) insertAfterDefaults(project);
        feedback.textContent = 'Stopped ' + project.name + ' in the example.';
        break;
      case 'add':
      case 'remove':
        project.default = type === 'add';
        insertAfterDefaults(project);
        break;
      case 'clear':
        projects = projects.filter(p => p !== project);
        break;
      case 'reorder': {
        const from = projects.indexOf(project);
        const to = projects.findIndex(p => p.id === targetId);
        if (to < 0 || from === to) return;
        projects.splice(from, 1);
        projects.splice(to, 0, project);
        break;
      }
    }
    render();
  }

  function onHover(inside) {
    clearTimeout(hoverTimer);
    if (view !== 'strip' || contextMenu || exited || inside === hoverOpen) return;
    hoverTimer = setTimeout(() => {
      hoverOpen = inside;
      render();
    }, inside ? 350 : 500);
  }

  function showContext(event) {
    event.preventDefault();
    if (exited) return;
    if (view !== 'defaults') setView('strip');
    dismissContext();
    contextMenu = window.PortsideMenu.createContextMenu({
      defaultsOpen: view === 'defaults',
      onAction({ type }) {
        dismissContext();
        if (type === 'defaults') return setView(view === 'defaults' ? 'strip' : 'defaults');
        if (type === 'exit') {
          exited = true;
          return setView('strip');
        }
        feedback.textContent = 'Move lets you reposition the strip on the Windows taskbar.';
      },
    });
    desktop.append(contextMenu);
    contextMenu.style.position = 'absolute';
    contextMenu.style.zIndex = '2';
    const bounds = desktop.getBoundingClientRect();
    const size = contextMenu.getBoundingClientRect();
    const scale = Number(zoom.value);
    const left = Math.min(event.clientX, bounds.right - size.width);
    const top = event.clientY + size.height <= bounds.bottom ? event.clientY : event.clientY - size.height;
    contextMenu.style.left = Math.max(0, (left - bounds.left) / scale) + 'px';
    contextMenu.style.top = Math.max(0, (top - bounds.top) / scale) + 'px';
    contextMenu.focus({ preventScroll: true });
  }

  function resize() {
    const scale = Number(zoom.value);
    desktop.style.zoom = scale;
    desktop.style.setProperty('--ps-menu-pixel', 1 / (devicePixelRatio * scale) + 'px');
    dismissContext();
  }

  async function init() {
    const initialExample = new URLSearchParams(location.search).get('example');
    if (Object.hasOwn(examples, initialExample)) example.value = initialExample;
    await Promise.allSettled([
      document.fonts.load('13px "Portside Atkinson"'),
      document.fonts.load('italic 13px "Portside Atkinson"'),
      document.fonts.load('12px "Portside Fluent"'),
    ]);
    strip = window.PortsideStrip.mountStrip(host, {
      onOpen: open, onStop: p => act({ type: 'stop', id: p.id }),
      onSelect: (_index, p) => select(p), onHover, onContextMenu: showContext,
    });
    for (const button of viewButtons) button.addEventListener('click', () => setView(button.dataset.view));
    example.addEventListener('change', loadExample);
    document.querySelector('#reset').addEventListener('click', loadExample);
    zoom.addEventListener('change', resize);
    window.addEventListener('resize', resize);
    shelf.addEventListener('contextmenu', showContext);
    shelf.addEventListener('pointerenter', () => clearTimeout(hoverTimer));
    shelf.addEventListener('pointerleave', () => onHover(false));
    document.addEventListener('pointerdown', event => {
      if (contextMenu?.contains(event.target)) return;
      dismissContext();
      if (view !== 'defaults' || shelf.contains(event.target) || event.target.closest('.controls, .heading')) return;
      if (event.button === 2 && host.contains(event.target)) return;
      setView('strip');
    });
    document.addEventListener('keydown', event => {
      if (event.key !== 'Escape') return;
      if (contextMenu) dismissContext();
      else setView('strip');
    });
    resize();
    loadExample();
  }
  init();
})();
