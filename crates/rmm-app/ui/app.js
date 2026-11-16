'use strict';
// The window talks to rmm-core through one Tauri command, `call(name, args)` (src/main.rs).
const T = window.__TAURI__;
const call = (name, args = {}) => T.core.invoke('call', { name, args });
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const esc = v => String(v ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const basename = p => String(p).split(/[\\/]/).pop();
const plural = (n, word) => `${n.toLocaleString()} ${word}${n === 1 ? '' : 's'}`;
const GAMES = { 'skyrim-se': 'Skyrim Special Edition', fallout4: 'Fallout 4' };
const LAST = 'rmm.lastInstance';
const remember = path => { try { localStorage.setItem(LAST, path); } catch {} };
const recall = () => { try { return localStorage.getItem(LAST); } catch { return null; } };

let S = null;               // instance state from the core
let names = new Map();      // mod id -> name
let byId = new Map();       // mod id -> row
let idByName = new Map();   // mod name -> id (first of equal names), for the name pickers
let D = null;               // lookups worked out once per load (see derive)
let plan = null;            // deploy dry run, or { error }
let issues = null;          // health check findings
let issuesStale = true;
let details = null;         // mod_details of the selected mod
let stamp = '';
let selected = new Set();   // selected mod and separator ids
let anchor = null;          // where Shift-selection starts
let cur = null;             // the row the arrow keys move from
let collapsed = new Set();  // collapsed separators, kept per instance in localStorage
let collapsedRoot = null;
let filters = new Set();    // state filters that are on (keys of CRITERIA)
let tab = 'mods';
let selPlugin = null;       // selected plugin name
let lootRules = null;       // { rules, groups } once a plugin was selected; null = reload
let toolsData = null;       // { tools, presets }
let ignored = new Set();    // health findings ignored in this instance ("kind|subject")
let busy = false;
let healthTimer = 0, healthRun = 0, filterTimer = 0;
let historyData = null;     // history_details, loaded when the History tab shows
let side = 'mods';          // which list was clicked last, when both show
let selPluginMod = null;    // the mod providing the selected plugin
let dropAt = null;          // { id, priority } while a file is dragged over a mod row
let updates = new Map();    // mod id -> newer Nexus file, from the last check

// Window preferences: columns, their widths, the tab, the filter and the split view.
const PREFS = 'rmm.prefs';
const prefs = (() => { try { return JSON.parse(localStorage.getItem(PREFS)) ?? {}; } catch { return {}; } })();
const savePrefs = () => { try { localStorage.setItem(PREFS, JSON.stringify(prefs)); } catch {} };
prefs.cols ??= { conflicts: true, tags: false, notes: false, installed: false, files: true, priority: true };
prefs.widths ??= {};
const COLS = {
  conflicts: { title: 'Conflicts', cls: 'conf-col' },
  tags: { title: 'Tags', cls: 'tags-col' },
  notes: { title: 'Notes', cls: 'notes-col' },
  installed: { title: 'Installed', cls: 'date-col' },
  files: { title: 'Files', cls: 'num' },
  priority: { title: '#', cls: 'num', tip: 'Priority: higher wins file conflicts' },
};
const shownCols = () => Object.keys(COLS).filter(k => prefs.cols[k]);
const splitOn = () => prefs.split !== false && innerWidth >= 1500;
const pluginSide = () => tab === 'plugins' || (tab === 'mods' && splitOn() && side === 'plugins' && !!selPlugin);
const nameOf = id => names.get(id) ?? id;
const isSep = id => !!byId.get(id)?.separator;

// ---- Calls ----------------------------------------------------------------------------------

/** Run one change: progress bar, errors as toasts, reload afterwards. Resolves to the call's
 *  result (true for calls that return nothing), or undefined when it failed. */
async function act(label, fn, { reload = true } = {}) {
  if (busy) return;
  setBusy(true);
  try {
    const r = await fn();
    if (reload) await load();
    return r ?? true;
  } catch (e) {
    toast(`${label} failed.\n${e}`, 'error');
    if (reload) load().catch(() => {}); // undo what the window showed ahead of the call
  } finally {
    setBusy(false);
  }
}

function setBusy(on) {
  busy = on;
  $('#busy').hidden = !on;
  document.body.classList.toggle('busy', on);
  if (!on) {
    $('#busy div').classList.remove('det');
    $('#busy div').style.width = '';
    $('#progress').hidden = true;
  }
}

// Long jobs report their stage; the bar fills in when the count is known.
T.event.listen('progress', ({ payload: p }) => {
  if (!busy) return;
  const bar = $('#busy div'), known = p.total > 0;
  bar.classList.toggle('det', known);
  bar.style.width = known ? `${(100 * p.done / p.total).toFixed(1)}%` : '';
  const stage = p.stage[0].toUpperCase() + p.stage.slice(1);
  $('#progress').textContent = known ? `${stage}: ${p.done.toLocaleString()} of ${p.total.toLocaleString()}` : `${stage}…`;
  $('#progress').hidden = false;
});

async function load() {
  S = await call('state');
  stamp = await call('stamp');
  names = new Map(S.mods.map(m => [m.id, m.name]));
  byId = new Map(S.mods.map(m => [m.id, m]));
  idByName = new Map();
  for (const m of S.mods) if (!m.separator && !idByName.has(m.name)) idByName.set(m.name, m.id);
  $('#mod-names').innerHTML = [...idByName.keys()].map(n => `<option value="${esc(n)}">`).join('');
  derive();
  // AI access follows the window to another instance, and comes back on with it.
  if (prefs.mcp && mcpRoot !== S.info.root) setMcp(true);
  if (S.info.root !== collapsedRoot) { loadCollapsed(); loadIgnored(); }
  for (const id of selected) if (!byId.has(id)) selected.delete(id);
  if (selPlugin && !S.plugins.some(p => p.name === selPlugin)) selPlugin = null;
  lootRules = null;
  if (tab === 'plugins' && selPlugin) loadLootRules();
  if (tab === 'tools' || toolsData) loadTools();
  issuesStale = true;
  historyData = null;
  if (tab === 'history') loadHistory();
  render();
  refreshPlan();
  // The health check reads every plugin; re-run it once a burst of changes settles.
  clearTimeout(healthTimer);
  healthTimer = setTimeout(runHealth, tab === 'problems' ? 0 : 800);
  if (oneMod()) loadDetails();
}

/** Per-load lookups: conflicts by mod, broken rules, mods whose plugins miss masters, and the
 *  separator groups (a separator owns the mods after it, up to the next separator). */
function derive() {
  const ref = x => typeof x === 'number' ? S.mods[x].id : x; // conflicts name mods by priority
  for (const c of S.conflicts) {
    for (const k of ['overwrites', 'overwritten_by', 'archive_overwrites', 'archive_overwritten_by']) if (c[k]) c[k] = c[k].map(ref);
  }
  const C = Object.fromEntries(S.conflicts.map(c => [c.id, c]));
  const broken = new Map();
  for (const [id, after] of S.broken) broken.set(id, [...(broken.get(id) ?? []), after]);
  for (const u of S.unmet) {
    broken.set(u.id, broken.get(u.id) ?? []);
    broken.get(u.id).unmet = u;
  }
  const missing = new Set(S.plugins.filter(p => p.enabled && p.missing_masters.length && p.mod_id).map(p => p.mod_id));
  const group = new Map(), kids = new Map();
  let g = null;
  for (const m of S.mods) {
    if (m.separator) kids.set(g = m.id, []);
    else if (g) { group.set(m.id, g); kids.get(g).push(m); }
  }
  D = { C, broken, missing, group, kids, locks: new Set((S.locks ?? []).map(n => n.toLowerCase())) };
}

async function refreshPlan() {
  try { plan = await call('plan'); } catch (e) { plan = { error: String(e) }; }
  renderDeploy();
}

// Another process (the CLI, an AI over MCP) changed the instance: reload.
setInterval(async () => {
  if (busy || !S || document.hidden) return;
  try {
    if ((await call('stamp')) !== stamp) await load();
  } catch {}
}, 1500);

const collapsedKey = () => 'rmm.collapsed:' + S.info.root;
const ignoredKey = () => 'rmm.ignored:' + S.info.root;
function loadIgnored() { try { ignored = new Set(JSON.parse(localStorage.getItem(ignoredKey()) ?? '[]')); } catch { ignored = new Set(); } }
function saveIgnored() { try { localStorage.setItem(ignoredKey(), JSON.stringify([...ignored])); } catch {} }
function loadCollapsed() {
  collapsedRoot = S.info.root;
  try { collapsed = new Set(JSON.parse(localStorage.getItem(collapsedKey())) ?? []); } catch { collapsed = new Set(); }
}
function saveCollapsed() { try { localStorage.setItem(collapsedKey(), JSON.stringify([...collapsed])); } catch {} }

// ---- Start ----------------------------------------------------------------------------------

async function start() {
  try { await load(); return showApp(); } catch {}
  const last = recall();
  if (last) {
    try { await call('open', { path: last }); await load(); return showApp(); } catch {}
  }
  showWelcome();
}

function showApp() {
  $('#welcome').hidden = true;
  $('#app').hidden = false;
  remember(S.info.root);
  showTab(tab);
}

// The last tab, filter and state chips come back with the window.
tab = ['mods', 'plugins', 'problems', 'tools', 'history'].includes(prefs.tab) ? prefs.tab : 'mods';
$('#filter').value = prefs.filter ?? '';
filters = new Set((prefs.chips ?? []).filter(f => f in { enabled: 1, disabled: 1, conflicted: 1, overridden: 1, rule: 1, masters: 1, plugins: 1, recent: 1, problems: 1 }));

async function showWelcome() {
  $('#app').hidden = true;
  $('#welcome').hidden = false;
  const found = await call('detect').catch(() => []);
  $('#create-games').innerHTML = found.map(d =>
    `<button data-game="${esc(d.game)}">New instance for ${esc(GAMES[d.game] ?? d.game)}<br><span class="muted small">${esc(d.game_path)}</span></button>`).join('');
}

async function enter(label, fn) {
  if (await act(label, fn)) showApp();
}

$('#open-instance').onclick = async () => {
  const path = await T.dialog.open({ directory: true, title: 'Open an rmm instance folder' });
  if (path) enter('Opening the instance', () => call('open', { path }));
};

$('#create-games').onclick = async e => {
  const game = e.target.closest('[data-game]')?.dataset.game;
  if (!game) return;
  const path = await T.dialog.open({ directory: true, title: 'Choose a folder for the new instance' });
  if (path) enter('Creating the instance', () => call('create', { game, path }));
};

// ---- Virtual lists: only the rows on screen are in the DOM -------------------------------------

let rowH = 32; // measured from the first row painted
const lists = {
  mods: { table: $('#mods'), items: [], row: modRow, drag: null },
  plugins: { table: $('#plugins'), items: [], row: pluginRow, drag: null },
};
const scroller = L => L.table.closest('.scroll');

function paint(L) {
  const sc = scroller(L);
  if (!sc.offsetParent) return; // its tab is hidden; showTab paints it
  const head = L.table.tHead.offsetHeight;
  const n = L.items.length;
  const first = Math.max(0, Math.floor((sc.scrollTop - head) / rowH) - 8);
  const last = Math.min(n, first + Math.ceil(sc.clientHeight / rowH) + 16);
  const cols = L.table.tHead.rows[0].cells.length;
  const pad = h => h > 0 ? `<tr class="pad"><td colspan="${cols}" style="height:${h}px"></td></tr>` : '';
  let h = pad(first * rowH);
  for (let i = first; i < last; i++) h += L.row(L.items[i], i);
  L.table.tBodies[0].innerHTML = h + pad((n - last) * rowH);
  const real = L.table.tBodies[0].querySelector('tr:not(.pad)')?.getBoundingClientRect().height;
  if (real && Math.abs(real - rowH) > 0.5) { rowH = real; paint(L); }
}

for (const L of Object.values(lists)) {
  let queued = false;
  scroller(L).addEventListener('scroll', () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; paint(L); });
  });
}
let wasSplit = null;
addEventListener('resize', () => {
  if (!S) return;
  if (splitOn() !== wasSplit) return showTab(tab);
  paint(lists.mods);
  paint(lists.plugins);
  renderTicks();
});

/** Scroll a mod's row into view. */
function reveal(id) {
  const L = lists.mods, i = L.items.findIndex(m => m.id === id);
  if (i < 0) return;
  const sc = scroller(L), head = L.table.tHead.offsetHeight;
  if (i * rowH < sc.scrollTop) sc.scrollTop = i * rowH;
  else if (head + (i + 1) * rowH > sc.scrollTop + sc.clientHeight) sc.scrollTop = head + (i + 1) * rowH - sc.clientHeight;
}

// ---- Rendering ------------------------------------------------------------------------------

function render() {
  const i = S.info;
  $('#game-name').textContent = GAMES[i.game] ?? i.game;
  $('#instance-path').textContent = i.root;
  $('#instance-path').title = i.root;
  $('#profile-name').textContent = i.profile;
  $('#undo').disabled = !S.history.length;
  $('#n-mods').textContent = i.mods ? `${i.enabled_mods}/${i.mods}` : '';
  $('#n-plugins').textContent = S.plugins.length ? S.plugins.filter(p => p.enabled).length : '';
  renderMods();
  renderPlugins();
  renderHistory();
  renderDetails();
}

const RECENT = 7 * 86400;
const CRITERIA = {
  enabled: m => m.enabled,
  disabled: m => !m.enabled,
  conflicted: m => { const c = D.C[m.id]; return !!c && c.overwrites.length + c.overwritten_by.length > 0; },
  overridden: m => !!D.C[m.id]?.fully_overridden,
  rule: m => D.broken.has(m.id),
  masters: m => D.missing.has(m.id),
  plugins: m => m.plugins.length > 0,
  recent: m => (m.installed ?? 0) >= Date.now() / 1000 - RECENT,
  problems: m => D.broken.has(m.id) || D.missing.has(m.id) || !!D.C[m.id]?.fully_overridden || !!D.flagged?.has(m.id),
};
const filterText = () => $('#filter').value.trim().toLowerCase();
const filtering = () => !!filterText() || filters.size > 0;

/** The rows to show: separators and the mods of open groups, or, while filtering, the
 *  matching mods under their separators. */
function modView() {
  const out = [];
  if (!filtering()) {
    let hide = false;
    for (const m of S.mods) {
      if (m.separator) { out.push(m); hide = collapsed.has(m.id); }
      else if (!hide) out.push(m);
    }
    return out;
  }
  const f = filterText(), crit = [...filters].map(k => CRITERIA[k]);
  let head = null;
  for (const m of S.mods) {
    if (m.separator) head = m;
    else if ((!f || matches(m, f)) && crit.every(c => c(m))) {
      if (head) { out.push(head); head = null; }
      out.push(m);
    }
  }
  return out;
}

/** Filter text: "#tag" matches tags, anything else the name or the notes. */
function matches(m, f) {
  if (f.startsWith('#')) return f.length > 1 && !!m.tags?.some(t => t.startsWith(f.slice(1)));
  return m.name.toLowerCase().includes(f) || !!m.notes?.toLowerCase().includes(f);
}

function conflictPills(c) {
  if (!c) return '';
  if (c.fully_overridden) {
    return `<span class="pill bad" title="Every file is replaced by ${esc(c.overwritten_by.map(nameOf).join(', '))}; this mod has no effect">Overridden</span>`;
  }
  let h = '';
  if (c.overwrites.length) h += `<span class="pill win" title="Wins file conflicts against ${esc(c.overwrites.map(nameOf).join(', '))}">▲ ${c.overwrites.length}</span>`;
  if (c.overwritten_by.length) h += `<span class="pill lose" title="Loses ${plural(c.losing_files + (c.archived_losing ?? 0), 'file')} to ${esc(c.overwritten_by.map(nameOf).join(', '))}">▼ ${c.overwritten_by.length}</span>`;
  return h;
}

const oneMod = () => selected.size === 1 && !isSep([...selected][0]) ? byId.get([...selected][0]) : null;
const dragCls = (L, id, i) => [
  L.drag?.ids.has(id) ? 'dragging' : '',
  L.drag?.mark === i ? (L.drag.after ? 'drop-after' : 'drop-before') : '',
];

function modRow(m, i) {
  const cls = [selected.has(m.id) ? 'sel' : '', ...dragCls(lists.mods, m.id, i)];
  if (m.separator) return sepRow(m, i, cls);
  const focus = oneMod() && D.C[oneMod().id];
  const beaten = !!focus?.overwrites.includes(m.id), beats = !!focus?.overwritten_by.includes(m.id);
  cls.push(
    m.enabled ? '' : 'off',
    D.group.has(m.id) ? 'child' : '',
    beaten ? 'beaten' : '',
    beats ? 'beats' : '',
    selPluginMod === m.id ? 'rel' : '',
    m.color ? 'tint' : '',
    dropAt?.id === m.id ? 'drop-into' : '',
  );
  // Partners of the selected mod say how they relate, not only by colour.
  const sel = beaten || beats ? nameOf(oneMod().id) : '';
  const relTag = beaten ? `<span class="rel-tag win" title="${esc(sel)} wins against it">▼ loses to ${esc(sel)}</span>`
    : beats ? `<span class="rel-tag lose" title="It wins against ${esc(sel)}">▲ beats ${esc(sel)}</span>` : '';
  const b = D.broken.get(m.id);
  const why = b && [
    b.length ? `Should load after ${b.map(nameOf).join(', ')}` : '',
    b.unmet?.missing.length ? `Requires ${b.unmet.missing.map(nameOf).join(', ')}, not enabled` : '',
    b.unmet?.clashes.length ? `Incompatible with ${b.unmet.clashes.map(nameOf).join(', ')}, enabled too` : '',
  ].filter(Boolean).join('. ') + ' (mod rules)';
  const warn = b ? `<span class="warn-icon" title="${esc(why)}">⚠</span>` : '';
  const masters = D.missing.has(m.id) ? '<span class="pill bad" title="One of its plugins needs masters that aren’t loaded before it">Masters</span>' : '';
  const note = m.notes ? `<span class="note-icon" title="${esc(m.notes)}">✎</span>` : '';
  const up = updates.get(m.id);
  const upd = up ? `<span class="pill win" title="Nexus has ${esc(up.file_name)}${up.latest ? ` (version ${esc(up.latest)})` : ''}">Update${up.latest ? ' ' + esc(up.latest) : ''}</span>` : '';
  const cell = {
    conflicts: () => `<td class="conf">${relTag}${conflictPills(D.C[m.id])}${masters}</td>`,
    tags: () => `<td class="muted small" title="${esc(m.tags?.join(', '))}">${esc(m.tags?.join(', '))}</td>`,
    notes: () => `<td class="muted" title="${esc(m.notes)}">${esc(m.notes)}</td>`,
    installed: () => `<td class="muted small">${m.installed ? new Date(m.installed * 1000).toLocaleDateString() : ''}</td>`,
    files: () => `<td class="num">${m.files.toLocaleString()}</td>`,
    priority: () => `<td class="num">${m.priority}</td>`,
  };
  return `<tr data-i="${i}" data-id="${esc(m.id)}" class="${cls.join(' ')}"${m.color ? ` style="--tint:${esc(m.color)}"` : ''}>
    <td><span class="grip" title="Drag to change priority">⋮⋮</span></td>
    <td><input type="checkbox" ${m.enabled ? 'checked' : ''} aria-label="Enable ${esc(m.name)}"></td>
    <td class="name" title="${esc(m.name)}">${esc(m.name)}${warn}${note} ${upd}</td>
    ${shownCols().map(k => cell[k]()).join('')}</tr>`;
}

/** The mod list's header: the chosen columns, with remembered widths and resize handles. */
function renderModHead() {
  const w = k => prefs.widths[k] ? ` style="width:${prefs.widths[k]}px"` : '';
  const rs = '<span class="rs" title="Drag to resize"></span>';
  $('#mods thead tr').innerHTML = `<th class="grip-col"></th><th class="check-col"></th><th>Mod</th>`
    + shownCols().map(k => `<th class="${COLS[k].cls}" data-col="${k}"${w(k)}${COLS[k].tip ? ` title="${COLS[k].tip}"` : ''}>${rs}${COLS[k].title}</th>`).join('');
}

function sepRow(m, i, cls) {
  const kids = D.kids.get(m.id);
  const open = !collapsed.has(m.id) || filtering();
  let badges = '';
  if (!open) {
    // A closed group still shows what its mods need attention for.
    const count = f => kids.filter(f).length;
    const lose = count(k => D.C[k.id]?.overwritten_by.length && !D.C[k.id].fully_overridden);
    const over = count(k => D.C[k.id]?.fully_overridden);
    const probs = count(k => D.broken.has(k.id) || D.missing.has(k.id));
    if (lose) badges += `<span class="pill lose" title="${plural(lose, 'mod')} in this group lose file conflicts">▼ ${lose}</span>`;
    if (over) badges += `<span class="pill bad" title="${plural(over, 'mod')} in this group are fully overridden">Overridden ${over}</span>`;
    if (probs) badges += `<span class="pill bad" title="${plural(probs, 'mod')} in this group break a mod rule or miss masters">⚠ ${probs}</span>`;
  }
  if (dropAt?.id === m.id) cls.push('drop-into');
  const cols = shownCols(), last = cols.at(-1) === 'priority';
  return `<tr data-i="${i}" data-id="${esc(m.id)}" class="sep ${cls.join(' ')}"${m.color ? ` style="--sep:${esc(m.color)}"` : ''}>
    <td><span class="grip" title="Drag to move the group">⋮⋮</span></td>
    <td><button class="twisty" aria-expanded="${open}" aria-label="${open ? 'Collapse' : 'Expand'} ${esc(m.name)}" ${filtering() ? 'disabled' : ''}>${open ? '▾' : '▸'}</button></td>
    <td class="name" colspan="${1 + cols.length - (last ? 1 : 0)}"><b>${esc(m.name)}</b><span class="muted small">${kids.filter(k => k.enabled).length}/${kids.length}</span>${badges}</td>
    ${last ? `<td class="num">${m.priority}</td>` : ''}</tr>`;
}

function renderMods() {
  const L = lists.mods;
  renderModHead();
  L.items = modView();
  // Bulk actions never reach rows the filter or a closed group hides.
  const shown = new Set(L.items.map(m => m.id));
  for (const id of selected) if (!shown.has(id)) selected.delete(id);
  const on = filtering();
  $('#mods').classList.toggle('filtering', on);
  $('#filter').classList.toggle('on', !!filterText());
  for (const el of ['#match', '#clear-filter', '#enable-matching', '#disable-matching']) $(el).hidden = !on;
  const n = L.items.filter(m => !m.separator).length;
  $('#match').textContent = `${n.toLocaleString()} of ${plural(S.info.mods, 'mod')}`;
  $$('#fchips [data-f]').forEach(b => b.setAttribute('aria-pressed', filters.has(b.dataset.f)));
  paint(L);
  renderTicks();
  $('#mods-empty').hidden = L.items.length > 0;
  $('#mods-empty').textContent = S.mods.length ? 'No mods match the filter.' : 'No mods yet. Drop a .zip or .7z here, or use Install mod.';
  const ow = S.overwrite;
  $('#overwrite-bar').hidden = !ow;
  $('#overwrite-bar').innerHTML = ow ? `<b>Overwrite</b><span class="muted small">${plural(ow, 'file')} that tools, the game or changes outside rmm left. They win over every mod.</span><span class="spacer"></span>
    <button data-ow="show">Show files</button><button data-ow="mod">Create mod…</button><button data-ow="clear" class="danger">Clear…</button>` : '';
  const nb = S.broken.length;
  $('#rules-notice').hidden = !nb;
  $('#rules-notice').innerHTML = nb ? `<span>⚠ ${nb === 1 ? 'A mod rule is' : `${nb} mod rules are`} broken by the current order.</span><button id="sort-mods">Sort by rules…</button>` : '';
}

/** Marks on the scrollbar for rows scrolled out of view: the selection and its conflict
 *  partners. Only while the list scrolls. */
function renderTicks() {
  const L = lists.mods, sc = scroller(L), box = $('#ticks'), n = L.items.length;
  if (!n || !sc.offsetParent || sc.scrollHeight <= sc.clientHeight) { box.innerHTML = ''; return; }
  const head = L.table.tHead.offsetHeight, scale = sc.clientHeight / sc.scrollHeight;
  box.style.top = `${sc.offsetTop}px`;
  box.style.height = `${sc.clientHeight}px`;
  const focus = oneMod() && D.C[oneMod().id];
  let h = '';
  L.items.forEach((m, i) => {
    const k = selected.has(m.id) ? 'sel' : focus?.overwrites.includes(m.id) ? 'win' : focus?.overwritten_by.includes(m.id) ? 'lose' : '';
    if (k) h += `<i class="${k}" style="top:${((head + (i + 0.5) * rowH) * scale).toFixed(1)}px"></i>`;
  });
  box.innerHTML = h;
}

function pluginRow(p, i) {
  const type = p.master ? (p.light ? 'ESL' : 'ESM') : p.light ? 'ESP · light' : 'ESP';
  const from = p.implicit ? 'Base game' : p.mod_id ? nameOf(p.mod_id) : 'Data folder';
  const issue = p.error ? `<span class="pill bad" title="${esc(p.error)}">Unreadable</span>`
    : p.enabled && p.missing_masters.length ? `<span class="pill bad" title="Needs ${esc(p.missing_masters.join(', '))} enabled and loaded earlier">Missing masters</span>` : '';
  const sp = selPlugin && S.plugins.find(x => x.name === selPlugin), om = oneMod();
  const rel = sp ? sp.masters.some(m => m.toLowerCase() === p.name.toLowerCase()) : !!om && p.mod_id === om.id;
  const cls = [p.implicit ? 'locked' : '', p.enabled ? '' : 'off', p.name === selPlugin ? 'sel' : '', rel ? 'rel' : '', ...dragCls(lists.plugins, p.name, i)];
  return `<tr data-i="${i}" data-name="${esc(p.name)}" class="${cls.join(' ')}">
    <td>${p.implicit ? '' : '<span class="grip" title="Drag to change load order">⋮⋮</span>'}</td>
    <td><input type="checkbox" ${p.enabled ? 'checked' : ''} ${p.implicit ? 'disabled title="Always loaded by the game"' : `aria-label="Enable ${esc(p.name)}"`}></td>
    <td class="name" title="${esc(p.name)}">${esc(p.name)}${D.locks.has(p.name.toLowerCase()) ? '<span class="lock-icon" title="Locked at this position: sorts and moves leave it here">🔒</span>' : ''}</td>
    <td class="muted">${type}</td>
    <td class="muted" title="${esc(from)}">${esc(from)}</td>
    <td>${issue}</td>
    <td class="num">${p.idx}</td></tr>`;
}

function renderPlugins() {
  const f = filterText();
  S.plugins.forEach((p, i) => { p.idx = i; });
  lists.plugins.items = f ? S.plugins.filter(p => p.name.toLowerCase().includes(f)) : S.plugins;
  $('#plugins').classList.toggle('filtering', !!f);
  $('#plugins-note').textContent = f
    ? `${lists.plugins.items.length.toLocaleString()} of ${plural(S.plugins.length, 'plugin')}`
    : 'Masters always load before other plugins. Drag to reorder.';
  paint(lists.plugins);
}

function renderHistory() {
  if (!S.history.length) return $('#history').innerHTML = '<li class="muted">No changes to this profile yet.</li>';
  const list = historyData ?? S.history.map(label => ({ label }));
  $('#history').innerHTML = list.map((h, i) => `<li><div class="what"><span>${esc(h.label)}</span>${h.change ? `<span class="muted small">${esc(changeText(h.change)) || 'No change to the lists.'}</span>` : ''}</div>
    <button data-restore="${i}" title="Undo this change and every later one">Restore to before…</button></li>`).join('');
}

/** "disabled A, B; moved C; 2 plugins" */
function changeText(c, max = 6) {
  const names = ids => ids.slice(0, max).map(nameOf).join(', ') + (ids.length > max ? ` and ${ids.length - max} more` : '');
  return [['enabled', c.enabled], ['disabled', c.disabled], ['added', c.added], ['removed', c.removed], ['moved', c.moved]]
    .filter(([, ids]) => ids.length).map(([verb, ids]) => `${verb} ${names(ids)}`)
    .concat(c.plugins ? [`${plural(c.plugins, 'plugin')} changed`] : []).join('; ');
}

async function loadHistory() {
  try { historyData = await call('history_details'); } catch { historyData = null; }
  if (tab === 'history') renderHistory();
}

$('#history').onclick = async e => {
  const n = e.target.closest('[data-restore]')?.dataset.restore;
  if (n === undefined) return;
  const d = await act('Checking', () => call('restore_preview', { n: +n }), { reload: false });
  if (!d || d === true) return;
  const part = (label, ids) => ids.length ? `<details><summary><b>${ids.length}</b> ${label}</summary><pre>${esc(ids.map(nameOf).join('\n'))}</pre></details>` : '';
  const body = `<p>Goes back ${plural(+n + 1, 'change')}, to before “${esc((historyData ?? [])[n]?.label ?? S.history[n])}”. Redo steps forward again.</p>`
    + part('to enable', d.enabled) + part('to disable', d.disabled) + part('to add back', d.added) + part('to remove from the list', d.removed) + part('to move', d.moved)
    + (d.plugins ? `<p class="muted small">${plural(d.plugins, 'plugin')} change state or place.</p>` : '');
  if (await dialog({ title: 'Restore', body, actions: [['cancel', 'Cancel'], ['ok', 'Restore', 'primary']] }) !== 'ok') return;
  const r = await act('Restoring', () => call('restore', { n: +n }));
  if (r) toast(`Went back ${plural(+n + 1, 'change')}. Redo steps forward again.`);
};

function renderDeploy() {
  const b = $('#deploy');
  b.classList.toggle('blocked', !!plan?.error);
  if (!plan) b.textContent = 'Deploy';
  else if (plan.error) b.textContent = '⚠ Deploy blocked';
  else {
    const n = pending(plan);
    b.innerHTML = n ? `Deploy <span class="badge">${n}</span>` : '✓ Deployed';
  }
}
const pending = p => p.link.length + p.remove.length + p.external_changes.length + (p.played?.length ?? 0);

// ---- Details panel: one mod, one separator, or several rows -----------------------------------

function renderDetails() {
  const box = $('#details');
  if (pluginSide()) {
    box.hidden = !selPlugin;
    return selPlugin && renderPluginDetails(box);
  }
  box.hidden = tab !== 'mods' || !selected.size;
  if (box.hidden) return;
  const ids = selIds();
  if (ids.length > 1) return renderBulk(box, ids);
  const m = byId.get(ids[0]);
  if (m.separator) return renderSep(box, m);
  const c = D.C[m.id];
  const R = S.rules.find(r => r.id === m.id) ?? {};
  const ruleList = (key, title, placeholder) => `<h3>${title}</h3>
    <div class="chips">${(R[key] ?? []).map(id => `<span class="chip">${esc(nameOf(id))}<button data-unrule="${esc(id)}" data-key="${key}" title="Remove from the rule" aria-label="Remove ${esc(nameOf(id))}">×</button></span>`).join('') || '<span class="muted">None.</span>'}</div>
    <div class="row"><input class="rule-add" data-key="${key}" list="mod-names" placeholder="${placeholder}" aria-label="${placeholder}" spellcheck="false"></div>`;
  const people = (title, ids) => ids.length
    ? `<div class="small muted">${title}</div><ul>${ids.map(id => `<li><button class="link" data-goto="${esc(id)}">${esc(nameOf(id))}</button></li>`).join('')}</ul>` : '';
  const d = details?.id === m.id ? details : null;
  box.innerHTML = `
    <h2>${esc(m.name)}</h2>
    ${m.id !== m.name ? `<div class="muted small">id: ${esc(m.id)}</div>` : ''}
    <div class="row">
      <label class="row" style="margin:0"><input type="checkbox" data-act="enable" ${m.enabled ? 'checked' : ''}> Enabled</label>
      <div class="spacer"></div>
      <button data-act="move">Move to…</button>
      <button class="danger" data-act="uninstall">Uninstall…</button>
    </div>
    <div class="row">
      <button data-act="rename-mod" title="F2">Rename…</button>
      <button data-act="reinstall" ${d && d.meta?.source && !d.meta.source.startsWith('tool:') ? '' : 'disabled title="Only mods installed from an archive or folder"'}>Reinstall…</button>
      <input type="color" data-act="mod-color" value="${esc(m.color ?? '#808080')}" title="Row colour" aria-label="Row colour">
      ${m.color ? '<button data-act="mod-nocolor" title="Remove the row colour">No colour</button>' : ''}
    </div>
    ${m.tags?.length ? `<div class="tagline">${m.tags.map(t => `<span class="pill plain">${esc(t)}</span>`).join('')}</div>` : ''}
    <h3>Notes</h3>
    <textarea id="mod-note" rows="2" placeholder="Notes for this mod…" spellcheck="true">${esc(m.notes ?? '')}</textarea>
    <h3>Conflicts</h3>
    ${!c || (!c.overwrites.length && !c.overwritten_by.length)
      ? `<p class="muted">No file conflicts${m.enabled ? '' : ' (only enabled mods are compared)'}.</p>`
      : `${c.fully_overridden ? '<p><span class="pill bad">Overridden</span> Every file is replaced by other mods, so this mod has no effect.</p>' : ''}
         ${people('Wins against', c.overwrites)}
         ${people('Loses to', c.overwritten_by)}
         <div class="row"><button data-act="files">Files in conflict…</button></div>
         <div class="legend"><span><i style="background:var(--win)"></i>this mod wins</span><span><i style="background:var(--lose)"></i>wins over this mod</span></div>`}
    ${ruleList('after', 'Loads after', 'Load after another mod…')}
    ${ruleList('requires', 'Requires', 'Needs another mod enabled…')}
    ${ruleList('incompatible', 'Incompatible with', 'Can’t be enabled with…')}
    <p class="muted small">Rules are suggestions: dragging and enabling are never blocked. Problems lists broken ones, and Sort by rules fixes the order.</p>
    <h3>Files (${m.files.toLocaleString()})</h3>
    ${m.plugins.length ? `<p>Plugins: ${esc(m.plugins.join(', '))}</p>` : ''}
    <div class="files">${d ? d.files.map(esc).join('<br>') + (d.files_truncated ? '<br>…' : '') : '…'}</div>
    ${d?.meta?.source ? `<h3>Installed from</h3><div class="small muted">${esc(d.meta.source)}</div>` : ''}`;
}

function renderSep(box, m) {
  const kids = D.kids.get(m.id);
  box.innerHTML = `
    <h2>${esc(m.name)}</h2>
    <div class="muted small">Separator · ${plural(kids.length, 'mod')}, ${kids.filter(k => k.enabled).length.toLocaleString()} enabled</div>
    <div class="row">
      <button data-act="rename" title="F2">Rename…</button>
      <button data-act="select-kids" ${kids.length ? '' : 'disabled'}>Select its mods</button>
    </div>
    <h3>Colour</h3>
    <div class="row">
      <input type="color" data-act="color" value="${esc(m.color ?? '#808080')}" aria-label="Separator colour">
      <button data-act="nocolor" ${m.color ? '' : 'disabled'}>No colour</button>
    </div>
    <div class="row">
      <button data-act="move">Move to…</button>
      <div class="spacer"></div>
      <button class="danger" data-act="uninstall">Remove…</button>
    </div>
    <p class="muted small">A separator moves with its whole group. Removing it leaves the mods where they are.</p>`;
}

function renderBulk(box, ids) {
  const seps = ids.filter(isSep).length, mods = ids.length - seps;
  box.innerHTML = `
    <h2>${plural(ids.length, 'row')} selected</h2>
    <div class="muted small">${[mods && plural(mods, 'mod'), seps && plural(seps, 'separator')].filter(Boolean).join(', ')}</div>
    <div class="stack">
      <button data-act="on" ${mods ? '' : 'disabled'}>Enable</button>
      <button data-act="off" ${mods ? '' : 'disabled'}>Disable</button>
      <button data-act="move">Move to…</button>
      <button class="danger" data-act="uninstall">${mods ? 'Uninstall…' : 'Remove…'}</button>
    </div>
    <p class="muted small">Space enables or disables, Delete uninstalls, Ctrl+↑/↓ moves, Esc clears the selection.</p>`;
}

function renderPluginDetails(box) {
  const p = S.plugins.find(x => x.name === selPlugin);
  const type = p.master ? (p.light ? 'Light master (ESL)' : 'Master (ESM)') : p.light ? 'Light plugin' : 'Plugin';
  const lower = new Set(p.missing_masters.map(m => m.toLowerCase()));
  const rule = lootRules?.rules.find(r => r.plugin.toLowerCase() === p.name.toLowerCase()) ?? { after: [], group: null };
  const loaders = S.plugins.filter(x => x.masters.some(m => m.toLowerCase() === p.name.toLowerCase()));
  box.innerHTML = `
    <h2>${esc(p.name)}</h2>
    <div class="muted small">${type} · position ${S.plugins.indexOf(p)}${p.implicit ? ' · always loaded by the game' : ''}</div>
    ${p.implicit ? '' : `<label class="row"><input type="checkbox" data-act="lock" ${D.locks.has(p.name.toLowerCase()) ? 'checked' : ''}> Lock at this position</label>`}
    ${p.mod_id ? `<p>From <button class="link" data-goto="${esc(p.mod_id)}">${esc(nameOf(p.mod_id))}</button></p>` : `<p class="muted">${p.implicit ? 'Base game' : 'In the Data folder, not from a mod'}</p>`}
    ${p.error ? `<p><span class="pill bad">Unreadable</span> ${esc(p.error)}</p>` : ''}
    <h3>Masters (${p.masters.length})</h3>
    ${p.masters.length ? `<ul>${p.masters.map(m => `<li>${lower.has(m.toLowerCase()) ? '<span class="pill bad">Missing</span>' : ''}<button class="link" data-plugin="${esc(m)}">${esc(m)}</button></li>`).join('')}</ul>` : '<p class="muted">None.</p>'}
    ${loaders.length ? `<h3>Needed by (${loaders.length})</h3><ul>${loaders.slice(0, 50).map(x => `<li><button class="link" data-plugin="${esc(x.name)}">${esc(x.name)}</button></li>`).join('')}</ul>` : ''}
    <h3>LOOT rule</h3>
    ${!lootRules ? '<p class="muted">Loading…</p>' : `
    <div class="small muted">Loads after</div>
    <div class="chips">${rule.after.map(a => `<span class="chip">${esc(a)}<button data-unloot="${esc(a)}" title="Remove from the rule" aria-label="Remove ${esc(a)}">×</button></span>`).join('') || '<span class="muted">Nothing.</span>'}</div>
    <div class="row"><input id="loot-after" list="plugin-names" placeholder="Load after another plugin…" aria-label="Add a plugin this one loads after" spellcheck="false"></div>
    <label class="field">Group<input id="loot-group" list="loot-groups" value="${esc(rule.group ?? '')}" placeholder="${lootRules.groups.length ? 'default' : 'Groups come with LOOT’s masterlist'}" spellcheck="false"></label>
    <datalist id="plugin-names">${S.plugins.filter(x => x.name !== p.name).map(x => `<option value="${esc(x.name)}">`).join('')}</datalist>
    <datalist id="loot-groups">${lootRules.groups.map(g => `<option value="${esc(g)}">`).join('')}</datalist>
    <p class="muted small">Kept in LOOT’s own userlist, so LOOT reads the same rules. Sort with LOOT applies them.</p>`}`;
}

async function loadLootRules() {
  try { lootRules = await call('loot_rules'); } catch (e) { lootRules = { rules: [], groups: [] }; toast(`Couldn’t read LOOT’s rules.\n${e}`, 'warn'); }
  if (pluginSide()) renderDetails();
}

function setLootRule(change) {
  const p = selPlugin;
  const old = lootRules?.rules.find(r => r.plugin.toLowerCase() === p.toLowerCase()) ?? { after: [], group: null };
  const rule = { plugin: p, after: old.after, group: old.group, ...change };
  act('Changing the LOOT rule', () => call('set_loot_rule', rule));
}

function pickPlugin(name) {
  selPlugin = name;
  side = 'plugins';
  selPluginMod = name ? S.plugins.find(p => p.name === name)?.mod_id ?? null : null;
  if (name && !lootRules) loadLootRules();
  paint(lists.plugins);
  if (splitOn()) { paint(lists.mods); if (selPluginMod) reveal(selPluginMod); }
  renderDetails();
  const i = lists.plugins.items.findIndex(p => p.name === name);
  const sc = scroller(lists.plugins);
  if (i >= 0 && (i * rowH < sc.scrollTop || (i + 2) * rowH > sc.scrollTop + sc.clientHeight)) sc.scrollTop = Math.max(0, (i - 3) * rowH);
}

async function loadDetails() {
  const id = oneMod()?.id;
  if (!id) return;
  try { details = await call('details', { id }); } catch { details = null; }
  if (oneMod()?.id === id) renderDetails();
}

// ---- Selection ------------------------------------------------------------------------------

/** Selected rows in list order. */
const selIds = () => lists.mods.items.filter(m => selected.has(m.id)).map(m => m.id);

/** Click (or arrow) selection: plain replaces, Ctrl toggles, Shift selects the range from the anchor. */
function pick(id, { shift = false, ctrl = false } = {}) {
  const view = lists.mods.items;
  const a = shift ? view.findIndex(m => m.id === anchor) : -1;
  if (a >= 0) {
    const b = view.findIndex(m => m.id === id);
    if (!ctrl) selected.clear();
    for (let i = Math.min(a, b); i <= Math.max(a, b); i++) selected.add(view[i].id);
  } else if (ctrl) {
    if (!selected.delete(id)) selected.add(id);
    anchor = id;
  } else {
    selected = new Set([id]);
    anchor = id;
  }
  cur = id;
  side = 'mods';
  selectionChanged();
}

function selectionChanged() {
  paint(lists.mods);
  renderTicks();
  if (splitOn()) paint(lists.plugins);
  renderDetails();
  if (oneMod() && details?.id !== oneMod().id) loadDetails();
}

/** Select a mod from a link, opening its group and clearing a filter that hides it. */
function goTo(id) {
  const g = D.group.get(id);
  if (g && collapsed.delete(g)) saveCollapsed();
  if (filtering() && !modView().some(m => m.id === id)) { filters.clear(); $('#filter').value = ''; }
  renderMods();
  pick(id);
  reveal(id);
}

function toggleGroup(id, open = collapsed.has(id)) {
  if (filtering() || open !== collapsed.has(id)) return;
  if (open) collapsed.delete(id); else collapsed.add(id);
  saveCollapsed();
  renderMods();
  renderDetails();
}

// ---- Mod actions ----------------------------------------------------------------------------

// Enable and move show their result at once; the reload that follows (half a second on 3,000
// mods) then brings conflicts and counts up to date.
function setEnabled(ids, enabled) {
  ids = ids.filter(id => !isSep(id));
  if (!ids.length || busy) return;
  for (const id of ids) byId.get(id).enabled = enabled;
  paint(lists.mods);
  return act(enabled ? 'Enabling' : 'Disabling', () => call('enable_mods', { ids, enabled }));
}

async function removeSelected() {
  const ids = selIds();
  if (!ids.length) return;
  const seps = ids.filter(isSep).length, mods = ids.length - seps;
  const title = ids.length > 1 ? `Remove ${[mods && plural(mods, 'mod'), seps && plural(seps, 'separator')].filter(Boolean).join(' and ')}?`
    : seps ? `Remove separator “${nameOf(ids[0])}”?` : `Uninstall ${nameOf(ids[0])}?`;
  const text = mods ? 'Mod files move to the instance’s trash folder. Undo brings them back.' : 'Its mods join the group above. Undo brings it back.';
  if (await confirmBox(title, text, mods ? 'Uninstall' : 'Remove')) act('Uninstalling', () => call('uninstall', { ids }));
}

/** What a move takes: the ids in list order, each separator with its whole group. */
function block(ids) {
  const set = new Set(ids), out = [];
  let g = null;
  for (const m of S.mods) {
    if (m.separator) g = m.id;
    if (set.has(m.id) || (g && set.has(g))) out.push(m.id);
  }
  return out;
}
const restOf = ids => { const s = new Set(ids); return S.mods.map(m => m.id).filter(id => !s.has(id)); };
/** Index in `rest` just past a separator's group. */
function groupEnd(rest, sep) {
  let j = rest.indexOf(sep) + 1;
  while (j < rest.length && !isSep(rest[j])) j++;
  return j;
}

/** Move a block so it starts at `at` in the list without it. */
function moveTo(ids, at) {
  const order = restOf(ids);
  order.splice(at, 0, ...ids);
  if (busy || order.every((id, i) => id === S.mods[i].id)) return;
  S.mods = order.map(id => byId.get(id));
  S.mods.forEach((m, i) => { m.priority = i; });
  derive();
  renderMods();
  act('Moving', () => call('move_mods', { ids, priority: at }));
}

/** Ctrl+Up/Down: one place, or past a whole group when moving a group, so none is split. */
function nudge(dir) {
  if (filtering()) return toast('Clear the filter to reorder.');
  const ids = block(selIds());
  if (!ids.length) return;
  const rest = restOf(ids), first = S.mods.findIndex(m => m.id === ids[0]);
  let at;
  if (isSep(ids[0])) {
    const seps = rest.flatMap((id, i) => isSep(id) ? [i] : []);
    if (dir < 0) at = seps.filter(i => i < first).pop();
    else { const next = seps.find(i => i >= first); at = next === undefined ? undefined : groupEnd(rest, rest[next]); }
    if (at === undefined) return;
  } else at = Math.max(0, Math.min(rest.length, first + dir));
  moveTo(ids, at);
}

async function moveDialog() {
  const ids = block(selIds());
  if (!ids.length) return;
  const set = new Set(ids), rest = restOf(ids);
  const partners = new Set();
  for (const id of ids) {
    const c = D.C[id];
    for (const o of c ? [...c.overwrites, ...c.overwritten_by] : []) if (!set.has(o)) partners.add(o);
  }
  const pos = [...partners].map(id => rest.indexOf(id)).filter(i => i >= 0);
  const seps = rest.filter(isSep);
  const opt = (v, label, note, off) => `<button value="${v}" ${off ? 'disabled' : ''}><b>${label}</b><span class="muted small">${note}</span></button>`;
  const body = `<div class="moveto">
      ${opt('top', 'Top', 'Lowest priority: loses every conflict')}
      ${opt('bottom', 'Bottom', 'Highest priority: wins every conflict')}
      ${opt('first', 'Before its first conflict', pos.length ? 'Loses to every mod it conflicts with' : 'No conflicts among enabled mods', !pos.length)}
      ${opt('last', 'After its last conflict', pos.length ? 'Wins against every mod it conflicts with' : 'No conflicts among enabled mods', !pos.length)}
    </div>
    ${seps.length ? `<label class="field">To the end of a group<select id="mv-group"><option value=""></option>${seps.map(s => `<option value="${esc(s)}">${esc(nameOf(s))}</option>`).join('')}</select></label>` : ''}
    <label class="field">After a mod<input id="mv-after" list="mod-names" placeholder="Type a name" spellcheck="false"></label>`;
  const title = `Move ${ids.length === 1 ? nameOf(ids[0]) : plural(ids.length, 'row')}`;
  const done = dialog({ title, body, actions: [['cancel', 'Cancel'], ['pick', 'Move', 'primary']] });
  // Enter in the name field moves (the form's first button would be "Top").
  $('#mv-after').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); $('#dlg').close('pick'); } };
  const r = await done;
  let at;
  if (!r) return;
  else if (r === 'top') at = 0;
  else if (r === 'bottom') at = rest.length;
  else if (r === 'first') at = Math.min(...pos);
  else if (r === 'last') at = Math.max(...pos) + 1;
  else {
    const g = $('#mv-group')?.value, typed = $('#mv-after').value.trim(), after = idByName.get(typed);
    if (g) at = groupEnd(rest, g);
    else if (after && !set.has(after)) at = rest.indexOf(after) + 1;
    else return toast(typed ? `No other mod is named “${typed}”.` : 'Pick a group or a mod to move after.', 'warn');
  }
  moveTo(ids, at);
}

/** Replace one list of a mod's rule, keeping the others. */
function setModRule(id, key, list) {
  const old = S.rules.find(r => r.id === id) ?? {};
  const rule = { id, after: old.after ?? [], requires: old.requires ?? [], incompatible: old.incompatible ?? [], [key]: list };
  act('Changing the rule', () => call('set_rule', rule));
}

async function renameMod(id) {
  const name = await ask('Rename mod', 'New name for the mod. Its folder keeps its id.', nameOf(id));
  if (!name || name === nameOf(id)) return;
  act('Renaming the mod', () => call('rename_mod', { id, name }));
}

/** Install a mod again from where it came from; a FOMOD installer opens with its stored choices. */
async function reinstall(id) {
  const d = details?.id === id ? details : await call('details', { id }).catch(() => null);
  const path = d?.meta?.source;
  if (!path) return toast('This mod wasn’t installed from an archive or folder.', 'warn');
  let choices;
  const walk = await act(`Reading ${basename(path)}`, async () => {
    try { return await call('fomod', { path, choices: d.meta.fomod ?? null }); }
    catch { return call('fomod', { path }); } // the stored choices no longer fit; start from the defaults
  }, { reload: false });
  if (!walk) return;
  if (walk === true) {
    if (!await confirmBox(`Reinstall ${nameOf(id)}?`, `Its files are read again from ${path}. The old files go to the instance's trash folder; this can't be undone.`, 'Reinstall')) return;
  } else {
    choices = await fomodWizard(path, walk);
    if (!choices) return;
  }
  const r = await act(`Reinstalling ${nameOf(id)}`, () => call('reinstall', { id, choices }));
  if (r) toast(`Reinstalled ${r.name} (${plural(r.files, 'file')}).${r.warnings.length ? '\n' + r.warnings.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
}

async function renameSep(id) {
  const name = await ask('Rename separator', 'New name for the separator.', nameOf(id));
  if (!name || name === nameOf(id)) return;
  act('Renaming the separator', async () => {
    const nid = await call('edit_separator', { id, name });
    selected = new Set([nid]);
    anchor = cur = nid;
    if (collapsed.delete(id)) { collapsed.add(nid); saveCollapsed(); }
  });
}

$('#add-sep').onclick = async () => {
  const name = await ask('Add a separator', 'A separator heads a group of mods. It goes above the selected rows, or at the end.', '');
  if (!name) return;
  const first = selIds()[0];
  const id = await act('Adding the separator', async () => {
    const id = await call('add_separator', { name, priority: first ? byId.get(first).priority : undefined });
    selected = new Set([id]);
    anchor = cur = id;
    return id;
  });
  if (id) reveal(id);
};

$('#overwrite-bar').onclick = async e => {
  const what = e.target.closest('[data-ow]')?.dataset.ow;
  if (what === 'show') {
    const files = await act('Reading overwrite', () => call('overwrite_files'), { reload: false });
    if (files) dialog({ title: 'Overwrite', body: `<p class="muted small">Deployed on top of every mod. Create a mod from them to give them a place in the list.</p><pre>${esc(files.join('\n'))}</pre>`, actions: [['cancel', 'Close']] });
  } else if (what === 'mod') {
    const name = await ask('Create a mod from overwrite', 'Everything in overwrite becomes a new mod at the end of the list.', 'Overwrite output');
    if (name) act('Creating the mod', async () => { const r = await call('mod_from_overwrite', { name }); toast(`Created ${r.name} (${plural(r.files, 'file')}).`); });
  } else if (what === 'clear') {
    if (await confirmBox('Clear overwrite?', `Moves ${plural(S.overwrite, 'file')} to the instance's trash/overwrite folder, replacing what an earlier clear put there. The next deploy removes them from the game folder. This isn't in the undo history.`, 'Clear')) {
      act('Clearing overwrite', () => call('clear_overwrite'));
    }
  }
};

// ---- Problems -------------------------------------------------------------------------------

async function runHealth() {
  const run = ++healthRun;
  issuesStale = false;
  $('#problems-note').textContent = 'Checking…';
  let found;
  try { found = await call('health'); } catch (e) { found = [{ severity: 'error', subject: 'Health check', message: String(e) }]; }
  if (run !== healthRun) return; // a newer check is on its way
  issues = found;
  renderProblems();
}

/** The mod or plugin a finding is about, so clicking it selects that row. */
function targetOf(i) {
  if (byId.has(i.subject)) return { mod: i.subject };
  const p = S.plugins.find(p => p.name.toLowerCase() === i.subject.toLowerCase());
  if (p) return { plugin: p.name };
  const owner = i.message.split(':')[0]; // archive findings start with the mod's id
  return byId.has(owner) ? { mod: owner } : null;
}
const keyOf = i => `${i.kind}|${i.subject}`;
let showIgnored = false;

function renderProblems() {
  const rank = { error: 0, warning: 1, info: 2 };
  const all = [...issues].sort((a, b) => rank[a.severity] - rank[b.severity]);
  const hidden = all.filter(i => ignored.has(keyOf(i))).length;
  const list = showIgnored ? all : all.filter(i => !ignored.has(keyOf(i)));
  $('#issues').innerHTML = list.length
    ? list.map(i => {
      const k = keyOf(i), ign = ignored.has(k), to = i.kind && targetOf(i);
      return `<li class="${esc(i.severity)}${to ? ' link' : ''}${ign ? ' ignored' : ''}" data-key="${esc(k)}" ${to ? `data-mod="${esc(to.mod ?? '')}" data-plugin="${esc(to.plugin ?? '')}" title="Show it in the list"` : ''}>
        <span class="sev">${esc(i.severity)}</span><span class="subject">${esc(to?.mod ? nameOf(to.mod) : i.subject)}</span><span class="message">${esc(i.message)}</span>
        ${i.kind ? `<button class="link ign" data-ignore="${esc(k)}">${ign ? 'Stop ignoring' : 'Ignore'}</button>` : ''}</li>`;
    }).join('')
    : `<li class="info"><span class="sev">ok</span><span class="subject">No problems found${hidden ? ' apart from ignored ones' : ''}.</span></li>`;
  $('#show-ignored').hidden = !hidden;
  $('#show-ignored').textContent = showIgnored ? 'Hide ignored' : `Show ${hidden} ignored`;
  // Mods with a finding, for the Problems filter.
  D.flagged = new Set(issues.filter(i => !ignored.has(keyOf(i)) && i.severity !== 'info').map(i => targetOf(i)?.mod).filter(Boolean));
  const shown = issues.filter(i => !ignored.has(keyOf(i)));
  const serious = shown.filter(i => i.severity !== 'info').length;
  $('#n-problems').textContent = serious || '';
  $('#n-problems').classList.toggle('bad', shown.some(i => i.severity === 'error'));
  if (filters.has('problems')) renderMods();
  $('#problems-note').textContent = `${plural(issues.length, 'finding')}. LOOT's messages are included once its masterlist is downloaded.`;
}

// ---- Tabs, filters, keys --------------------------------------------------------------------

function showTab(t) {
  tab = t;
  prefs.tab = t;
  savePrefs();
  const split = wasSplit = splitOn();
  $$('.tabs [data-tab]').forEach(b => b.classList.toggle('on', b.dataset.tab === t));
  for (const p of ['mods', 'plugins', 'problems', 'tools', 'history']) $('#pane-' + p).hidden = p !== t && !(split && t === 'mods' && p === 'plugins');
  $('main').classList.toggle('split', split && t === 'mods');
  $('#split').hidden = innerWidth < 1500;
  $('#split').setAttribute('aria-pressed', split);
  $('#filter').hidden = t !== 'mods' && t !== 'plugins';
  if (lists[t]) paint(lists[t]);
  if (split && t === 'mods') paint(lists.plugins);
  if (t === 'mods') renderTicks();
  if (t === 'history' && !historyData) loadHistory();
  renderDetails();
  if (t === 'problems' && issuesStale) runHealth();
  if (t === 'tools' && !toolsData) loadTools();
  if (t === 'plugins' && selPlugin && !lootRules) loadLootRules();
}
$$('.tabs [data-tab]').forEach(b => { b.onclick = () => showTab(b.dataset.tab); });
$('#recheck').onclick = runHealth;

const when = s => s ? new Date(s * 1000).toLocaleString() : '';
const modLink = id => !id ? '<span class="muted">not from a mod</span>'
  : `<button class="link" data-goto="${esc(id)}">${esc(nameOf(id))}</button>${byId.get(id)?.enabled ? '' : ' <span class="muted">(disabled now)</span>'}`;

/** The newest crash log (or `path`), with the mods behind what it names. */
async function crashLog(path) {
  const r = await act('Reading the crash log', () => call('crash_log', { path }), { reload: false });
  if (!r || r === true) return;
  const rows = r.suspects.map(s => `<tr><td>${esc(s.name)}${s.in_call_stack ? ' <span class="pill lose" title="Named in the call stack">stack</span>' : ''}</td><td>${modLink(s.mod_id)}</td></tr>`).join('');
  const others = r.others.filter(o => o.path !== r.path);
  const body = `<p class="muted small">${esc(basename(r.path))} · ${esc(when(r.modified))}${r.logger ? ` · ${esc(r.logger)}` : ''}${r.game_version ? ` · game ${esc(r.game_version)}` : ''}</p>
    ${r.error ? `<p><b>${esc(r.error)}</b></p>` : ''}
    ${rows ? `<p>Named in the log, most likely first:</p><table class="fc">${rows}</table>` : '<p class="muted">The log names nothing that mods provide.</p>'}
    ${r.call_stack.length ? `<details><summary>Call stack</summary><pre>${esc(r.call_stack.join('\n'))}</pre></details>` : ''}
    ${others.length ? `<label class="field">Older crash logs<select id="older"><option value="">Choose…</option>${others.map(o => `<option value="${esc(o.path)}">${esc(basename(o.path))} · ${esc(when(o.modified))}</option>`).join('')}</select></label>` : ''}
    <p class="muted small" style="margin-top:10px">A name in a crash log is a lead, not a verdict: the mod that crashed often isn't the one at fault.</p>`;
  const done = dialog({ title: 'Crash log', body, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  $('#dlg').onclick = e => { const id = e.target.closest('[data-goto]')?.dataset.goto; if (id) { $('#dlg').close(); showTab('mods'); goTo(id); } };
  const older = $('#older');
  if (older) older.onchange = () => { const p = older.value; if (p) { $('#dlg').close(); setTimeout(() => crashLog(p)); } };
  await done;
  $('#dlg').onclick = null;
}
$('#crash-log').onclick = () => crashLog(null);

$('#test-run').onclick = async () => {
  const body = `<p>Deploys, starts the game through the script extender and checks that it holds up. With rmm's harness plugin installed as a mod it waits for the main menu, loads the save or goes to each cell with <code>coc</code>, and measures FPS; without it, it only watches the game. A crash log written during the run fails it.</p>
    <label class="field">Save to load <input id="tr-save" type="text" placeholder="optional: the save's name without extension" spellcheck="false"></label>
    <label class="field">Cells to visit <input id="tr-cells" type="text" placeholder="Riverwood, WhiterunBreezehome" spellcheck="false"></label>
    <label class="field">Seconds in each cell <input id="tr-dwell" type="number" min="1" value="10"></label>
    <label class="field">Window <select id="tr-window"><option value="960x540">Small, behind your other windows</option><option value="1280x720">1280×720, behind your other windows</option><option value="fullscreen">The game’s own display settings</option></select></label>
    <label class="row" style="margin-top:12px"><input id="tr-shots" type="checkbox" checked> A screenshot in each cell</label>`;
  if (await dialog({ title: 'Test run', body, actions: [['cancel', 'Cancel'], ['run', 'Start the game', 'primary']] }) !== 'run') return;
  const scenario = {
    save: $('#tr-save').value.trim() || null,
    cells: $('#tr-cells').value.split(',').map(c => c.trim()).filter(Boolean),
    dwell: Math.max(1, Number($('#tr-dwell').value) || 10),
    window: $('#tr-window').value,
    screenshots: $('#tr-shots').checked,
  };
  const r = await act('The test run', () => call('test_run', scenario));
  if (r && r !== true) await showTestRun(r);
};

/** A test run's result: pass or fail, timings, FPS per cell, crash suspects and screenshots. */
async function showTestRun(r) {
  const fps = f => f ? `${f.avg.toFixed(0)} FPS, lowest ${f.min.toFixed(0)}` : '';
  const cells = r.cells.map(c => `<tr><td>${esc(c.cell)}</td><td>${c.ok ? fps(c.fps) : `<span class="pill lose">${esc(c.error ?? 'failed')}</span>`}</td></tr>`).join('');
  const times = Object.entries(r.timings).sort((a, b) => a[1] - b[1]).map(([k, v]) => `${esc(k.replace('_', ' '))} ${v.toFixed(1)} s`).join(' · ');
  const suspects = (r.crash?.suspects ?? []).slice(0, 5).map(s => `<tr><td>${esc(s.name)}</td><td>${modLink(s.mod_id)}</td></tr>`).join('');
  const shots = await Promise.all(r.screenshots.map(p => call('screenshot', { path: p }).catch(() => null)));
  const how = r.mode === 'harness' ? `scripted through the harness${r.runtime ? `, game ${r.runtime}` : ''}` : 'watched the game (no harness plugin)';
  const body = `<p><b>${r.passed ? 'Passed' : 'Failed'}</b>: ${esc(r.summary)}</p>
    <p class="muted small">${esc(how)}${times ? ` · ${times}` : ''}</p>
    ${r.fps ? `<p>After loading the save: ${fps(r.fps)}</p>` : ''}
    ${cells ? `<table class="fc">${cells}</table>` : ''}
    ${r.crash ? `<p>The game crashed${suspects ? '. Named in the crash log, most likely first:' : '.'}</p>${suspects ? `<table class="fc">${suspects}</table>` : ''}` : ''}
    ${r.papyrus?.errors ? `<p class="muted small">The Papyrus log has ${plural(r.papyrus.errors, 'error')} from this run (Papyrus log…).</p>` : ''}
    ${r.warnings?.length ? `<ul class="small">${r.warnings.map(w => `<li>${esc(w)}</li>`).join('')}</ul>` : ''}
    ${shots.map((src, i) => src ? `<figure class="shot"><img src="${src}" alt="Screenshot: ${esc(basename(r.screenshots[i]))}"><figcaption class="muted small">${esc(basename(r.screenshots[i]))}</figcaption></figure>` : '').join('')}`;
  const done = dialog({ title: 'Test run', body, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  $('#dlg').onclick = e => { const id = e.target.closest('[data-goto]')?.dataset.goto; if (id) { $('#dlg').close(); showTab('mods'); goTo(id); } };
  await done;
  $('#dlg').onclick = null;
}

$('#papyrus-log').onclick = async () => {
  const r = await act('Reading the Papyrus log', () => call('papyrus_log'), { reload: false });
  if (!r || r === true) return;
  const off = r.logging_enabled ? '' : '<p>Papyrus logging is off, so the game writes no new log. Set <code>bEnableLogging=1</code> under <code>[Papyrus]</code> in the game’s Custom INI to turn it on.</p>';
  const body = off + (!r.modified ? '<p class="muted">There is no Papyrus log yet.</p>' : `
    <p class="muted small">${esc(basename(r.path))} · ${esc(when(r.modified))} · ${plural(r.errors, 'error')}, ${plural(r.warnings, 'warning')}</p>
    ${r.scripts.length ? `<p>Scripts in errors:</p><table class="fc">${r.scripts.slice(0, 30).map(s => `<tr><td>${esc(s.script)} <span class="muted">×${s.count}</span></td><td>${modLink(s.mod_id)}</td></tr>`).join('')}</table>` : ''}
    ${r.messages.length ? `<details open><summary>Errors by message</summary><pre>${esc(r.messages.slice(0, 30).map(([m, n]) => `${String(n).padStart(5)}  ${m}`).join('\n'))}</pre></details>` : ''}
    ${r.tail.length ? `<details><summary>Last lines</summary><pre>${esc(r.tail.join('\n'))}</pre></details>` : ''}`);
  const done = dialog({ title: 'Papyrus log', body, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  $('#dlg').onclick = e => { const id = e.target.closest('[data-goto]')?.dataset.goto; if (id) { $('#dlg').close(); showTab('mods'); goTo(id); } };
  await done;
  $('#dlg').onclick = null;
};
$('#show-ignored').onclick = () => { showIgnored = !showIgnored; renderProblems(); };
$('#issues').onclick = e => {
  const ign = e.target.closest('[data-ignore]')?.dataset.ignore;
  if (ign) {
    if (!ignored.delete(ign)) ignored.add(ign);
    saveIgnored();
    return renderProblems();
  }
  const li = e.target.closest('li.link');
  if (!li) return;
  if (li.dataset.mod) { showTab('mods'); goTo(li.dataset.mod); }
  else if (li.dataset.plugin) { showTab('plugins'); pickPlugin(li.dataset.plugin); }
};

function filterChanged() {
  prefs.filter = $('#filter').value;
  prefs.chips = [...filters];
  savePrefs();
  scroller(lists.mods).scrollTop = scroller(lists.plugins).scrollTop = 0;
  renderMods();
  renderPlugins();
  renderDetails();
}
function clearFilters() {
  filters.clear();
  $('#filter').value = '';
  filterChanged();
}
$('#filter').oninput = () => { clearTimeout(filterTimer); filterTimer = setTimeout(filterChanged, 120); };
$('#fchips').onclick = e => {
  const f = e.target.closest('[data-f]')?.dataset.f;
  if (!f) return;
  if (!filters.delete(f)) {
    filters.add(f);
    if (f === 'enabled') filters.delete('disabled');
    if (f === 'disabled') filters.delete('enabled');
  }
  filterChanged();
};
$('#clear-filter').onclick = clearFilters;
$('#split').onclick = () => { prefs.split = !splitOn(); savePrefs(); showTab('mods'); };

// Columns: right-click the header to choose them; drag a header's right edge to resize it.
$('#mods thead').addEventListener('contextmenu', async e => {
  e.preventDefault();
  const body = `<div class="stack">${Object.entries(COLS).map(([k, c]) => `<label class="row" style="margin:0"><input type="checkbox" data-col="${k}" ${prefs.cols[k] ? 'checked' : ''}> ${c.title === '#' ? 'Priority' : c.title}</label>`).join('')}</div>`;
  const done = dialog({ title: 'Columns', body, actions: [['reset', 'Reset widths'], ['cancel', 'Close']] });
  $('#dlg').onchange = ev => {
    const k = ev.target.dataset.col;
    if (!k) return;
    prefs.cols[k] = ev.target.checked;
    savePrefs();
    renderMods();
  };
  if (await done === 'reset') { prefs.widths = {}; savePrefs(); renderMods(); }
  $('#dlg').onchange = null;
});
$('#mods thead').addEventListener('pointerdown', e => {
  const th = e.target.closest('.rs')?.closest('th');
  if (!th) return;
  e.preventDefault();
  const x0 = e.clientX, w0 = th.offsetWidth, k = th.dataset.col;
  const move = ev => { th.style.width = `${Math.max(40, w0 + ev.clientX - x0)}px`; };
  addEventListener('pointermove', move);
  addEventListener('pointerup', () => {
    removeEventListener('pointermove', move);
    prefs.widths[k] = th.offsetWidth;
    savePrefs();
    paint(lists.mods);
  }, { once: true });
});
$('#enable-matching').onclick = () => setEnabled(lists.mods.items.map(m => m.id), true);
$('#disable-matching').onclick = () => setEnabled(lists.mods.items.map(m => m.id), false);

const undo = () => act('Undo', async () => { const l = await call('undo'); toast(l ? `Undid: ${l}` : 'Nothing to undo.'); });
const redo = () => act('Redo', async () => { const l = await call('redo'); toast(l ? `Redid: ${l}` : 'Nothing to redo.'); });
$('#undo').onclick = undo;
$('#redo').onclick = redo;

document.addEventListener('keydown', e => {
  if (!S || $('#dlg').open) return;
  const k = e.key, low = k.toLowerCase();
  if (e.ctrlKey && low === 'f' && lists[tab]) {
    e.preventDefault();
    $('#filter').focus();
    $('#filter').select();
    return;
  }
  if (e.target.matches('input, select, textarea')) {
    if (k === 'Escape' && e.target.id === 'filter') {
      e.preventDefault();
      if ($('#filter').value) { $('#filter').value = ''; filterChanged(); } else { clearFilters(); $('#filter').blur(); }
    }
    return;
  }
  if (e.ctrlKey && low === 'z' && !e.shiftKey) { e.preventDefault(); undo(); }
  else if (e.ctrlKey && (low === 'y' || (low === 'z' && e.shiftKey))) { e.preventDefault(); redo(); }
  else if (tab === 'mods' && !(e.target.closest('button') && (k === ' ' || k === 'Enter'))) modKeys(e);
});

function modKeys(e) {
  const view = lists.mods.items, k = e.key;
  const i = view.findIndex(m => m.id === cur);
  const page = Math.max(1, Math.floor(scroller(lists.mods).clientHeight / rowH) - 2);
  const go = j => {
    if (!view.length) return;
    const id = view[Math.max(0, Math.min(view.length - 1, j))].id;
    pick(id, { shift: e.shiftKey });
    reveal(id);
  };
  const moves = { ArrowUp: -1, ArrowDown: 1, PageUp: -page, PageDown: page };
  const c = byId.get(cur);
  if (k in moves) {
    e.preventDefault();
    if (e.ctrlKey && Math.abs(moves[k]) === 1) nudge(moves[k]);
    else go(i < 0 ? 0 : i + moves[k]);
  } else if (k === 'Home' || k === 'End') { e.preventDefault(); go(k === 'Home' ? 0 : view.length - 1); }
  else if ((k === 'ArrowLeft' || k === 'ArrowRight') && c?.separator) toggleGroup(c.id, k === 'ArrowRight');
  else if (e.ctrlKey && k.toLowerCase() === 'a') {
    e.preventDefault();
    selected = new Set(view.map(m => m.id));
    selectionChanged();
  } else if (k === ' ') {
    e.preventDefault();
    const ids = selIds().filter(id => !isSep(id));
    if (ids.length) setEnabled(ids, !ids.every(id => byId.get(id).enabled));
  } else if (k === 'Delete') removeSelected();
  else if (k === 'F2' && c && selected.has(c.id)) { e.preventDefault(); c.separator ? renameSep(c.id) : renameMod(c.id); }
  else if (k === 'Escape') {
    if (filtering()) clearFilters();
    else if (selected.size) { selected.clear(); selectionChanged(); }
  }
}

// ---- Mods -----------------------------------------------------------------------------------

$('#mods tbody').addEventListener('click', e => {
  const tr = e.target.closest('tr[data-id]');
  if (!tr) return;
  const id = tr.dataset.id;
  if (e.target.matches('input[type=checkbox]')) {
    if (busy) return e.preventDefault();
    // A checkbox in a selection of several applies to all of them.
    setEnabled(selected.has(id) && selected.size > 1 ? selIds() : [id], e.target.checked);
  } else if (e.target.closest('.twisty')) toggleGroup(id);
  else if (!e.target.closest('.grip')) pick(id, { shift: e.shiftKey, ctrl: e.ctrlKey });
});
$('#mods tbody').addEventListener('dblclick', e => {
  const tr = e.target.closest('tr.sep');
  if (tr && !e.target.closest('.twisty, .grip')) return toggleGroup(tr.dataset.id);
  const mod = e.target.closest('td.conf')?.closest('tr[data-id]');
  if (mod) fileConflicts(mod.dataset.id);
});

/** Every file a mod shares with others: who provides it, in load order, and who wins. */
async function fileConflicts(id) {
  const all = await act('Reading the conflicts', () => call('file_conflicts', { id }), { reload: false });
  if (!Array.isArray(all)) return;
  if (!all.length) return toast(`${nameOf(id)} has no file conflicts${byId.get(id)?.enabled ? '' : ' (only enabled mods are compared)'}.`);
  const win = all.filter(f => f.winner === id), lose = all.filter(f => f.winner !== id);
  const chain = f => f.mods.map((m, i) => {
    const where = f.archives?.length ? ` <span class="muted">${f.archives[i] ? esc(f.archives[i]) : 'loose'}</span>` : '';
    const label = `${esc(nameOf(m))}${where}`;
    return m === f.winner ? `<b>${label}</b>` : m === id ? `<span class="me">${label}</span>` : label;
  }).join(' <span class="muted">→</span> ');
  const MAX = 1000;
  const part = (title, list, f) => {
    const m = f ? list.filter(x => x.file.toLowerCase().includes(f)) : list;
    return `<details open><summary><b>${m.length.toLocaleString()}</b> ${title}</summary><table class="fc">${m.slice(0, MAX).map(x => `<tr><td>${esc(x.file)}</td><td>${chain(x)}</td></tr>`).join('')}</table>`
      + (m.length > MAX ? `<p class="muted small">${(m.length - MAX).toLocaleString()} more; filter to narrow them down.</p>` : '') + '</details>';
  };
  const done = dialog({
    title: `File conflicts: ${nameOf(id)}`,
    body: `<p class="muted small">Providers load left to right; the winner is in bold. Archives lose to every loose file.</p><input id="fc-filter" type="search" placeholder="Filter files" spellcheck="false"><div id="fc"></div>`,
    actions: [['cancel', 'Close']],
  });
  $('#dlg').classList.add('wide');
  const draw = () => {
    const f = $('#fc-filter').value.trim().toLowerCase();
    $('#fc').innerHTML = part('winning', win, f) + part('losing', lose, f);
  };
  $('#fc-filter').oninput = draw;
  draw();
  $('#fc-filter').focus();
  await done;
}

draggable(lists.mods, {
  // Dragging a selected row takes the whole selection; a separator takes its group.
  blockOf: m => block(selected.has(m.id) ? selIds() : [m.id]),
  drop: (ids, t, after) => {
    if (ids.includes(t.id)) return;
    const rest = restOf(ids);
    const at = !after ? rest.indexOf(t.id)
      : t.separator && collapsed.has(t.id) ? groupEnd(rest, t.id) : rest.indexOf(t.id) + 1;
    moveTo(ids, at);
  },
});

$('#rules-notice').onclick = async e => {
  if (!e.target.closest('#sort-mods')) return;
  const r = await act('Sorting by rules', () => call('sort_mods', { dry_run: true }), { reload: false });
  if (!r) return;
  const moved = r.moved.map(id => `${String(r.order.indexOf(id)).padStart(4)}  ${nameOf(id)}`).join('\n');
  const cycles = r.cycles.length
    ? `<p class="muted">Left out because they contradict each other: ${esc(r.cycles.map(([a, b]) => `${nameOf(a)} after ${nameOf(b)}`).join('; '))}.</p>` : '';
  const body = `<p>${plural(r.moved.length, 'mod')} change position (new position first):</p><pre>${esc(moved)}</pre>${cycles}`;
  if (await dialog({ title: 'Sort mods by rules', body, actions: [['cancel', 'Cancel'], ['apply', 'Apply order', 'primary']] }) === 'apply') {
    act('Sorting by rules', () => call('sort_mods', { dry_run: false }));
  }
};

$('#details').addEventListener('click', async e => {
  const t = e.target.closest('button, input');
  if (!t) return;
  const id = selIds()[0];
  if (t.dataset.goto) { if (tab !== 'mods') showTab('mods'); goTo(t.dataset.goto); }
  else if (t.dataset.plugin) {
    if (S.plugins.some(p => p.name.toLowerCase() === t.dataset.plugin.toLowerCase())) pickPlugin(S.plugins.find(p => p.name.toLowerCase() === t.dataset.plugin.toLowerCase()).name);
    else toast(`${t.dataset.plugin} isn’t installed.`, 'warn');
  } else if (t.dataset.unloot) {
    const old = lootRules?.rules.find(r => r.plugin.toLowerCase() === selPlugin.toLowerCase());
    setLootRule({ after: (old?.after ?? []).filter(a => a !== t.dataset.unloot) });
  } else if (t.dataset.unrule) {
    const key = t.dataset.key;
    setModRule(id, key, (S.rules.find(r => r.id === id)?.[key] ?? []).filter(a => a !== t.dataset.unrule));
  } else switch (t.dataset.act) {
    case 'enable':
      if (busy) return e.preventDefault();
      return setEnabled([id], t.checked);
    case 'on': return setEnabled(selIds(), true);
    case 'off': return setEnabled(selIds(), false);
    case 'move': return moveDialog();
    case 'uninstall': return removeSelected();
    case 'rename': return renameSep(id);
    case 'rename-mod': return renameMod(id);
    case 'reinstall': return reinstall(id);
    case 'nocolor': return act('Changing the colour', () => call('edit_separator', { id, color: '' }));
    case 'mod-nocolor': return act('Changing the colour', () => call('set_color', { id, color: '' }));
    case 'files': return fileConflicts(id);
    case 'select-kids':
      toggleGroup(id, true);
      selected = new Set(D.kids.get(id).map(m => m.id));
      return selectionChanged();
  }
});
$('#details').addEventListener('change', e => {
  const t = e.target, id = selIds()[0];
  if (t.id === 'loot-after' && t.value.trim()) {
    const old = lootRules?.rules.find(r => r.plugin.toLowerCase() === selPlugin.toLowerCase());
    const name = S.plugins.find(p => p.name.toLowerCase() === t.value.trim().toLowerCase())?.name ?? t.value.trim();
    return setLootRule({ after: [...(old?.after ?? []), name] });
  }
  if (t.id === 'loot-group') return setLootRule({ group: t.value.trim() || null });
  if (t.dataset.act === 'color') act('Changing the colour', () => call('edit_separator', { id, color: t.value }));
  if (t.dataset.act === 'mod-color') act('Changing the colour', () => call('set_color', { id, color: t.value }));
  if (t.dataset.act === 'lock') act(t.checked ? 'Locking the plugin' : 'Unlocking the plugin', () => call('lock_plugin', { name: selPlugin, lock: t.checked }));
  if (t.id === 'mod-note') act('Saving the notes', () => call('set_note', { id, note: t.value }));
  if (!t.classList.contains('rule-add') || !t.value.trim()) return;
  const other = idByName.get(t.value.trim()), key = t.dataset.key;
  if (!other) return toast(`No mod is named “${t.value.trim()}”.`, 'warn');
  setModRule(id, key, [...(S.rules.find(r => r.id === id)?.[key] ?? []), other]);
});

// ---- Plugins --------------------------------------------------------------------------------

$('#plugins tbody').addEventListener('click', e => {
  const tr = e.target.closest('tr[data-name]');
  if (!tr) return;
  if (!e.target.matches('input[type=checkbox]')) {
    if (!e.target.closest('.grip')) pickPlugin(tr.dataset.name === selPlugin ? null : tr.dataset.name);
    return;
  }
  if (busy) return e.preventDefault();
  act('Changing the plugin', () => call('enable_plugin', { name: tr.dataset.name, enabled: e.target.checked }));
});
draggable(lists.plugins, {
  blockOf: p => [p.name],
  drop: ([name], t, after) => {
    const order = S.plugins.filter(p => !p.implicit).map(p => p.name);
    const from = order.indexOf(name), ti = order.indexOf(t.name);
    const slot = ti < 0 ? 0 : ti + (after ? 1 : 0);
    const to = slot > from ? slot - 1 : slot;
    if (to === from) return;
    order.splice(to, 0, ...order.splice(from, 1));
    act('Moving the plugin', () => call('plugin_order', { names: order }));
  },
});

$('#loot').onclick = async () => {
  toast('Sorting with LOOT. The first sort downloads LOOT’s masterlist.');
  const r = await act('Sorting with LOOT', () => call('sort_plugins', { dry_run: true }), { reload: false });
  if (!r) return;
  const warnings = r.warnings.length ? `<details><summary>${plural(r.warnings.length, 'warning')}</summary><pre>${esc(r.warnings.join('\n'))}</pre></details>` : '';
  if (!r.moved.length) {
    return dialog({ title: 'Sort with LOOT', body: `<p>The load order already matches LOOT.</p>${warnings}`, actions: [['cancel', 'Close']] });
  }
  const moved = r.moved.map(m => `${String(m.from).padStart(4)} → ${String(m.to).padEnd(4)} ${m.name}`).join('\n');
  const body = `<p>${plural(r.moved.length, 'plugin')} move:</p><pre>${esc(moved)}</pre>${warnings}`;
  if (await dialog({ title: 'Sort with LOOT', body, actions: [['cancel', 'Cancel'], ['apply', 'Apply order', 'primary']] }) === 'apply') {
    act('Saving the LOOT order', () => call('sort_plugins', { dry_run: false }));
  }
};

// ---- Drag to reorder (pointer events: HTML drag and drop is off so files can be dropped) ----

/** Rows come and go while the list scrolls, so the drag lives in L.drag and the row templates
 *  draw it. drop(ids, targetItem, after) runs on release over a row. */
function draggable(L, { blockOf, drop }) {
  L.table.tBodies[0].addEventListener('pointerdown', e => {
    const tr = e.target.closest('.grip')?.closest('tr[data-i]');
    if (!tr || busy || e.button !== 0) return;
    e.preventDefault();
    const sc = scroller(L);
    let x = e.clientX, y = e.clientY;
    L.drag = { ids: new Set(blockOf(L.items[+tr.dataset.i])), mark: -1, after: false };
    document.body.classList.add('dragging');
    paint(L);
    const aim = () => {
      const over = document.elementFromPoint(x, y)?.closest('tr[data-i]');
      let mark = -1, after = false;
      if (over && L.table.contains(over)) {
        const r = over.getBoundingClientRect();
        mark = +over.dataset.i;
        after = y > r.top + r.height / 2;
      }
      if (mark !== L.drag.mark || after !== L.drag.after) {
        Object.assign(L.drag, { mark, after });
        paint(L);
      }
    };
    const timer = setInterval(() => {
      const r = sc.getBoundingClientRect();
      if (y < r.top + L.table.tHead.offsetHeight + 30) sc.scrollTop -= 14;
      else if (y > r.bottom - 30) sc.scrollTop += 14;
      else return;
      paint(L);
      aim();
    }, 16);
    const move = ev => { x = ev.clientX; y = ev.clientY; aim(); };
    const up = () => {
      clearInterval(timer);
      removeEventListener('pointermove', move);
      document.body.classList.remove('dragging');
      const { ids, mark, after } = L.drag;
      L.drag = null;
      paint(L);
      if (mark >= 0) drop([...ids], L.items[mark], after);
    };
    addEventListener('pointermove', move);
    addEventListener('pointerup', up, { once: true });
  });
}

// ---- Deploy, play ---------------------------------------------------------------------------

$('#deploy').onclick = async () => {
  await refreshPlan();
  if (plan.error) {
    return dialog({ title: 'Deploy is blocked', body: `<p>${esc(plan.error)}</p>`, actions: [['cancel', 'Close']] });
  }
  const n = pending(plan);
  const part = (label, files, note) => files.length
    ? `<details><summary><b>${files.length}</b> ${label}</summary>${note ? `<p class="muted small">${note}</p>` : ''}<pre>${esc(files.join('\n'))}</pre></details>` : '';
  const body = n
    ? part('to link', plan.link)
      + part('to remove', plan.remove)
      + part('original files to back up', plan.backup, 'Restored when the mod goes away, or on purge.')
      + part('changed outside rmm', plan.external_changes, 'Copied to overwrite/, where they keep winning over every mod.')
      + part('written by the game during play', plan.played ?? [], 'Moved to overwrite/ and linked back.')
      + `<p class="muted small" style="margin-top:10px">${plural(plan.unchanged, 'file')} already in place.</p>`
    : `<p>The game folder already matches this profile (${plural(plan.unchanged, 'file')}).</p>`;
  const r = await dialog({ title: 'Deploy to the game folder', body, actions: [['purge', 'Purge…'], ...(n ? [['cancel', 'Cancel'], ['apply', 'Deploy', 'primary']] : [['cancel', 'Close']])] });
  if (r === 'purge') return purge();
  if (r !== 'apply') return;
  const res = await act('Deploying', () => call('deploy'));
  if (res) toast(`Deployed: ${res.link.length} linked, ${res.remove.length} removed.`);
};

/** Take every deployed file out of the game folder and put the originals back. */
async function purge() {
  const p = await act('Checking the game folder', () => call('purge', { dry_run: true }), { reload: false });
  if (!p) return;
  const files = p.remove.length + (p.external_changes?.length ?? 0);
  if (!files) return dialog({ title: 'Purge', body: '<p>Nothing rmm deployed is in the game folder.</p>', actions: [['cancel', 'Close']] });
  const part = (label, list) => list?.length ? `<details><summary><b>${list.length}</b> ${label}</summary><pre>${esc(list.join('\n'))}</pre></details>` : '';
  const body = `<p>The game folder goes back to how it was before rmm: deployed files are removed and backed-up originals restored. Your mods and profiles stay; Deploy puts everything back.</p>`
    + part('to remove', p.remove) + part('changed outside rmm, copied to overwrite first', p.external_changes) + part('originals to restore', p.backup);
  if (await dialog({ title: 'Purge the game folder', body, actions: [['cancel', 'Cancel'], ['apply', 'Purge', 'danger']] }) !== 'apply') return;
  const r = await act('Purging', () => call('purge', { dry_run: false }));
  if (r) toast(`Purged: ${plural(r.remove.length, 'file')} removed.`);
}

$('#play').onclick = async () => {
  const r = await act('Starting the game', () => call('launch'));
  if (!r) return;
  const what = [`game ${r.game_version}`, r.script_extender].filter(Boolean).join(', ');
  toast(`Started ${basename(r.exe)} (${what}).${r.warnings.length ? '\n' + r.warnings.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
};

// ---- Profiles -------------------------------------------------------------------------------

$('#profile-btn').onclick = e => {
  e.stopPropagation();
  const pop = $('#profile-pop');
  if (!pop.hidden) { pop.hidden = true; return; }
  const cur = S.info.profile;
  pop.innerHTML = S.profiles.map(p => `<button class="item" data-switch="${esc(p)}"><span class="check">${p === cur ? '✓' : ''}</span>${esc(p)}${p === cur ? '' : `<span class="x" data-delete="${esc(p)}" title="Delete this profile">×</span>`}</button>`).join('')
    + '<hr><button class="item" data-new="copy"><span class="check"></span>Copy this profile…</button>'
    + '<button class="item" data-new="empty"><span class="check"></span>New empty profile…</button>'
    + '<button class="item" data-rename="1"><span class="check"></span>Rename this profile…</button>'
    + `<hr><button class="item" data-opt="local_inis" title="Deploy puts this profile's copies of the game's INI files in My Games; what the game changes there comes back to the profile. Purge puts yours back."><span class="check">${S.options?.local_inis ? '✓' : ''}</span>Own INI files</button>`
    + `<button class="item" data-opt="local_saves" title="Deploy points the game's saves at Saves\\${esc(cur)}\\. Purge puts your save folder back."><span class="check">${S.options?.local_saves ? '✓' : ''}</span>Own saves</button>`
    + '<hr><button class="item" data-io="mo2"><span class="check"></span>Import from MO2…</button>'
    + '<button class="item" data-io="vortex"><span class="check"></span>Import from Vortex…</button>'
    + '<button class="item" data-io="export"><span class="check"></span>Export lockfile</button>'
    + '<button class="item" data-io="apply"><span class="check"></span>Apply a lockfile…</button>';
  pop.hidden = false;
};
document.addEventListener('click', e => { if (!e.target.closest('.menu')) $('#profile-pop').hidden = true; });

$('#profile-pop').onclick = async e => {
  const t = e.target.closest('[data-delete],[data-switch],[data-new],[data-rename],[data-io],[data-opt]');
  if (!t) return;
  $('#profile-pop').hidden = true;
  const cur = S.info.profile;
  if (t.dataset.io) return importExport(t.dataset.io);
  if (t.dataset.opt) {
    const on = !S.options?.[t.dataset.opt];
    if (await act('Changing the profile', () => call('set_profile_options', { [t.dataset.opt]: on }))) {
      toast(`${t.dataset.opt === 'local_inis' ? 'Own INI files' : 'Own saves'} ${on ? 'on' : 'off'} for ${cur}. Deploy or Play applies it.`);
    }
    return;
  }
  if (t.dataset.delete) {
    const name = t.dataset.delete;
    if (await confirmBox(`Delete profile “${name}”?`, 'It moves to profiles/.deleted/ in the instance folder.', 'Delete')) {
      act('Deleting the profile', () => call('delete_profile', { name }));
    }
  } else if (t.dataset.switch && t.dataset.switch !== cur) {
    const name = t.dataset.switch;
    if (await act('Switching profile', () => call('switch_profile', { name }))) {
      toast(`Switched to ${name}. Deploy or Play applies it to the game folder.`);
    }
  } else if (t.dataset.new) {
    const copy = t.dataset.new === 'copy';
    const name = await ask(copy ? 'Copy this profile' : 'New empty profile',
      copy ? `A copy of “${cur}”: same mods, order and plugins.` : 'Every installed mod, disabled.', copy ? `${cur} copy` : '');
    if (name && await act('Creating the profile', async () => { await call('create_profile', { name, copy }); await call('switch_profile', { name }); })) {
      toast(`Switched to ${name}.`);
    }
  } else if (t.dataset.rename) {
    const to = await ask('Rename this profile', `New name for “${cur}”.`, cur);
    if (to && to !== cur) act('Renaming the profile', () => call('rename_profile', { from: cur, to }));
  }
};

// ---- Nexus ----------------------------------------------------------------------------------

/** Ask for the API key (the user pastes it; it is checked with Nexus and kept in their config
 *  folder, never in the instance). Resolves to true once a key works. */
async function nexusLogin(why, sso) {
  const body = `<p>${esc(why)}</p><p class="muted small">${sso ? 'Sign in on nexusmods.com and approve rmm there, or paste' : 'Copy'} your personal API key from nexusmods.com: Site preferences, API Keys. rmm keeps it in your user folder, never in the instance or a lockfile.</p>
    <input id="nexus-key" type="password" autocomplete="off" spellcheck="false" placeholder="API key" aria-label="Nexus API key">`;
  const done = dialog({ title: 'Nexus Mods', body, actions: [['cancel', 'Cancel'], ...(sso ? [['sso', 'Sign in on nexusmods.com']] : []), ['ok', 'Save key', 'primary']] });
  $('#nexus-key').focus();
  $('#nexus-key').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); $('#dlg').close('ok'); } };
  const how = await done;
  if (how !== 'ok' && how !== 'sso') return false;
  const key = $('#nexus-key').value;
  $('#nexus-key').value = '';
  const who = how === 'sso'
    ? await act('Waiting for you to approve rmm on nexusmods.com', () => call('nexus_sso'), { reload: false })
    : await act('Checking the key', () => call('nexus_set_key', { key }), { reload: false });
  if (who && who !== true) toast(`Signed in to Nexus as ${who.name}${who.is_premium ? ' (Premium)' : ''}.`);
  return !!who;
}

/** True once a Nexus API key is saved, asking for one if needed. */
async function nexusReady(why) {
  const st = await call('nexus_status').catch(() => ({ key: false }));
  return st.key || nexusLogin(why, st.sso);
}

$('#nxm').onclick = async () => {
  const link = await ask('Install from Nexus Mods', 'Paste the nxm:// link of a file’s “Mod manager download” button (right-click it, copy the link). Premium accounts can also give <mod id>/<file id>.', '');
  if (!link || !await nexusReady('Downloads from Nexus Mods need your API key.')) return;
  const f = await act('Downloading from Nexus', () => call('nexus_fetch', { link }), { reload: false });
  if (f && f !== true) install([f.path], slotAfter(selIds().length === 1 ? selIds()[0] : null), f);
};

$('#updates').onclick = async () => {
  if (!await nexusReady('Update checks ask Nexus Mods about your mods, which needs your API key.')) return;
  const r = await act('Checking for updates', () => call('check_updates'), { reload: false });
  if (!Array.isArray(r)) return;
  updates = new Map(r.map(u => [u.id, u]));
  paint(lists.mods);
  if (!r.length) return toast('Every mod with Nexus ids is up to date.');
  const rows = r.map(u => `<tr><td>${esc(nameOf(u.id))}</td><td>${esc(u.installed ?? '?')} → <b>${esc(u.latest ?? u.file_name)}</b></td><td><span class="small">${esc(u.url)}</span></td></tr>`).join('');
  dialog({ title: `${plural(r.length, 'update')} on Nexus`, body: `<table class="fc">${rows}</table><p class="muted small">Download a new file with its “Mod manager download” button (once nxm links open rmm), or use From Nexus…, or download it and drop it on the mod’s row.</p>`, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
};

// ---- Add-ons ----------------------------------------------------------------------------------

const CAPS = { read_instance: 'read your setup', modify_profile: 'change mod and plugin order and what is enabled', mod_files: 'read mod files and write to overwrite/', run_tools: 'run your tools', network: 'reach the internet' };
const HOOK_TEXT = { after_install: 'after installs', before_deploy: 'before deploys (and can stop them)', after_deploy: 'after deploys', after_tool_run: 'after tool runs', health_check: 'in health checks' };
const hookText = a => a.hooks.map(h => HOOK_TEXT[h] ?? h).join(', ');
const capText = a => a.capabilities.length ? a.capabilities.map(c => CAPS[c] ?? c).join(', ') + (a.hosts.length ? ` (${a.hosts.join(', ')})` : '') : 'nothing beyond its own work';

/** Show what an add-on function returned: {text}, {columns, rows}, or plain JSON. */
function addonOutput(v) {
  if (v && typeof v.text === 'string') return `<p style="white-space:pre-wrap">${esc(v.text)}</p>`;
  if (v && Array.isArray(v.columns) && Array.isArray(v.rows)) {
    return `<table class="fc"><tr>${v.columns.map(c => `<th>${esc(c)}</th>`).join('')}</tr>${v.rows.map(r => `<tr>${r.map(c => `<td>${esc(String(c))}</td>`).join('')}</tr>`).join('')}</table>`;
  }
  return `<pre class="sel">${esc(JSON.stringify(v, null, 2))}</pre>`;
}

$('#addons').onclick = async () => {
  const list = await act('Reading add-ons', () => call('addons'), { reload: false });
  if (!Array.isArray(list)) return;
  const rows = list.map(a => `<li><b>${esc(a.name)}</b> ${esc(a.version)}${a.description ? ` <span class="muted">${esc(a.description)}</span>` : ''}
      <div class="small muted">May ${esc(capText(a))}.${a.hooks.length ? ` Runs ${esc(hookText(a))}.` : ''}</div>
      <div>${a.panels.map(p => `<button data-addon="${esc(a.name)}" data-fn="${esc(p.function)}" data-panel="${esc(p.title)}">${esc(p.title)}</button>`).join('')}${a.tools.map(t => `<button data-addon="${esc(a.name)}" data-fn="${esc(t.name)}" data-input="${t.input ? 1 : ''}" title="${esc(t.description)}">${esc(t.name)}</button>`).join('')}
      <button data-remove="${esc(a.name)}" class="danger-text">Remove</button></div></li>`).join('');
  const body = list.length ? `<ul class="addons">${rows}</ul>` : '<p>No add-ons yet. Add-ons are WebAssembly plugins (Extism) that run sandboxed and can only do what they ask for when you install them.</p>';
  const done = dialog({ title: 'Add-ons', body, actions: [['install', 'Install add-on…'], ['cancel', 'Close', 'primary']] });
  $('#dlg').classList.add('wide');
  $('#dlg .body').onclick = async e => {
    const b = e.target.closest('button[data-addon],button[data-remove]');
    if (!b) return;
    e.preventDefault();
    $('#dlg').close('cancel');
    if (b.dataset.remove) {
      if (await confirmBox(`Remove the add-on “${b.dataset.remove}”?`, 'Its folder is deleted from the instance.', 'Remove')) act('Removing the add-on', () => call('addon_remove', { name: b.dataset.remove }), { reload: false });
      return;
    }
    let input = {};
    if (b.dataset.input) {
      const text = await ask(b.textContent, `JSON input for ${b.dataset.fn}.`, '{}');
      if (text === null) return;
      try { input = JSON.parse(text); } catch { return toast('That isn’t JSON.', 'error'); }
    }
    const r = await act(`Running ${b.dataset.addon}`, () => call('addon_call', { name: b.dataset.addon, function: b.dataset.fn, input }));
    if (r !== null && r !== undefined && r !== true) {
      dialog({ title: b.dataset.panel ?? `${b.dataset.addon}: ${b.dataset.fn}`, body: addonOutput(r), actions: [['cancel', 'Close']] });
      $('#dlg').classList.add('wide');
    }
  };
  if (await done !== 'install') return;
  const path = await T.dialog.open({ directory: true, title: 'The add-on folder (with addon.toml and addon.wasm)' });
  if (!path) return;
  const a = await act('Checking the add-on', () => call('addon_install', { path, dry_run: true }), { reload: false });
  if (!a || a === true) return;
  const ok = await confirmBox(`Install ${a.name} ${a.version}?`, `It may ${capText(a)}.${a.hooks.length ? ` It runs ${hookText(a)}.` : ''} Only install add-ons from people you trust.`, 'Install');
  if (ok && await act('Installing the add-on', () => call('addon_install', { path, dry_run: false }), { reload: false })) toast(`Installed ${a.name}.`);
};

// ---- AI access (MCP over HTTP) ----------------------------------------------------------------

let mcpOn = null;   // { url, token } while the window serves MCP
let mcpRoot = null; // the instance it serves

/** Start or stop the server; quiet unless it fails. */
async function setMcp(on) {
  try {
    const r = await T.core.invoke('mcp', { on });
    mcpOn = r.on ? r : null;
    mcpRoot = mcpOn ? S.info.root : null;
  } catch (e) {
    mcpOn = null;
    toast(`AI access: ${e}`, 'error');
  }
  prefs.mcp = !!mcpOn;
  savePrefs();
  $('#ai-access').setAttribute('aria-pressed', !!mcpOn);
}

$('#ai-access').onclick = async () => {
  const conf = m => esc(JSON.stringify({ mcpServers: { rmm: { type: 'http', url: m.url, headers: { Authorization: `Bearer ${m.token}` } } } }, null, 2));
  const body = () => mcpOn
    ? `<p>AI clients on this computer can use rmm's tools on this instance while the window is open. Their changes show up here and can be undone.</p>
       <p class="small">Claude Code:</p><pre class="sel">claude mcp add --transport http rmm ${esc(mcpOn.url)} --header "Authorization: Bearer ${esc(mcpOn.token)}"</pre>
       <p class="small">Other clients (JSON settings):</p><pre class="sel">${conf(mcpOn)}</pre>
       <p class="muted small">The token is kept in your user folder. Anyone with it can change this instance, so keep it to yourself.</p>`
    : `<p>Serve rmm's tools to AI clients on this computer, over HTTP on 127.0.0.1 with a private token. Clients that start programs themselves can use <code>rmm mcp</code> instead and need nothing here.</p>`;
  const r = await dialog({ title: 'AI access (MCP)', body: body(), actions: mcpOn ? [['cancel', 'Close'], ['off', 'Turn off']] : [['cancel', 'Cancel'], ['on', 'Turn on', 'primary']] });
  if (r === 'on' || r === 'off') {
    await setMcp(r === 'on');
    if (mcpOn) $('#ai-access').click();
  }
};

// ---- Import, lockfiles ------------------------------------------------------------------------

async function importExport(what) {
  const part = (label, list) => list?.length ? `<details><summary><b>${list.length.toLocaleString()}</b> ${label}</summary><pre>${esc(list.join('\n'))}</pre></details>` : '';
  if (what === 'export') {
    const path = await act('Writing the lockfile', () => call('export_lockfile'), { reload: false });
    if (path && path !== true) toast(`Wrote ${path}`);
    return;
  }
  if (what === 'mo2' || what === 'vortex') {
    const src = await call('import_sources').catch(() => ({ mo2: [], vortex: [] }));
    // Vortex's live database comes first when it has one; a backup file is the other way in.
    const live = what === 'vortex' && src.vortex.find(p => /state\.v2$/.test(p));
    let path = null;
    if (live) {
      const pick = await dialog({ title: 'Import from Vortex', body: '<p>Read Vortex’s current setup? Close Vortex first so everything it knows is saved.</p><p class="muted small">rmm reads a copy and never changes Vortex’s files.</p>', actions: [['cancel', 'Cancel'], ['file', 'A backup file…'], ['ok', 'Current setup', 'primary']] });
      if (pick === 'ok') path = live;
      else if (pick !== 'file') return;
    }
    path ??= what === 'mo2'
      ? await T.dialog.open({ directory: true, title: 'The MO2 instance folder (with ModOrganizer.ini)', defaultPath: src.mo2[0] })
      : await T.dialog.open({ title: 'A Vortex state backup', filters: [{ name: 'Vortex backup', extensions: ['json'] }], defaultPath: src.vortex.find(p => p.endsWith('.json')) });
    if (!path) return;
    const args = what === 'mo2' ? { dir: path } : { path };
    const run = dry_run => call(what === 'mo2' ? 'import_mo2' : 'import_vortex', { ...args, dry_run });
    const r = await act('Reading the setup', () => run(true), { reload: false });
    if (!r || r === true) return;
    const body = `<p>${what === 'mo2' ? 'MO2' : 'Vortex'} profile <b>${esc(r.profile)}</b>: ${plural(r.mods.length, 'mod')} to bring in, ${plural(r.separators, 'separator')}, ${plural(r.plugins, 'plugin')}${what === 'vortex' ? `, ${plural(r.rules, 'mod rule')}` : ''}.</p>
      <p class="muted small">Files are hard-linked when they’re on the same drive (no extra space), copied otherwise. The list becomes this profile’s order in one step that Undo reverts; mods already here stay as they are.${what === 'vortex' ? ' Purge Vortex’s deployment before deploying with rmm.' : ''}</p>`
      + part('to bring in', r.mods) + part('already installed, kept as they are', r.existing) + part('warnings', r.warnings);
    if (await dialog({ title: `Import from ${what === 'mo2' ? 'MO2' : 'Vortex'}`, body, actions: [['cancel', 'Cancel'], ['ok', 'Import', 'primary']] }) !== 'ok') return;
    const done = await act('Importing', () => run(false));
    if (done && done !== true) toast(`Imported ${plural(done.mods.length, 'mod')} (${done.linked.toLocaleString()} files linked, ${done.copied.toLocaleString()} copied).${done.warnings.length ? '\n' + done.warnings.join('\n') : ''}`, done.warnings.length ? 'warn' : 'info');
    return;
  }
  // Apply a lockfile.
  const path = await T.dialog.open({ title: 'A lockfile', filters: [{ name: 'rmm lockfile', extensions: ['toml'] }] });
  if (!path) return;
  const dl = await T.dialog.open({ directory: true, title: 'Where the mod archives are (cancel to skip installing)' });
  const downloads = dl ? [dl] : [];
  const run = dry_run => call('apply_lockfile', { path, downloads, dry_run });
  const r = await act('Reading the lockfile', () => run(true), { reload: false });
  if (!r || r === true) return;
  const missing = r.missing.map(m => `${m.name ?? m.id}${m.tool ? ` (made by the tool “${m.tool}”)` : m.archive ? ` (${m.archive})` : ''}${m.url ? `\n    ${m.url}` : ''}`);
  const body = `<p>${r.ok.toLocaleString()} mods already match. ${plural(r.install.length, 'mod')} to install from the archives.</p>`
    + part('to install', r.install) + part('missing: download these, then apply again', missing)
    + part('installed with other files (another version or other choices), left as they are', r.differs)
    + part('yours, not in the lockfile: kept after the mod they follow', r.kept) + part('warnings', r.warnings)
    + '<p class="muted small">Then the order, enabled state, colours, plugin order and mod rules follow the lockfile, in one step that Undo reverts.</p>';
  if (await dialog({ title: 'Apply a lockfile', body, actions: [['cancel', 'Cancel'], ['ok', 'Apply', 'primary']] }) !== 'ok') return;
  const done = await act('Applying the lockfile', () => run(false));
  if (done && done !== true) toast(`Applied the lockfile: ${plural(done.install.length, 'mod')} installed${done.missing.length ? `, ${done.missing.length} still missing` : ''}.`, done.missing.length || done.warnings.length ? 'warn' : 'info');
}

// ---- Tools ----------------------------------------------------------------------------------

const BUILTIN = new Set(['data', 'game', 'plugins', 'ini', 'ini_dir', 'output', 'state']);

async function loadTools() {
  try { toolsData = await call('tools'); } catch (e) { toolsData = { tools: [], presets: [] }; toast(`Couldn’t read the tools.\n${e}`, 'error'); }
  renderTools();
}

function renderTools() {
  const t = toolsData;
  $('#n-tools').textContent = t.tools.length || '';
  $('#tools tbody').innerHTML = t.tools.map(({ id, tool, last }) => {
    const out = tool.output_mod ?? `${tool.name} Output`;
    const how = !last ? '<span class="muted">Never run</span>'
      : (last.mod_id ? `${plural(last.files, 'file')} captured` : 'Captured nothing')
        + (last.timed_out ? ' · timed out' : last.exit_code ? ` · exit code ${last.exit_code}` : '')
        + (last.warnings?.length ? ` · <span title="${esc(last.warnings.join('\n'))}">⚠ ${last.warnings.length}</span>` : '');
    return `<tr data-tool="${esc(id)}">
      <td class="name"><b>${esc(tool.name)}</b> <span class="muted small">${esc(id)}</span><div class="muted small" title="${esc(tool.exe)}">${esc(tool.exe)}</div></td>
      <td>${esc(out)}</td><td>${how}</td>
      <td class="run-col"><button data-run class="primary">Run…</button> <button data-remove class="link">Remove</button></td></tr>`;
  }).join('');
  $('#tools-empty').hidden = t.tools.length > 0;
}

$('#add-tool').onclick = async () => {
  if (!toolsData) await loadTools();
  const body = `
    <label class="field">Preset<select id="t-preset"><option value="">Another program</option>${toolsData.presets.map(p => `<option>${esc(p)}</option>`).join('')}</select></label>
    <label class="field">Program<span class="row" style="margin:0"><input id="t-exe" spellcheck="false" placeholder="Path to the .exe" style="flex:1"><button type="button" id="t-browse">Browse…</button></span></label>
    <label class="field">Id<input id="t-id" spellcheck="false" placeholder="A short name, such as xedit"></label>
    <label class="field">Name<input id="t-name" spellcheck="false" placeholder="Shown in the list (optional)"></label>
    <label class="field" id="t-args-f">Arguments, one per line<textarea id="t-args" rows="4" spellcheck="false" placeholder="-o:{output}"></textarea></label>
    <p class="muted small">Placeholders: {data} {game} {plugins} {ini} {ini_dir} {output} {state}; Run asks for any other name in braces. A preset fills in its own arguments.</p>`;
  const d = $('#dlg');
  const wait = dialog({ title: 'Add a tool', body, actions: [['cancel', 'Cancel'], ['ok', 'Add', 'primary']] });
  const preset = $('#t-preset', d), id = $('#t-id', d);
  preset.onchange = () => { $('#t-args-f', d).hidden = !!preset.value; if (preset.value && !id.value) id.value = preset.value; };
  $('#t-browse', d).onclick = async () => {
    const exe = await T.dialog.open({ title: 'Choose the program' });
    if (exe) { $('#t-exe', d).value = exe; if (!id.value) id.value = basename(exe).replace(/\.[^.]+$/, '').toLowerCase().replace(/[^a-z0-9-]+/g, '-'); }
  };
  if (await wait !== 'ok') return;
  const spec = { exe: $('#t-exe', d).value.trim(), preset: preset.value || null, name: $('#t-name', d).value.trim() || null };
  const args = $('#t-args', d).value.split('\n').map(s => s.trim()).filter(Boolean);
  if (!preset.value) spec.args = args;
  const tid = id.value.trim();
  if (!tid || !spec.exe) return toast('A tool needs an id and a program.', 'warn');
  if (await act('Adding the tool', () => call('add_tool', { id: tid, spec }), { reload: false })) loadTools();
};

$('#tools tbody').addEventListener('click', async e => {
  const id = e.target.closest('tr[data-tool]')?.dataset.tool;
  if (!id) return;
  if (e.target.closest('[data-remove]')) {
    if (await confirmBox(`Remove ${id}?`, 'Its settings leave tools.toml. Its generated mod and its state folder stay.', 'Remove')) {
      if (await act('Removing the tool', () => call('remove_tool', { id }), { reload: false })) loadTools();
    }
  } else if (e.target.closest('[data-run]')) runTool(id);
});

/** Ask for the tool's values, preview the run, then run it and show what it captured. */
async function runTool(id) {
  const { tool } = toolsData.tools.find(t => t.id === id);
  const named = new Set(Object.keys(tool.vars ?? {}));
  for (const s of [...tool.args, tool.output_mod ?? '', ...(tool.rewrites ?? [])]) for (const m of s.matchAll(/\{(\w+)\}/g)) if (!BUILTIN.has(m[1])) named.add(m[1]);
  let vars = {};
  if (named.size) {
    const body = [...named].map(n => `<label class="field">${esc(n)}<input data-var="${esc(n)}" value="${esc(tool.vars?.[n] ?? '')}" spellcheck="false"${n === 'plugin' ? ' list="plugin-list"' : ''}></label>`).join('')
      + `<datalist id="plugin-list">${S.plugins.map(p => `<option value="${esc(p.name)}">`).join('')}</datalist>`;
    if (await dialog({ title: `Run ${tool.name}`, body, actions: [['cancel', 'Cancel'], ['ok', 'Next', 'primary']] }) !== 'ok') return;
    for (const el of $$('[data-var]', $('#dlg'))) if (el.value.trim()) vars[el.dataset.var] = el.value.trim();
  }
  const plan = await act(`Preparing ${tool.name}`, () => call('run_tool', { id, vars, dry_run: true }), { reload: false });
  if (!plan) return;
  const list = (label, l) => l?.length ? `<details><summary><b>${l.length}</b> ${label}</summary><pre>${esc(l.join('\n'))}</pre></details>` : '';
  const body = `<div class="report"><p>rmm deploys, runs the tool, and captures what it writes into <b>${esc(plan.output_mod)}</b>${plan.previous ? `, replacing ${esc(nameOf(plan.previous))}` : ''}. Then it deploys again. Undo brings the previous output back.</p>
    <pre>${esc(plan.command.join(' '))}</pre>
    ${list('deployed files swapped for copies while it runs', plan.swapped)}
    ${plan.deploy ? `<p class="muted small">The first deploy: ${plural(plan.deploy.link.length, 'file')} to link, ${plural(plan.deploy.remove.length, 'file')} to remove.</p>` : ''}
    ${plan.warnings.length ? `<p>⚠ ${esc(plan.warnings.join(' · '))}</p>` : ''}
    <p class="muted small">The window waits until the tool closes.</p></div>`;
  if (await dialog({ title: `Run ${tool.name}`, body, actions: [['cancel', 'Cancel'], ['run', 'Run', 'primary']] }) !== 'run') return;
  const r = await act(`Running ${tool.name}`, () => call('run_tool', { id, vars, dry_run: false }));
  if (!r) return;
  loadTools();
  const out = `<div class="report"><p>${r.mod_id ? `Captured ${plural(r.files, 'file')} into <b>${esc(nameOf(r.mod_id))}</b>.` : 'The tool wrote nothing rmm could capture.'}
    ${r.timed_out ? ' It timed out.' : r.exit_code ? ` It exited with code ${r.exit_code}.` : ''}</p>
    ${list('new', r.new)}${list('changed', r.changed)}${list('deleted', r.deleted)}
    ${r.modified_in_place.length ? `<p>⚠ It rewrote mod files through their links: ${esc(r.modified_in_place.join(', '))}. Reinstall those mods.</p>` : ''}
    ${r.warnings.length ? `<p>⚠ ${esc(r.warnings.join(' · '))}</p>` : ''}
    ${r.log_tail ? `<details><summary>Log</summary><pre>${esc(r.log_tail)}</pre></details>` : ''}</div>`;
  dialog({ title: `${tool.name} finished`, body: out, actions: [['cancel', 'Close']] });
}

// ---- Install --------------------------------------------------------------------------------

$('#install').onclick = async () => {
  const picked = await T.dialog.open({ multiple: true, title: 'Install mods', filters: [{ name: 'Mod archives', extensions: ['zip', '7z'] }] });
  if (picked) install([].concat(picked), slotAfter(selIds().length === 1 ? selIds()[0] : null));
};

/** Where new mods go for a row: just after a mod, or at the end of a separator's group; null
 *  (no row) lets the core place them by the mod rules, at the end otherwise. */
function slotAfter(id) {
  if (!id || !byId.has(id)) return null;
  const order = S.mods.map(m => m.id);
  return isSep(id) ? groupEnd(order, id) : order.indexOf(id) + 1;
}

/** `nx`: what nexus_fetch found about a downloaded file, recorded on the new mod. */
async function install(paths, at = null, nx = null) {
  for (const path of paths) {
    // Unpacks the archive once; the install below reuses it.
    const walk = await act(`Reading ${basename(path)}`, () => call('fomod', { path }), { reload: false });
    if (!walk) continue;
    let choices, name = nx?.name ?? undefined;
    if (walk !== true) { // it has a FOMOD installer, which names the mod better than most file names
      choices = walk.steps.length ? await fomodWizard(path, walk) : walk.choices;
      if (!choices) continue;
      name = walk.module;
    }
    const r = await act(`Installing ${basename(path)}`, () => call('install', { path, choices, name, priority: at, nexus: nx?.nexus }));
    if (!r) continue;
    if (at !== null) at++;
    const notes = [r.plugins.length ? `Plugins: ${r.plugins.join(', ')}` : '', ...r.warnings].filter(Boolean);
    toast(`Installed ${r.name} (${plural(r.files, 'file')}).${notes.length ? '\n' + notes.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
  }
}

T.webview.getCurrentWebview().onDragDropEvent(e => {
  const type = e.payload.type;
  if (type === 'enter' || type === 'over') {
    $('#dropzone').hidden = !S || busy;
    // Over a mod row: install right after it (a separator: at the end of its group).
    const pos = e.payload.position, r = window.devicePixelRatio || 1;
    const tr = pos && tab === 'mods' ? document.elementFromPoint(pos.x / r, pos.y / r)?.closest('#mods tr[data-id]') : null;
    const id = tr?.dataset.id ?? null;
    if (id !== (dropAt?.id ?? null)) {
      dropAt = id ? { id, priority: slotAfter(id) } : null;
      $('#drop-text').textContent = !id ? 'Drop to install' : isSep(id) ? `Drop to install into ${nameOf(id)}` : `Drop to install after ${nameOf(id)}`;
      paint(lists.mods);
    }
  } else {
    const at = dropAt?.priority ?? null;
    $('#dropzone').hidden = true;
    dropAt = null;
    paint(lists.mods);
    if (type === 'drop' && S && e.payload.paths?.length) install(e.payload.paths, at);
  }
});

// ---- FOMOD installer ------------------------------------------------------------------------

const RULE = {
  SelectExactlyOne: 'Choose one',
  SelectAtMostOne: 'Choose one or none',
  SelectAtLeastOne: 'Choose at least one',
  SelectAny: 'Choose any',
  SelectAll: 'All included',
};
const TAG = {
  Required: ['Required', 'plain'],
  Recommended: ['Recommended', 'win'],
  NotUsable: ['Can’t be used', 'bad'],
  CouldBeUsable: ['Might not work', 'lose'],
};

/** Walk a FOMOD installer one step at a time. After every change the core plays the whole
 *  installer again, so later steps, defaults and option types follow the choices. Resolves to
 *  the choices to install with, or null when cancelled. */
function fomodWizard(path, first) {
  const d = $('#dlg');
  const images = new Map(); // image path -> data: URL, or null when it can't be read
  let w = first, at = 0, working = false, shown = null;

  const radio = g => g.type === 'SelectExactlyOne' || g.type === 'SelectAtMostOne';
  const option = (gi, oi) => {
    const g = w.steps[at].groups[gi];
    const tag = TAG[g.options[oi].type];
    const o = g.options[oi];
    const locked = g.type === 'SelectAll' || o.type === 'Required' || o.type === 'NotUsable';
    return `<label class="opt${o.type === 'NotUsable' ? ' off' : ''}" data-g="${gi}" data-o="${oi}">
      <input type="${radio(g) ? 'radio' : 'checkbox'}" name="g${gi}" ${o.selected ? 'checked' : ''} ${locked ? 'disabled' : ''}>
      <span>${esc(o.name)}</span>${tag ? `<span class="pill ${tag[1]}">${tag[0]}</span>` : ''}</label>`;
  };
  const group = (g, gi) => `<fieldset><legend>${esc(g.name)}<span class="muted small">${RULE[g.type]}</span></legend>
    ${g.options.map((_, oi) => option(gi, oi)).join('')}
    ${g.type === 'SelectAtMostOne' ? `<label class="opt" data-g="${gi}" data-o="-1"><input type="radio" name="g${gi}" ${g.options.some(o => o.selected) ? '' : 'checked'}><span class="muted">None</span></label>` : ''}
    </fieldset>`;

  // The side panel: what an option does, with its picture.
  async function show(gi, oi) {
    const o = w.steps[at].groups[gi]?.options[oi];
    if (!o || shown === o) return;
    shown = o;
    $('#opt-info', d).innerHTML = `<h3>${esc(o.name)}</h3>${o.image ? '<div class="img"></div>' : ''}
      <p>${esc(o.description.trim()) || '<span class="muted">No description.</span>'}</p>`;
    if (!o.image) return;
    if (!images.has(o.image)) images.set(o.image, await call('fomod_image', { path, image: o.image }).catch(() => null));
    const src = images.get(o.image), box = $('#opt-info .img', d);
    if (shown === o && src && box) box.innerHTML = `<img src="${src}" alt="">`;
  }

  function draw(focus) {
    const step = w.steps[at], last = at === w.steps.length - 1;
    d.className = 'wizard';
    d.innerHTML = `<form method="dialog">
      <h2>${esc(w.module || basename(path))}</h2>
      <div class="stepline muted small">Step ${at + 1} of ${w.steps.length} · <b>${esc(step.name)}</b></div>
      <div class="body wiz">
        <div class="groups">${step.groups.map(group).join('')}</div>
        <aside id="opt-info"></aside>
      </div>
      <div class="actions">
        <span class="muted small summary">${plural(w.files, 'file')} to install${w.warnings.length ? `<br>⚠ ${esc(w.warnings.join(' · '))}` : ''}</span>
        <button value="cancel">Cancel</button>
        <button value="back" ${at ? '' : 'disabled'}>Back</button>
        <button value="${last ? 'install' : 'next'}" class="primary">${last ? 'Install' : 'Next'}</button>
      </div></form>`;
    shown = null;
    const [gi, oi] = focus ?? (() => {
      for (const [i, g] of step.groups.entries()) { const j = g.options.findIndex(o => o.selected); if (j >= 0) return [i, j]; }
      return [0, 0];
    })();
    show(gi, oi);
    const el = $(`[data-g="${gi}"][data-o="${oi}"] input`, d);
    if (focus && el) el.focus(); else $('.actions .primary', d).focus();
  }

  // Play the installer again with the choices; a change can make a later step's earlier choice
  // unusable, and then the later steps take their defaults.
  async function rewalk(choices) {
    try { return await call('fomod', { path, choices }); } catch {}
    choices.options.length = at + 1;
    return call('fomod', { path, choices });
  }

  return new Promise(resolve => {
    d.onsubmit = e => {
      const v = e.submitter?.value;
      if (v !== 'next' && v !== 'back') return; // cancel and install close the dialog
      e.preventDefault();
      at += v === 'next' ? 1 : -1;
      draw();
    };
    d.onchange = async e => {
      const label = e.target.closest('[data-g]');
      if (!label || working) return;
      const gi = +label.dataset.g, oi = +label.dataset.o, g = w.steps[at].groups[gi];
      const names = oi < 0 ? []
        : radio(g) ? [g.options[oi].name]
        : g.options.filter((o, i) => (i === oi ? e.target.checked : o.selected)).map(o => o.name);
      if (!names.length && g.type === 'SelectAtLeastOne') {
        e.target.checked = true;
        return toast(`“${g.name}” needs at least one option.`, 'warn');
      }
      const choices = structuredClone(w.choices);
      choices.options[at].groups[gi].choices = names.map(name => ({ name }));
      working = true;
      d.classList.add('working');
      try {
        w = await rewalk(choices);
        at = Math.min(at, w.steps.length - 1);
      } catch (err) {
        toast(String(err), 'error');
      } finally {
        working = false;
      }
      draw([gi, Math.max(0, oi)]);
    };
    const point = e => {
      const l = e.target.closest?.('label[data-o]');
      if (l && +l.dataset.o >= 0) show(+l.dataset.g, +l.dataset.o);
    };
    d.addEventListener('mouseover', point);
    d.addEventListener('focusin', point);
    d.onclose = () => {
      d.onsubmit = d.onchange = null;
      d.removeEventListener('mouseover', point);
      d.removeEventListener('focusin', point);
      resolve(d.returnValue === 'install' ? w.choices : null);
    };
    d.returnValue = '';
    draw();
    d.showModal();
    $('.actions .primary', d).focus();
  });
}

// ---- Dialogs and toasts ---------------------------------------------------------------------

/** Modal dialog. actions: [[value, label, class]]. Resolves to the chosen value (the input's
 *  text when `input` is given), or null when cancelled. */
function dialog({ title, body = '', actions, input }) {
  const d = $('#dlg');
  d.className = '';
  const field = input === undefined ? '' : '<input type="text" class="answer" spellcheck="false">';
  d.innerHTML = `<form method="dialog"><h2>${esc(title)}</h2><div class="body">${body}${field}</div>
    <div class="actions">${actions.map(([v, label, cls]) => `<button value="${v}" class="${cls ?? ''}">${esc(label)}</button>`).join('')}</div></form>`;
  const text = $('input.answer', d);
  if (text) text.value = input;
  text?.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); d.close('ok'); } });
  return new Promise(resolve => {
    d.returnValue = '';
    d.onclose = () => {
      const v = d.returnValue;
      resolve(!v || v === 'cancel' ? null : text ? text.value.trim() || null : v);
    };
    d.showModal();
    // Focus the text field, or the first button (Cancel), never a destructive default.
    if (text) text.select(); else $('.actions button', d).focus();
  });
}

const ask = (title, text, value) => dialog({ title, body: `<p>${esc(text)}</p>`, input: value, actions: [['cancel', 'Cancel'], ['ok', 'OK', 'primary']] });
const confirmBox = async (title, text, yes) => (await dialog({ title, body: `<p>${esc(text)}</p>`, actions: [['cancel', 'Cancel'], ['ok', yes, 'primary']] })) === 'ok';

function toast(msg, kind = 'info') {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = msg;
  el.title = 'Click to dismiss';
  el.onclick = () => el.remove();
  $('#toasts').append(el);
  setTimeout(() => el.remove(), kind === 'error' ? 15000 : 5000);
}

start();
