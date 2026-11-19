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
let failures = [];          // failed installs, Plays and downloads in this window, until dismissed
let details = null;         // mod_details of the selected mod
const nexusPages = new Map(); // Nexus mod id -> what its page says ('loading', the info, or { error })
const modPics = new Map();    // mod id -> its pictures as data: URLs, once asked for
let stamp = '';
let selected = new Set();   // selected mod and separator ids
let anchor = null;          // where Shift-selection starts
let cur = null;             // the row the arrow keys move from
let collapsed = new Set();  // collapsed separators, kept per instance in localStorage
let collapsedRoot = null;
let filters = new Set();    // state filters that are on (keys of CRITERIA)
let tab = 'mods';
let selPlugin = null;       // the plugin the panel shows: the one clicked last
let plugSel = new Set();    // selected plugin names
let plugAnchor = null;      // where a Shift-click range starts
let lootRules = null;       // { rules, groups } once a plugin was selected; null = reload
let toolsData = null;       // { tools, presets }
let ignored = new Set();    // health findings ignored in this instance ("kind|subject")
let busy = false;
let healthTimer = 0, healthRun = 0, filterTimer = 0;
let historyData = null;     // history_details, loaded when the History tab shows
let side = 'mods';          // which list was clicked last, when both show
let selPluginMod = null;    // the mod providing the selected plugin
let dropAt = null;          // { id, priority } while a file is dragged over a mod row

// Window preferences: columns, their widths, the tab, the filter and the split view.
const PREFS = 'rmm.prefs';
const prefs = (() => { try { return JSON.parse(localStorage.getItem(PREFS)) ?? {}; } catch { return {}; } })();
const savePrefs = () => { try { localStorage.setItem(PREFS, JSON.stringify(prefs)); } catch {} };
prefs.cols ??= { conflicts: true, tags: false, notes: false, installed: false, files: true, priority: true };
prefs.cols.category ??= true;
prefs.widths ??= {};
// This window's look and habits (Settings, "This window").
const WIN = { theme: 'system', density: 'comfortable', zoom: 1, simple: false, gettingStarted: true, startTab: 'last', poll: 1.5, toast: 5, recentDays: 7, confirmDrop: false, deployPreview: true, tabSize: 4, spaces: false, wrap: false, enterSends: true };
const W = prefs.win = { ...WIN, ...prefs.win };
function applyWin() {
  if (W.theme === 'system') delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = W.theme;
  document.body.classList.toggle('compact', W.density === 'compact');
  T.webview.getCurrentWebview().setZoom(W.zoom).catch(() => {});
  for (const el of [$('#ide-text'), $('#ide-gutter')]) el.style.tabSize = W.tabSize;
  $('#ide-text').wrap = W.wrap ? 'soft' : 'off';
  $('#ide-gutter').style.display = W.wrap ? 'none' : ''; // its numbers count unwrapped lines
  // Simple view: the tabs a beginner doesn't need yet (AI access is on Tools, the editor on AI).
  for (const t of SIMPLE_HIDES) $(`.tabs [data-tab="${t}"]`).hidden = W.simple;
  visibleTabs().forEach((t, i) => { $(`.tabs [data-tab="${t}"]`).title = i < 9 ? `Ctrl+${i + 1}` : 'Ctrl+,'; });
  if (W.simple && SIMPLE_HIDES.includes(tab)) { tab = 'mods'; if (!$('#app').hidden) showTab('mods'); }
  if (S) renderStart();
}
const SIMPLE_HIDES = ['data', 'tools', 'ai'];
const visibleTabs = () => $$('.tabs [data-tab]').filter(b => !b.hidden).map(b => b.dataset.tab);
// `w`: the default width in pixels, as style.css sets it for `cls`.
const COLS = {
  conflicts: { title: 'Conflicts', cls: 'conf-col', w: 200 },
  category: { title: 'Category', cls: 'cat-col', w: 140, tip: 'Click a category to show only its mods' },
  tags: { title: 'Tags', cls: 'tags-col', w: 160 },
  notes: { title: 'Notes', cls: 'notes-col', w: 200 },
  installed: { title: 'Installed', cls: 'date-col', w: 96 },
  version: { title: 'Version', cls: 'ver-col', w: 90, tip: 'The installed version, as Nexus names it' },
  files: { title: 'Files', cls: 'num', w: 64 },
  priority: { title: '#', cls: 'num', w: 64, tip: 'Priority: higher wins file conflicts' },
};
const shownCols = () => Object.keys(COLS).filter(k => prefs.cols[k]);
const splitOn = () => prefs.split !== false && innerWidth >= 1500;
const pluginSide = () => tab === 'plugins' || (tab === 'mods' && splitOn() && side === 'plugins' && !!selPlugin);
const nameOf = id => names.get(id) ?? id;
const isSep = id => !!byId.get(id)?.separator;

// ---- Calls ----------------------------------------------------------------------------------

/** Run one change: progress bar, errors as toasts, reload afterwards. Resolves to the call's
 *  result (true for calls that return nothing), or undefined when it failed. With `problem`, a
 *  failure is also listed in Problems until dismissed. */
async function act(label, fn, { reload = true, problem = false } = {}) {
  if (busy) return busyNote();
  setBusy(true, label);
  try {
    const r = await fn();
    if (reload) await load();
    return r ?? true;
  } catch (e) {
    toast(`${label} failed.\n${e}`, 'error');
    if (problem) {
      const f = { severity: 'error', subject: `${label} failed`, message: String(e) };
      failures = [f, ...failures.filter(x => x.subject !== f.subject || x.message !== f.message)].slice(0, 20); // a retry doesn't add a copy
      renderProblems();
    }
    if (reload) load().catch(() => {}); // undo what the window showed ahead of the call
  } finally {
    setBusy(false);
  }
}

let busyLabel = '';
function setBusy(on, label = '') {
  busy = on;
  busyLabel = label;
  $('#busy').hidden = !on;
  document.body.classList.toggle('busy', on);
  if (!on) {
    $('#busy div').classList.remove('det');
    $('#busy div').style.width = '';
    $('#progress').hidden = true;
  }
}

/** A click that comes while a change runs does nothing (one change at a time): say so, once. */
function busyNote() {
  const msg = `${busyLabel || 'Another change'} is still running. Try again when it's done.`;
  if (![...$('#toasts').children].some(t => t.textContent === msg)) toast(msg);
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

let loadRun = 0;
async function load() {
  // The stamp first: a change landing while the state is read leaves it old, so the next poll
  // reloads. A load that a newer one overtook is dropped rather than drawn over it.
  const run = ++loadRun;
  const st = await call('stamp'), s = await call('state');
  if (run !== loadRun) return;
  S = s;
  stamp = st;
  names = new Map(S.mods.map(m => [m.id, m.name]));
  byId = new Map(S.mods.map(m => [m.id, m]));
  idByName = new Map();
  for (const m of S.mods) if (!m.separator && !idByName.has(m.name)) idByName.set(m.name, m.id);
  $('#mod-names').innerHTML = [...idByName.keys()].map(n => `<option value="${esc(n)}">`).join('');
  const cats = [...new Set(S.mods.flatMap(m => m.categories ?? []))].sort((a, b) => a.localeCompare(b));
  $('#cat-names').innerHTML = cats.map(c => `<option value="${esc(c)}">`).join('');
  derive();
  // AI access follows the window to another instance, and comes back on with it.
  if (prefs.mcp && mcpRoot !== S.info.root) setMcp(true);
  if (S.info.root !== collapsedRoot) { loadCollapsed(); loadIgnored(); }
  for (const id of selected) if (!byId.has(id)) selected.delete(id);
  if (selPlugin && !S.plugins.some(p => p.name === selPlugin)) selPlugin = null;
  for (const n of plugSel) if (!S.plugins.some(p => p.name === n)) plugSel.delete(n);
  lootRules = null;
  if (tab === 'plugins' && selPlugin) loadLootRules();
  if (tab === 'tools' || toolsData) loadTools();
  issuesStale = true;
  historyData = null;
  if (tab === 'history') loadHistory();
  dataLoaded = false;
  if (tab === 'data') loadData(false);
  if (tab === 'saves') loadSaves();
  if (tab === 'downloads' || downloadsData) loadDownloads();
  render();
  refreshPlan();
  renderStart();
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

let planRun = 0;
async function refreshPlan() {
  const run = ++planRun;
  // Neither "Deployed" nor an old count while the new one is worked out.
  plan = null;
  renderDeploy();
  let p;
  try { p = { pending: await call('pending') }; } catch (e) { p = { error: String(e) }; }
  if (run !== planRun) return;
  plan = p;
  renderDeploy();
}

// Another process (the CLI, an AI over MCP) changed the instance: reload. One poll at a time: a
// load slower than the interval would otherwise be overtaken, and dropped, by the next poll's.
let polling = false;
let polled = 0;
setInterval(async () => {
  if (busy || polling || !S || document.hidden || !W.poll || Date.now() - polled < W.poll * 1000) return;
  polling = true;
  polled = Date.now();
  try {
    if ((await call('stamp')) !== stamp) await load();
  } catch {} finally { polling = false; }
}, 500);

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
  nexusShow().then(async () => { if ((await loadConf())?.nexus.check_on_start && nexus.user) checkUpdates(true); });
  takeLinks(); // a link that started the window
}

// The last tab, filter and state chips come back with the window.
const START = W.startTab === 'last' ? prefs.tab : W.startTab;
tab = ['mods', 'plugins', 'data', 'saves', 'downloads', 'problems', 'tools', 'history', 'ai', 'settings'].includes(START) ? START : 'mods';
$('#filter').value = prefs.filter ?? '';
filters = new Set((prefs.chips ?? []).filter(f => f in { enabled: 1, disabled: 1, conflicted: 1, overridden: 1, rule: 1, masters: 1, plugins: 1, recent: 1, update: 1, problems: 1 }));

let gamesFound = []; // the welcome screen's game folders: detected, then any the search adds

async function showWelcome() {
  $('#app').hidden = true;
  $('#welcome').hidden = false;
  gamesFound = await call('detect').catch(() => []);
  drawGames();
}

function drawGames() {
  const found = gamesFound, first = !recall(); // nothing opened before: making an instance comes first
  $('#open-instance').classList.toggle('primary', !first || !found.length);
  $('#search-drives').classList.toggle('primary', first && !found.length);
  $('#search-note').hidden = found.length > 0;
  $('#search-note').textContent = 'No game found through Steam or the registry. Auto search looks through every drive, for copies outside Steam too.';
  $('#create-games').innerHTML = found.map((d, i) =>
    `<button data-game="${esc(d.game)}" data-path="${esc(d.game_path)}"${first && !i ? ' class="primary"' : ''}>New instance for ${esc(GAMES[d.game] ?? d.game)}<br><span class="muted small">${esc(d.game_path)}</span></button>`).join('');
}

/** Same folder, whatever the case and trailing slash. */
const samePath = (a, b) => String(a).replace(/[\\/]+$/, '').toLowerCase() === String(b).replace(/[\\/]+$/, '').toLowerCase();

$('#search-drives').onclick = async () => {
  const r = await act('Searching every drive', () => call('search_drives'), { reload: false });
  if (!r || r === true) return;
  const added = r.games.filter(g => !gamesFound.some(d => samePath(d.game_path, g.game_path)));
  gamesFound = [...gamesFound, ...added];
  drawGames();
  toast(added.length ? `Found ${plural(added.length, 'more game folder')}.` : r.games.length ? 'No other game folders: the search found the same ones.' : 'No Skyrim Special Edition or Fallout 4 folder on any drive.');
};

async function enter(label, fn) {
  if (await act(label, fn)) showApp();
}

$('#open-instance').onclick = async () => {
  const path = await T.dialog.open({ directory: true, title: 'Open an rmm instance folder' });
  if (path) enter('Opening the instance', () => call('open', { path }));
};

$('#create-games').onclick = e => {
  const b = e.target.closest('[data-game]');
  if (b) newInstance(b.dataset.game, b.dataset.path);
};

$('#pick-game').onclick = async () => {
  const path = await T.dialog.open({ directory: true, title: 'The game’s folder, with SkyrimSE.exe or Fallout4.exe in it' });
  if (!path) return;
  const d = await call('identify_game', { path }).catch(() => null);
  if (!d) return toast('There’s no SkyrimSE.exe or Fallout4.exe in that folder. Pick the folder the game’s exe is in.', 'error');
  if (!gamesFound.some(g => samePath(g.game_path, d.game_path))) { gamesFound.push(d); drawGames(); }
  newInstance(d.game, d.game_path);
};

/** A new instance for the game in `gamePath`: a folder on the game's drive is suggested, since
 *  deploys link files there instead of copying them; Program Files is warned about. The first
 *  instance ever starts in the Simple view. */
async function newInstance(game, gamePath) {
  const first = !recall(), name = GAMES[game] ?? game, there = gamePath.slice(0, 2).toUpperCase();
  const suggested = `${there}\\rmm\\${name}`;
  const how = await dialog({
    title: `New instance for ${name}`,
    body: `<p>The instance folder holds your mods, profiles and undo history. On the game’s drive, deploys link mod files into the game folder instead of copying them.</p><p><b>${esc(suggested)}</b></p>`,
    actions: [['cancel', 'Cancel'], ['pick', 'Choose another folder…'], ['ok', 'Use this folder', 'primary']],
  });
  if (!how) return;
  const path = how === 'ok' ? suggested : await T.dialog.open({ directory: true, title: 'Choose a folder for the new instance', defaultPath: `${there}\\` });
  if (!path) return;
  const here = path.slice(0, 2).toUpperCase(), drive = d => `drive ${d.replace(':', '')}`;
  if (here !== there && !await confirmBox('Another drive', `The game is on ${drive(there)} and this folder on ${drive(here)}. Deploys would then copy mod files into the game folder instead of linking them, which takes as much space again. A folder on ${drive(there)} avoids that.`, 'Use this folder', 'primary')) return;
  if (/^[a-z]:\\program files( \(x86\))?(\\|$)/i.test(path) && !await confirmBox('Program Files', `Windows protects Program Files: programs can be stopped from writing there, or have what they write moved somewhere else without a word. A folder outside it, such as ${suggested}, avoids that.`, 'Use this folder', 'primary')) return;
  if (!await act('Creating the instance', () => call('create', { game, game_path: gamePath, path }))) return;
  if (first) { W.simple = true; savePrefs(); applyWin(); }
  showApp();
}

// ---- Virtual lists: only the rows on screen are in the DOM -------------------------------------

let rowH = 32; // measured from the first row painted
const lists = {
  mods: { table: $('#mods'), items: [], row: modRow, drag: null },
  plugins: { table: $('#plugins'), items: [], row: pluginRow, drag: null },
  data: { table: $('#data'), items: [], row: dataRow, drag: null },
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
  paint(lists.data);
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
  $('#redo').disabled = !i.redo_steps;
  $('#n-mods').textContent = i.mods ? `${i.enabled_mods}/${i.mods}` : '';
  $('#n-plugins').textContent = S.plugins.length ? S.plugins.filter(p => p.enabled).length : '';
  renderMods();
  renderPlugins();
  renderHistory();
  renderDetails();
  renderBisect();
}

const CRITERIA = {
  enabled: m => m.enabled,
  disabled: m => !m.enabled,
  conflicted: m => { const c = D.C[m.id]; return !!c && c.overwrites.length + c.overwritten_by.length > 0; },
  overridden: m => !!D.C[m.id]?.fully_overridden,
  rule: m => D.broken.has(m.id),
  masters: m => D.missing.has(m.id),
  plugins: m => m.plugins.length > 0,
  recent: m => (m.installed ?? 0) >= Date.now() / 1000 - W.recentDays * 86400,
  update: m => !!m.update,
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

/** Filter text: "#tag" matches tags and categories, anything else the name or the notes. */
function matches(m, f) {
  if (f.startsWith('#')) return f.length > 1 && [...(m.tags ?? []), ...(m.categories ?? [])].some(t => t.toLowerCase().startsWith(f.slice(1)));
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
  const masters = D.missing.has(m.id) ? '<span class="pill bad" title="One of its plugins needs a master that’s off or loads after it. A master is a plugin another is built on; without it the game crashes at start.">Masters</span>' : '';
  const note = m.notes ? `<span class="note-icon" title="${esc(m.notes)}">✎</span>` : '';
  const up = m.update;
  const upd = up ? `<span class="pill win" title="Nexus has ${esc(up.file_name)}${up.latest ? ` (version ${esc(up.latest)})` : ''}">Update${up.latest ? ' ' + esc(up.latest) : ''}</span>`
    : m.pinned ? '<span class="pill plain" title="Held at this version: update checks don’t offer newer ones (the mod’s panel releases it)">Held</span>' : '';
  const cell = {
    conflicts: () => `<td class="conf">${relTag}${conflictPills(D.C[m.id])}${masters}</td>`,
    category: () => `<td class="cats" title="${esc(m.categories?.join(', '))}">${(m.categories ?? []).map(c => `<span class="cat" data-cat="${esc(c)}">${esc(c)}</span>`).join('')}</td>`,
    tags: () => `<td class="muted small" title="${esc(m.tags?.join(', '))}">${esc(m.tags?.join(', '))}</td>`,
    notes: () => `<td class="muted" title="${esc(m.notes)}">${esc(m.notes)}</td>`,
    installed: () => `<td class="muted small">${m.installed ? new Date(m.installed * 1000).toLocaleDateString() : ''}</td>`,
    version: () => `<td class="muted small" title="${esc(m.version)}">${esc(m.version)}</td>`,
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
/** A Nexus mod's version, the update the last check found, and what holds its updates back. */
function updatesBlock(m, meta) {
  const u = m.update;
  return `<h3>Updates</h3>
    <div class="row">
      <span>${m.version ? `Version ${esc(m.version)}` : 'Version not known'}${u ? `; Nexus has <b>${esc(u.latest ?? u.file_name)}</b>` : ''}</span>
      <div class="spacer"></div>
      ${u ? '<button data-act="skip-update" title="Don’t offer this file; a later one is offered">Skip this version</button>' : ''}
    </div>
    <label class="row" style="margin:0"><input type="checkbox" data-act="pin" ${m.pinned ? 'checked' : ''}> Hold at this version (update checks don’t offer newer ones)</label>
    ${(meta.skip_file || meta.skip_version) && !u && !m.pinned ? '<div class="row"><span class="muted small">A newer file is skipped.</span><button class="link" data-act="unskip">Offer it again</button></div>' : ''}`;
}

function renderModHead() {
  const w = k => prefs.widths[k] ? ` style="width:${prefs.widths[k]}px"` : '';
  const rs = '<span class="rs" title="Drag to resize"></span>';
  $('#mods thead tr').innerHTML = `<th class="grip-col"></th><th class="check-col"></th><th>Mod</th>`
    + shownCols().map(k => `<th class="${COLS[k].cls}" data-col="${k}"${w(k)}${COLS[k].tip ? ` title="${COLS[k].tip}"` : ''}>${rs}${COLS[k].title}</th>`).join('');
  // Names keep room to be read (grip and checkbox take 62px); past that the list scrolls sideways.
  $('#mods').style.minWidth = `${62 + 220 + shownCols().reduce((s, k) => s + (prefs.widths[k] ?? COLS[k].w), 0)}px`;
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
  $('#mods-empty').textContent = S.mods.length ? 'No mods match the filter.' : 'No mods yet. Drop a .zip, .7z or .rar here, or use Install mod.';
  const ow = S.overwrite;
  $('#overwrite-bar').hidden = !ow;
  $('#overwrite-bar').innerHTML = ow ? `<b title="Files in the game’s Data folder that belong to no mod: what the game, tools or other programs wrote there. rmm keeps them here so they aren’t lost.">Overwrite</b><span class="muted small">${plural(ow, 'file')} that tools, the game or changes outside rmm left. They win over every mod.</span><span class="spacer"></span>
    <button data-ow="show">Show files</button><button data-ow="move" title="Move chosen files, MCM settings say, into a mod you have">Move to mod…</button><button data-ow="mod">Create mod…</button><button data-ow="clear" class="danger">Clear…</button>` : '';
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

/** A master that is on but loads after the plugin that needs it: present, just out of order. */
const loadsLater = m => S.plugins.some(x => x.enabled && x.name.toLowerCase() === m.toLowerCase());

function pluginRow(p, i) {
  const type = p.master ? (p.light ? 'ESL' : 'ESM') : p.light ? 'ESP · light' : 'ESP';
  const from = p.implicit ? 'Base game' : p.mod_id ? nameOf(p.mod_id) : 'Data folder';
  const issue = p.error ? `<span class="pill bad" title="${esc(p.error)}">Unreadable</span>`
    : p.enabled && p.missing_masters.length ? (p.missing_masters.every(loadsLater)
      ? `<span class="pill bad" title="${esc(p.missing_masters.join(', '))} load after it. Masters are the plugins this one is built on, and the game crashes at start until they load first: sort with LOOT, or move it below them.">Loads before its masters</span>`
      : `<span class="pill bad" title="Needs ${esc(p.missing_masters.join(', '))} turned on and loaded before it: masters are the plugins this one is built on, and the game crashes at start without them.">Missing masters</span>`) : '';
  const sp = selPlugin && S.plugins.find(x => x.name === selPlugin), om = oneMod();
  const rel = sp ? sp.masters.some(m => m.toLowerCase() === p.name.toLowerCase()) : !!om && p.mod_id === om.id;
  const cls = [p.implicit ? 'locked' : '', p.enabled ? '' : 'off', plugSel.has(p.name) ? 'sel' : '', rel ? 'rel' : '', ...dragCls(lists.plugins, p.name, i)];
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
    : 'Masters (ESM and ESL plugins, which others are built on) always load first. Drag to reorder; Ctrl+↑/↓ moves the selected ones, Space turns them on or off, right-click for more.';
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
  loadTrash();
}

// Uninstalled mods stay in trash/ for undo until the user empties it.
let trashData = null;
async function loadTrash() {
  trashData = await call('trash').catch(() => null);
  $('#trash-note').textContent = trashData?.entries ? `Trash: ${bytes(trashData.bytes)} in ${plural(trashData.entries, 'item')}` : '';
  $('#empty-trash').hidden = !trashData?.entries;
}
$('#empty-trash').onclick = async () => {
  const text = `Deletes ${bytes(trashData.bytes)} of uninstalled mods, files replaced by reinstalls and cleared overwrite for good. Undo can't bring those mods back afterwards.`;
  if (!await confirmBox('Empty the trash?', text, 'Delete for good')) return;
  const t = await act('Emptying the trash', () => call('empty_trash'));
  if (t && t !== true) toast(`Freed ${bytes(t.bytes)}.`);
};

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
    const n = plan.pending;
    b.innerHTML = n ? `Deploy <span class="badge">${n.toLocaleString()}</span>` : '✓ Deployed';
  }
}
const pending = p => p.link.length + p.remove.length + p.external_changes.length + (p.played?.length ?? 0);
/** A file list for a dialog; a first deploy can list hundreds of thousands. */
const fileList = (files, max = 2000) => esc(files.slice(0, max).join('\n')) + (files.length > max ? `\n… and ${(files.length - max).toLocaleString()} more` : '');

// ---- Details panel: one mod, one separator, or several rows -----------------------------------

const OVERWRITE_ID = '<overwrite>';

/** Tick files from a list, with a filter box; folders tick everything under them. Resolves to the
 *  ticked paths, or null when cancelled. `extra` is more of the dialog's body, read after it closes. */
async function pickFiles(title, files, intro, ok, extra = '') {
  const dirs = [...new Set(files.flatMap(f => f.split('/').slice(0, -1).map((_, i, a) => a.slice(0, i + 1).join('/'))))].sort();
  const rows = [...dirs.map(d => [d + '/', true]), ...files.map(f => [f, false])].sort((a, b) => a[0].localeCompare(b[0]));
  const body = `${intro}<input id="pf-find" type="search" placeholder="Filter…" spellcheck="false" aria-label="Filter the files">
    <div id="pf-list" class="pick">${rows.map(([p, dir]) => `<label class="row small"${dir ? ' style="font-weight:600"' : ''}><input type="checkbox" value="${esc(p)}"> ${esc(p)}</label>`).join('')}</div>${extra}`;
  const done = dialog({ title, body, actions: [['cancel', 'Cancel'], ['ok', ok, 'primary']] });
  $('#dlg').classList.add('wide');
  $('#pf-find').oninput = e => { const q = e.target.value.toLowerCase(); for (const l of $('#pf-list').children) l.hidden = !l.textContent.toLowerCase().includes(q); };
  $('#pf-list').onchange = e => {
    const v = e.target.value;
    if (v.endsWith('/')) for (const c of $('#pf-list').querySelectorAll('input')) if (c.value.startsWith(v)) c.checked = e.target.checked;
  };
  if (await done !== 'ok') return null;
  const picked = [...$('#pf-list').querySelectorAll('input:checked')].map(c => c.value);
  const folders = picked.filter(p => p.endsWith('/'));
  return picked.filter(p => !folders.some(d => p !== d && p.startsWith(d))).map(p => p.replace(/\/$/, ''));
}

/** Move files from a mod (or Overwrite) into another mod, asking again before replacing. */
async function moveFiles(from, files) {
  const where = `<label class="field">Into the mod <input id="mf-to" list="mod-names" spellcheck="false" placeholder="A mod’s name"></label>`;
  const paths = await pickFiles(`Move files from ${from === OVERWRITE_ID ? 'Overwrite' : nameOf(from)}`, files, '<p class="muted small">Tick the files or folders to move, MCM settings say, then name the mod they belong with. The next deploy uses them from there.</p>', 'Move', where);
  if (!paths?.length) return;
  const to = idByName.get($('#mf-to').value.trim()) ?? ($('#mf-to').value.trim().toLowerCase() === 'overwrite' ? OVERWRITE_ID : null);
  if (!to) return toast('Pick one of your mods by its name.', 'error');
  let r = await call('move_files', { from, to, paths }).catch(e => String(e));
  if (typeof r === 'string') {
    if (!r.includes('of these already') || !await confirmBox('Replace files?', `${esc(r.replace(/^.*?: /, ''))}.`, 'Replace')) return toast(r, 'error');
    r = await act('Moving the files', () => call('move_files', { from, to, paths, replace: true }), { reload: false });
    if (!r || r === true) return;
  }
  toast(`Moved ${plural(r.length, 'file')} into ${to === OVERWRITE_ID ? 'Overwrite' : nameOf(to)}. Deploy uses ${r.length === 1 ? 'it' : 'them'} from there.`, 'info', ['Move back', () => act('Moving them back', () => call('move_files', { from: to, to: from, paths: r }))]);
  await load();
}

/** Hide files of a mod (or show them again): out of the game, kept through reinstalls. */
async function hideFiles(id, paths, show = false) {
  const r = await act(show ? 'Showing the files' : 'Hiding the files', () => call('hide_files', { id, paths, show }), { problem: true });
  if (!r || r === true) return;
  details = null;
  toast(`${show ? 'Showing' : 'Hid'} ${plural(r.length, 'file')} in ${nameOf(id)}. The next deploy ${show ? 'puts' : 'takes'} ${r.length === 1 ? 'it' : 'them'} ${show ? 'in' : 'out of'} the game.`, 'info', [show ? 'Hide again' : 'Show again', () => hideFiles(id, r, !show)]);
  loadDetails();
}

/** Creation Club content in the game's Data folder as mods, or back into Data. */
async function creations() {
  const [adopt, back] = await Promise.all([call('creations', { dry_run: true }), call('creations', { dry_run: true, restore: true })]).catch(e => { toast(String(e), 'error'); return []; });
  if (!adopt) return;
  if (!adopt.mods.length && !back.mods.length) return toast('The game’s Data folder has no Creation Club content (the game’s .ccc file lists none that’s there).');
  const body = `${adopt.mods.length ? `<p>${plural(adopt.mods.length, 'Creation')} in the game’s Data folder (${bytes(adopt.bytes)}) can become ${adopt.mods.length === 1 ? 'a mod' : 'mods, one each,'} at the top of every profile and on. Then you can turn ${adopt.mods.length === 1 ? 'it' : 'each'} off, and Purge or Play without mods leaves them out. Their files move into rmm’s mods folder; the next deploy links them back.</p>` : ''}
    ${back.mods.length ? `<p>${plural(back.mods.length, 'Creation')} ${back.mods.length === 1 ? 'is a mod' : 'are mods'} now; putting them back moves their files into the game’s Data folder (after a Purge) and takes the mods out of every profile.</p>` : ''}
    <p class="muted small">This isn’t in the undo history, as it moves the game’s own files: Put back in Data, here, undoes it.</p>`;
  const actions = [['cancel', 'Cancel']];
  if (back.mods.length) actions.push(['back', 'Put back in Data']);
  if (adopt.mods.length) actions.push(['adopt', 'Make them mods', 'primary']);
  const what = await dialog({ title: 'Creation Club content', body, actions });
  if (what === 'adopt') act('Making the Creations mods', async () => { const r = await call('creations', {}); toast(`${plural(r.mods.length, 'Creation')} ${r.mods.length === 1 ? 'is a mod' : 'are mods'} now. Deploy or Play links ${r.mods.length === 1 ? 'it' : 'them'} back into the game.`); });
  if (what === 'back') act('Putting the Creations back', async () => { const r = await call('creations', { restore: true }); toast(`${plural(r.mods.length, 'Creation')} ${r.mods.length === 1 ? 'is' : 'are'} back in the game’s Data folder.`); });
}

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
  // Notes being typed survive the redraws that reloads bring.
  const note = $('#mod-note');
  const draft = note && note === document.activeElement && note.dataset.id === m.id ? [note.value, note.selectionStart, note.selectionEnd] : null;
  box.innerHTML = `
    <h2>${esc(m.name)}</h2>
    ${m.id !== m.name ? `<div class="muted small">id: ${esc(m.id)}</div>` : ''}
    ${m.version || d?.meta?.nexus ? `<div class="row small">${m.version && !d?.meta?.nexus ? `<span class="muted">Version ${esc(m.version)}</span>` : ''}${d?.meta?.nexus ? '<button class="link" data-act="nexus-page" title="Open its page on Nexus Mods in your browser">Nexus page ↗</button>' : ''}</div>` : ''}
    <div class="row">
      <label class="row" style="margin:0"><input type="checkbox" data-act="enable" ${m.enabled ? 'checked' : ''}> Enabled</label>
      <div class="spacer"></div>
      <button data-act="move">Move to…</button>
      <button class="danger" data-act="uninstall">Uninstall…</button>
    </div>
    <div class="row">
      <button data-act="rename-mod" title="F2">Rename…</button>
      <button data-act="reinstall" ${d && d.meta?.source && !d.meta.source.startsWith('tool:') ? '' : 'disabled title="Only mods installed from an archive or folder"'}>Reinstall…</button>
      <button data-act="update-mod" title="Replace its files with a newer download, keeping its place, notes, categories, colour and rules">Update…</button>
      <input type="color" data-act="mod-color" value="${esc(m.color ?? '#808080')}" title="Row colour" aria-label="Row colour">
      ${m.color ? '<button data-act="mod-nocolor" title="Remove the row colour">No colour</button>' : ''}
    </div>
    ${m.tags?.length ? `<div class="tagline">${m.tags.map(t => `<span class="pill plain">${esc(t)}</span>`).join('')}</div>` : ''}
    ${d?.meta?.nexus ? updatesBlock(m, d.meta) : ''}
    <h3>Categories</h3>
    <div class="chips">${(m.categories ?? []).map(c => `<span class="chip">${esc(c)}<button data-uncat="${esc(c)}" title="Remove the category" aria-label="Remove ${esc(c)}">×</button></span>`).join('') || '<span class="muted">None.</span>'}</div>
    <div class="row"><input class="cat-add" list="cat-names" placeholder="Add a category…" aria-label="Add a category" spellcheck="false"></div>
    ${d?.meta?.nexus ? nexusBlock(d.meta.nexus.mod_id) : ''}
    ${d?.docs?.length ? `<h3>Read me and settings</h3><div class="files">${d.docs.map(f => `<button class="link" data-doc="${esc(f)}" title="Open in the editor (AI tab)">${esc(f)}</button>`).join('<br>')}</div>` : ''}
    ${d?.images?.length ? `<h3>Pictures (${d.images.length})</h3>${modPics.has(m.id)
      ? `<div class="pics">${modPics.get(m.id).map(([f, url]) => url ? `<img src="${url}" alt="${esc(f)}" title="${esc(f)}" data-pic="${esc(f)}">` : `<span class="muted small">${esc(f)}: can’t be shown</span>`).join('')}</div>`
      : '<div class="row"><button data-act="mod-pics">Show them</button></div>'}` : ''}
    <h3>Notes</h3>
    <textarea id="mod-note" data-id="${esc(m.id)}" rows="2" placeholder="Notes for this mod…" spellcheck="true">${esc(m.notes ?? '')}</textarea>
    ${d?.review ? reviewBlock(d.review) : ''}
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
    <div class="row"><button data-act="hide-files" title="Keep chosen files out of the game, through reinstalls and updates (MO2’s hide)">Hide files…</button><button data-act="move-files" title="Move chosen files into another mod">Move files…</button></div>
    <div class="files">${d ? d.files.map(esc).join('<br>') + (d.files_truncated ? '<br>…' : '') : '…'}</div>
    ${d?.meta?.hidden?.length ? `<h3>Hidden (${d.meta.hidden.length})</h3><p class="muted small">Kept out of the game, also after a reinstall or update.</p>
      <div class="files">${d.meta.hidden.map(h => `<div class="row"><span>${esc(h)}</span><div class="spacer"></div><button class="link small" data-unhide="${esc(h)}">Show</button></div>`).join('')}</div>` : ''}
    ${d?.meta?.source ? `<h3>Installed from</h3><div class="small muted">${esc(d.meta.source)}</div>` : ''}`;
  if (draft) {
    const n = $('#mod-note');
    n.value = draft[0];
    n.focus();
    n.setSelectionRange(draft[1], draft[2]);
  }
}

/** What the mod's Nexus page says, read when asked: summary, picture, description. */
function nexusBlock(modId) {
  const p = nexusPages.get(modId);
  if (!p) return '<h3>On Nexus</h3><div class="row"><button data-act="nexus-info" title="Reads the mod’s Nexus page (no sign-in needed)">Show its description</button></div>';
  if (p === 'loading') return '<h3>On Nexus</h3><p class="muted">Reading its page…</p>';
  if (p.error) return `<h3>On Nexus</h3><p class="muted small">${esc(p.error)}</p><div class="row"><button data-act="nexus-info">Try again</button></div>`;
  return `<h3>On Nexus</h3>
    ${p.picture ? `<img class="nexus-pic" src="${esc(p.picture)}" alt="">` : ''}
    <p>${esc(p.summary)}</p>
    <div class="small muted">By ${esc(p.author)} · version ${esc(p.version)} · ${p.endorsements.toLocaleString()} endorsements${p.updated ? ` · updated ${new Date(p.updated).toLocaleDateString()}` : ''}</div>
    ${p.description ? `<details><summary class="small">Description</summary><div class="desc small">${esc(p.description)}</div></details>` : ''}`;
}

// A review an AI (or the user) saved with the mod: its verdict, notes and place in the order.
function reviewBlock(r) {
  const pill = r.verdict === 'clean' ? 'win' : r.verdict === 'malicious' ? 'bad' : 'lose';
  return `<h3>Review</h3>
    <p><span class="pill ${pill}">${esc(r.verdict)}</span> <span class="small muted">${new Date(r.date * 1000).toLocaleDateString()}${r.current ? '' : ' · the files changed since, so it is out of date'}</span></p>
    <p class="small">${esc(r.notes)}</p>
    ${r.load_order ? `<p class="small"><b>Order:</b> ${esc(r.load_order)}</p>` : ''}`;
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
    ${mods ? `<h3>Categories</h3>
    <div class="row"><input class="cat-add" list="cat-names" placeholder="Add a category to these mods…" aria-label="Add a category to the selected mods" spellcheck="false"></div>
    <div class="row"><button data-act="cat-clear">Remove their categories</button></div>` : ''}
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
    ${p.masters.length ? '<p class="muted small">The plugins this one is built on. They have to be on and load before it.</p>' : ''}
    ${p.masters.length ? `<ul>${p.masters.map(m => `<li>${!lower.has(m.toLowerCase()) ? '' : loadsLater(m) ? '<span class="pill bad">Loads after it</span>' : '<span class="pill bad">Missing</span>'}<button class="link" data-plugin="${esc(m)}">${esc(m)}</button></li>`).join('')}</ul>` : '<p class="muted">None.</p>'}
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

/** Click (or arrow) selection in the plugin list: plain replaces, Ctrl toggles, Shift selects the
 *  range from the anchor; null clears it. The panel shows the plugin picked last. */
function pickPlugin(name, { shift = false, ctrl = false } = {}) {
  const view = lists.plugins.items, at = n => view.findIndex(p => p.name === n);
  if (!name) plugSel = new Set();
  else if (shift && at(plugAnchor) >= 0 && at(name) >= 0) {
    if (!ctrl) plugSel = new Set();
    const [a, b] = [at(plugAnchor), at(name)];
    for (let i = Math.min(a, b); i <= Math.max(a, b); i++) plugSel.add(view[i].name);
  } else if (ctrl) {
    if (!plugSel.delete(name)) plugSel.add(name);
    plugAnchor = name;
  } else {
    plugSel = new Set([name]);
    plugAnchor = name;
  }
  if (name && !plugSel.has(name)) name = [...plugSel].pop() ?? null;
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
  if (!ids.length) return;
  if (busy) return busyNote();
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
  const plugins = ids.flatMap(id => byId.get(id)?.plugins ?? []);
  const used = plugins.length ? await call('newest_save_using', { plugins }).catch(() => null) : null;
  if (!used) {
    if (await confirmBox(title, text, mods ? 'Uninstall' : 'Remove')) act('Uninstalling', () => call('uninstall', { ids }));
    return;
  }
  const [save, its] = used;
  const v = await dialog({ title, body: `<p>${esc(text)}</p><p>Your latest save, <b>${esc(save)}</b>, was made with ${esc(its.join(', '))}. Loading the save without ${its.length > 1 ? 'those plugins' : 'that plugin'} can break it. Disable instead if you may go back to that save.</p>`,
    actions: [['cancel', 'Cancel'], ['off', 'Disable instead'], ['ok', 'Uninstall', 'danger']] });
  if (v === 'off') act('Disabling', () => call('enable_mods', { ids: ids.filter(id => !isSep(id)), enabled: false }));
  else if (v === 'ok') act('Uninstalling', () => call('uninstall', { ids }));
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
  if (order.every((id, i) => id === S.mods[i].id)) return;
  if (busy) return busyNote();
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
    if (!await confirmBox(`Reinstall ${nameOf(id)}?`, `Its files are read again from ${path}. The old files go to the instance's trash folder, and Undo puts them back.`, 'Reinstall', 'primary')) return;
  } else {
    choices = await fomodWizard(path, walk);
    if (!choices) return;
  }
  const r = await act(`Reinstalling ${nameOf(id)}`, () => call('reinstall', { id, choices }));
  if (r) toast(`Reinstalled ${r.name} (${plural(r.files, 'file')}).${r.warnings.length ? '\n' + r.warnings.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
}

/** Replace a mod's files with a newer version; a FOMOD installer opens with the choices it was
 *  installed with. */
async function updateMod(id, path, nx = null) {
  const d = await call('details', { id }).catch(() => null);
  const walk = await act(`Reading ${basename(path)}`, async () => {
    try { return await call('fomod', { path, choices: d?.meta?.fomod ?? null }); }
    catch { return call('fomod', { path }); } // the stored choices don't fit the new installer
  }, { reload: false, problem: true });
  if (!walk) return;
  let choices;
  if (walk !== true) {
    choices = walk.steps.length ? await fomodWizard(path, walk) : walk.choices;
    if (!choices) return;
  }
  const r = await act(`Updating ${nameOf(id)}`, () => call('update_mod', { id, path, choices, nexus: nx?.nexus }), { problem: true });
  if (r && r !== true) toast(`Updated ${r.name} from ${basename(path)} (${plural(r.files, 'file')}). Undo puts the old files back.${r.warnings.length ? '\n' + r.warnings.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
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
  } else if (what === 'move') {
    const files = await act('Reading overwrite', () => call('overwrite_files'), { reload: false });
    if (files) moveFiles(OVERWRITE_ID, files);
  } else if (what === 'mod') {
    const name = await ask('Create a mod from Overwrite', 'Everything in Overwrite becomes a new mod at the end of the list.', 'Overwrite output');
    if (name) act('Creating the mod', async () => { const r = await call('mod_from_overwrite', { name }); toast(`Created ${r.name} (${plural(r.files, 'file')}).`); });
  } else if (what === 'clear') {
    if (await confirmBox('Clear Overwrite?', `Moves ${plural(S.overwrite, 'file')} to the instance's trash; the next deploy removes them from the game folder. They are often settings the game saved (MCM menus, for example) or a tool's output: Create mod… keeps them instead. Undo brings them back as a mod named “Cleared overwrite”.`, 'Clear')) {
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

/** LOOT's messages are Markdown: bold, code and links, the rest as text. */
const markdown = text => esc(text)
  .replace(/\*\*(.+?)\*\*/g, '<b>$1</b>')
  .replace(/`([^`]+)`/g, '<code>$1</code>')
  .replace(/\[([^\]]+)\]\((https:\/\/[^\s)]+)\)/g, (_, t, u) => `<button class="link" data-link="${u}" title="${u}">${t}</button>`);
let showIgnored = false;

function renderProblems() {
  const rank = { error: 0, warning: 1, info: 2 };
  const all = [...failures, ...[...(issues ?? [])].sort((a, b) => rank[a.severity] - rank[b.severity])];
  const hidden = all.filter(i => ignored.has(keyOf(i))).length;
  const list = showIgnored ? all : all.filter(i => !ignored.has(keyOf(i)));
  $('#issues').innerHTML = list.length
    ? list.map(i => {
      const k = keyOf(i), ign = ignored.has(k), to = i.kind && targetOf(i);
      return `<li class="${esc(i.severity)}${to ? ' link' : ''}${ign ? ' ignored' : ''}" data-key="${esc(k)}" ${to ? `data-mod="${esc(to.mod ?? '')}" data-plugin="${esc(to.plugin ?? '')}" title="Show it in the list"` : ''}>
        <span class="sev">${esc(i.severity)}</span><span class="subject">${esc(to?.mod ? nameOf(to.mod) : i.subject)}</span><span class="message">${i.kind === 'loot_message' ? markdown(i.message) : esc(i.message)}</span>
        ${i.kind ? `<button class="link ign" data-ignore="${esc(k)}">${ign ? 'Stop ignoring' : 'Ignore'}</button>` : ''}${failures.includes(i) ? `<button class="link ign" data-dismiss="${failures.indexOf(i)}">Dismiss</button>` : ''}</li>`;
    }).join('')
    : `<li class="info"><span class="sev">ok</span><span class="subject">No problems found${hidden ? ' apart from ignored ones' : ''}.</span></li>`;
  $('#show-ignored').hidden = !hidden;
  $('#install-missing').hidden = !all.some(i => i.kind === 'nexus_requirement' && !ignored.has(keyOf(i)));
  $('#clean-dirty').hidden = !all.some(i => i.kind === 'dirty_plugin' && !ignored.has(keyOf(i)));
  $('#rebuild-stale').hidden = !all.some(i => i.kind === 'stale_output' && !ignored.has(keyOf(i)));
  $('#show-ignored').textContent = showIgnored ? 'Hide ignored' : `Show ${hidden} ignored`;
  // Mods with a finding, for the Problems filter.
  D.flagged = new Set(all.filter(i => !ignored.has(keyOf(i)) && i.severity !== 'info').map(i => targetOf(i)?.mod).filter(Boolean));
  const shown = all.filter(i => !ignored.has(keyOf(i)));
  const serious = shown.filter(i => i.severity !== 'info').length;
  $('#n-problems').textContent = serious || '';
  $('#n-problems').classList.toggle('bad', shown.some(i => i.severity === 'error'));
  if (filters.has('problems')) renderMods();
  const info = shown.length - serious, lootRan = all.some(i => i.kind === 'loot_message');
  $('#problems-note').textContent = [serious ? `${serious} to look at` : 'Nothing to look at', info ? `${info} for information` : '', hidden ? `${hidden} ignored` : ''].filter(Boolean).join(', ') + '.'
    + (lootRan ? '' : ' LOOT’s messages show here once its masterlist is downloaded (Sort with LOOT downloads it).');
}

/** Run again, in order, the whole-setup tools whose generated output is out of date. */
async function rebuildOutputs() {
  const plan = await act('Checking the outputs', () => call('rebuild_outputs', { dry_run: true }), { reload: false });
  if (!plan || plan === true) return;
  if (!toolsData) await loadTools();
  const name = id => toolsData?.tools.find(t => t.id === id)?.tool.name ?? id;
  if (!plan.steps.length) {
    return dialog({ title: 'Rebuild outputs', body: '<p>Every generated output is up to date. The outputs of xLODGen, TexGen, DynDOLOD, Pandora, BodySlide, Synthesis and PGPatcher are checked against the mods and load order they were made from.</p>', actions: [['ok', 'OK', 'primary']] });
  }
  const body = `<p>rmm runs these one after another, each a run of its own (deploy, run, capture; Undo takes one back):</p>
    <ol>${plan.steps.map(([id, why]) => `<li><b>${esc(name(id))}</b> <span class="muted small">${esc(why)}</span></li>`).join('')}</ol>
    <p class="muted small">TexGen, DynDOLOD, xLODGen and PGPatcher need you to click through their windows; the next one starts when one closes. A failed run stops the rest, since the later tools read its output.</p>`;
  if (await dialog({ title: 'Rebuild outputs', body, actions: [['cancel', 'Cancel'], ['ok', `Run ${plural(plan.steps.length, 'tool')}`, 'primary']] }) !== 'ok') return;
  const r = await act('Rebuilding outputs', () => call('rebuild_outputs', {}), { problem: true });
  if (!r || r === true) return;
  loadTools();
  runHealth();
  const rows = r.done.map(([id, m]) => `<li>${esc(name(id))}: ${m ? `<b>${esc(nameOf(m))}</b>` : 'captured nothing'}</li>`).join('');
  dialog({
    title: r.failed ? 'Rebuild stopped' : 'Outputs rebuilt',
    body: `${rows ? `<ul>${rows}</ul>` : ''}${r.failed ? `<p>⚠ ${esc(name(r.failed[0]))} failed: ${esc(r.failed[1])}</p><p class="muted small">The tools after it didn’t run.</p>` : ''}`,
    actions: [['ok', 'OK', 'primary']],
  });
}
$('#rebuild-outputs').onclick = rebuildOutputs;
$('#rebuild-stale').onclick = rebuildOutputs;

/** Clean the plugins LOOT knows as dirty with xEdit QuickAutoClean, one run each. */
$('#clean-dirty').onclick = async () => {
  const r = await call('clean_dirty', { dry_run: true }).catch(e => { toast(String(e), 'error', /tool first/.test(String(e)) ? ['Open Tools', () => showTab('tools')] : null); return null; });
  if (!r) return;
  const what = d => `${d.itm ? plural(d.itm, 'ITM') : ''}${d.itm && d.deleted_references ? ', ' : ''}${d.deleted_references ? plural(d.deleted_references, 'deleted reference') : ''}`;
  const body = (r.plugins.length ? `<p>xEdit QuickAutoClean cleans ${plural(r.plugins.length, 'plugin')}, one run after another. Each clean copy goes into its own “Cleaned …” mod above the original, so Undo or turning that mod off brings the original back.</p>
      <ul>${r.plugins.map(d => `<li><b>${esc(d.plugin)}</b> <span class="muted small">${esc(what(d))}${d.deleted_navmeshes ? '; its deleted navmeshes take xEdit by hand' : ''}</span></li>`).join('')}</ul>` : '<p>No plugin LOOT knows as dirty can be cleaned automatically.</p>')
    + (r.by_hand.length ? `<p class="muted small">Only deleted navmeshes, which QuickAutoClean can’t fix (xEdit by hand, or leave them): ${esc(r.by_hand.map(d => d.plugin).join(', '))}.</p>` : '')
    + '<p class="muted small">xEdit opens for each plugin and closes when done; this takes a while. ITMs are records identical to the master’s; deleted references can crash the game when another mod uses them.</p>';
  if (!r.plugins.length) return dialog({ title: 'Dirty plugins', body, actions: [['cancel', 'Close']] });
  if (await dialog({ title: 'Clean dirty plugins', body, actions: [['cancel', 'Cancel'], ['ok', `Clean ${plural(r.plugins.length, 'plugin')}`, 'primary']] }) !== 'ok') return;
  const done = await act('Cleaning plugins', () => call('clean_dirty', {}), { problem: true });
  if (!done || done === true) return;
  const part = (title, xs) => xs.length ? `<p><b>${esc(title)}</b></p><ul class="small">${xs.map(x => `<li>${esc(x)}</li>`).join('')}</ul>` : '';
  await dialog({
    title: 'Cleaning done',
    body: part('Cleaned', done.mods.map(([p, m]) => m ? `${p} → ${m}` : `${p} (xEdit found nothing to remove)`)) + part('Couldn’t clean', done.failed.map(([p, e]) => `${p}: ${e}`)) + part('LOOT still lists as dirty', done.still_dirty)
      + (done.mods.some(([p, m]) => !m && done.still_dirty.includes(p)) ? '<p class="muted small">Where xEdit found nothing to remove, LOOT’s entry likely comes from another xEdit version; the plugin is fine as it is.</p>' : ''),
    actions: [['ok', 'OK', 'primary']],
  });
  runHealth();
};

/** What the mods' Nexus pages list as required and this setup lacks: install it all (Premium) or
 *  open the Files tabs (free), and turn on what's installed but off. */
$('#install-missing').onclick = async () => {
  let list = await call('lacking_requirements').catch(e => { toast(String(e), 'error'); return null; });
  if (!list) return;
  list = list.filter(l => !ignored.has(`nexus_requirement|${l.for_mod}`));
  const by = new Map();
  for (const l of list) {
    const k = l.disabled ?? l.mod_id ?? l.name.toLowerCase();
    const e = by.get(k) ?? { ...l, for: [] };
    e.for.push(nameOf(l.for_mod));
    by.set(k, e);
  }
  const rows = [...by.values()];
  if (!rows.length) return toast('Nothing the mods need is missing.');
  const premium = !!nexus.user?.is_premium;
  const get = rows.filter(r => r.mod_id && !r.disabled), off = rows.filter(r => r.disabled), other = rows.filter(r => !r.mod_id && !r.disabled);
  const li = (name, r) => `<li><b>${esc(name)}</b> <span class="muted small">for ${esc(r.for.join(', '))}</span></li>`;
  const body = (get.length ? `<p>${premium ? 'To download and install, each one’s newest main file' : 'To download on Nexus'}:</p><ul>${get.map(r => li(r.name, r)).join('')}</ul>` : '')
    + (off.length ? `<p>Installed but off, to turn on:</p><ul>${off.map(r => li(nameOf(r.disabled), r)).join('')}</ul>` : '')
    + (other.length ? `<p class="muted small">Not on Nexus, so these are yours to get: ${other.map(r => esc(r.dlc ? `the ${r.name} DLC (on Steam)` : `${r.name} (${r.page})`)).join('; ')}.</p>` : '')
    + (get.length && !premium ? '<p class="muted small">Free accounts download through the website: rmm opens each one’s Files tab, where “Mod manager download” installs it here.</p>' : '');
  const what = [get.length && (premium ? `install ${get.length}` : `open ${plural(get.length, 'page')}`), off.length && `turn on ${off.length}`].filter(Boolean).join(' and ');
  if (!what) return dialog({ title: 'What’s missing', body, actions: [['cancel', 'Close']] });
  if (await dialog({ title: 'What’s missing', body, actions: [['cancel', 'Cancel'], ['ok', what[0].toUpperCase() + what.slice(1), 'primary']] }) !== 'ok') return;
  if (off.length) await act('Turning the mods on', () => call('enable_mods', { ids: off.map(r => r.disabled), enabled: true }));
  if (!get.length) return;
  if (premium) {
    nxmQueue.push(...get.map(r => ({ link: String(r.mod_id), at: null })));
    return runQueue();
  }
  for (const r of get.slice(0, 10)) await call('open_nexus_mod', { mod_id: r.mod_id }).catch(e => toast(String(e), 'error'));
  if (get.length > 10) toast(`Opened the first 10 pages; Install what’s missing… opens the rest once those are in.`);
};

// ---- Tabs, filters, keys --------------------------------------------------------------------

function showTab(t) {
  tab = t;
  prefs.tab = t;
  savePrefs();
  const split = wasSplit = splitOn();
  $$('.tabs [data-tab]').forEach(b => b.classList.toggle('on', b.dataset.tab === t));
  for (const p of ['mods', 'plugins', 'data', 'saves', 'downloads', 'problems', 'tools', 'history', 'ai', 'settings']) $('#pane-' + p).hidden = p !== t && !(split && t === 'mods' && p === 'plugins');
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
  if (t === 'data' && !dataLoaded) loadData();
  if (t === 'saves') loadSaves(); // the game writes them; read them each time
  if (t === 'downloads' && !downloadsData) loadDownloads();
  if (t === 'ai') ideInit();
  if (t === 'settings') { aiLoad(); drawMore(); }
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
    ${r.next_steps.length ? `<h3>What to try</h3><ul>${r.next_steps.map(s => `<li>${esc(s)}</li>`).join('')}</ul>` : ''}
    ${r.call_stack.length ? `<details><summary>Call stack</summary><pre>${esc(r.call_stack.join('\n'))}</pre></details>` : ''}
    <div class="row"><button type="button" id="copy-report" title="A report to paste into a forum post or chat, without your Windows user name">Copy report</button><button type="button" id="save-bundle" title="A zip of the setup and its logs to attach to a post, without your Windows user name">Save support bundle…</button><button type="button" id="find-culprit" title="Turn off half of the mods at a time and play, until one is left">Find the culprit…</button>${W.simple ? '' : '<button type="button" id="ask-ai" title="rmm’s AI reads the log and your setup, and says what it thinks and what to try; it changes nothing without asking">Ask AI about it</button>'}<span class="muted small" id="crash-note" role="status"></span></div>
    ${others.length ? `<label class="field">Older crash logs<select id="older"><option value="">Choose…</option>${others.map(o => `<option value="${esc(o.path)}">${esc(basename(o.path))} · ${esc(when(o.modified))}</option>`).join('')}</select></label>` : ''}
    <p class="muted small" style="margin-top:10px">A name in a crash log is a lead, not a verdict: the mod that crashed often isn't the one at fault.</p>`;
  const done = dialog({ title: 'Crash log', body, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  $('#dlg').onclick = e => { const id = e.target.closest('[data-goto]')?.dataset.goto; if (id) { $('#dlg').close(); showTab('mods'); goTo(id); } };
  const older = $('#older');
  if (older) older.onchange = () => { const p = older.value; if (p) { $('#dlg').close(); setTimeout(() => crashLog(p)); } };
  const note = t => { $('#crash-note').textContent = t; };
  $('#copy-report').onclick = async () => note(await copyText(r.text, $('#dlg')) ? 'Copied. Paste it into your post.' : 'Couldn’t copy the report.');
  $('#save-bundle').onclick = async () => {
    const b = await saveBundle(r.path, note);
    if (b) note(`Saved ${b.path}`);
  };
  $('#find-culprit').onclick = () => { $('#dlg').close(); setTimeout(() => startBisect(r)); };
  $('#ask-ai')?.addEventListener('click', () => {
    $('#dlg').close();
    setTimeout(() => askAi(`My game crashed. Read the crash log ${r.path} with read_crash_log, then tell me in plain words what most likely caused it and what I should try first. Change nothing until I say so.`));
  });
  await done;
  $('#dlg').onclick = null;
}
$('#crash-log').onclick = () => crashLog(null);
$('#support-bundle').onclick = async () => {
  const b = await saveBundle(null, t => toast(t, 'error'));
  if (b) toast(`Saved ${b.path}. Your Windows user name is taken out; look inside before you share it.`);
};

/** Start a new chat with rmm's AI about `text` (on the AI tab, which says what's missing to set up). */
async function askAi(text) {
  showTab('ai');
  if (aiRunning) return toast('The AI is still busy; stop it or wait, then ask again.');
  if (!aiProviders) await aiLoad();
  if (aiChat) aiNewChat();
  $('#ai-text').value = text;
  $('#ai-form').requestSubmit();
}

/** Copy text; the fallback selects it in a text box inside `box` (an open dialog stays modal). */
async function copyText(text, box = document.body) {
  try { await navigator.clipboard.writeText(text); return true; } catch { /* the fallback below */ }
  const ta = Object.assign(document.createElement('textarea'), { value: text });
  box.append(ta);
  ta.select();
  const ok = document.execCommand('copy');
  ta.remove();
  return ok;
}

/** Ask for a folder and save a support bundle there (with `crash`, that crash log). */
async function saveBundle(crash, fail) {
  const dir = await T.dialog.open({ directory: true, title: 'Where to save the support bundle' });
  if (!dir) return null;
  try { return await call('support_bundle', { dir, crash }); } catch (e) { fail(`Couldn’t save the bundle: ${e}`); return null; }
}

/** Ask what a test run does: a save, cells, seconds in each, the window, screenshots. */
async function askScenario(title, intro) {
  const t = (await loadConf())?.test ?? { window: '960x540', dwell: 10, screenshots: true };
  const sizes = [['960x540', 'Small, behind your other windows'], ['1280x720', '1280×720, behind your other windows'], ['fullscreen', 'The game’s own display settings']];
  if (!sizes.some(([v]) => v === t.window)) sizes.unshift([t.window, `${t.window.replace('x', '×')}, behind your other windows`]);
  const body = `${intro}
    <label class="field">Save to load <input id="tr-save" type="text" placeholder="optional: the save's name without extension" spellcheck="false"></label>
    <label class="field">Cells to visit <input id="tr-cells" type="text" placeholder="Riverwood, WhiterunBreezehome" spellcheck="false"></label>
    <label class="field">Seconds in each cell <input id="tr-dwell" type="number" min="1" value="${t.dwell}"></label>
    <label class="field">Window <select id="tr-window">${sizes.map(([v, l]) => `<option value="${esc(v)}"${v === t.window ? ' selected' : ''}>${esc(l)}</option>`).join('')}</select></label>
    <label class="row" style="margin-top:12px"><input id="tr-shots" type="checkbox" ${t.screenshots ? 'checked' : ''}> A screenshot in each cell</label>`;
  if (await dialog({ title, body, actions: [['cancel', 'Cancel'], ['run', 'Start the game', 'primary']] }) !== 'run') return null;
  return {
    save: $('#tr-save').value.trim() || null,
    cells: $('#tr-cells').value.split(',').map(c => c.trim()).filter(Boolean),
    dwell: Math.max(1, Number($('#tr-dwell').value) || t.dwell),
    window: $('#tr-window').value,
    screenshots: $('#tr-shots').checked,
  };
}

$('#test-run').onclick = async () => {
  const scenario = await askScenario('Test run', `<p>Deploys, starts the game through the script extender and checks that it holds up. With rmm's harness plugin installed as a mod it waits for the main menu, loads the save or goes to each cell with <code>coc</code>, and measures FPS; without it, it only watches the game. A crash log written during the run fails it.</p>`);
  if (!scenario) return;
  const r = await act('The test run', () => call('test_run', scenario));
  if (r && r !== true) await showTestRun(r);
};

/** Explain the bisect and start it over the crash log's mods, or every enabled mod. */
async function startBisect(r) {
  if (S.bisect) return toast('A search for the culprit is under way: finish or stop it in the bar under the tabs.');
  const named = [...new Set(r.suspects.map(s => s.mod_id).filter(id => id && byId.get(id)?.enabled))];
  const all = S.mods.filter(m => !m.separator && m.enabled).length;
  const games = n => plural(Math.ceil(Math.log2(Math.max(n, 1))) + 1, 'game');
  const body = `<p>rmm turns off half of the mods, with the mods that need them, and you play to where the game crashed. Then you say whether it still crashed: rmm turns them back on and halves what's left, until one mod is left. Each step is in History, and Stop turns everything back on.</p>
    <p class="muted small">It works when the crash happens every time at the same place. With rmm's harness plugin, Test each step for me plays the game for you.</p>
    <label class="row"><input type="radio" name="bi" value="named" ${named.length ? 'checked' : 'disabled'}> ${named.length ? `The ${plural(named.length, 'mod')} the crash log names (about ${games(named.length)})` : 'The crash log names no enabled mods'}</label>
    <label class="row"><input type="radio" name="bi" value="all" ${named.length ? '' : 'checked'}> Every enabled mod but the script extender (about ${games(all)})</label>`;
  if (await dialog({ title: 'Find the culprit', body, actions: [['cancel', 'Cancel'], ['go', 'Turn off the first half', 'primary']] }) !== 'go') return;
  const args = $('input[name=bi]:checked')?.value === 'all' ? { all: true } : { mods: named };
  await act('Turning off the first half', () => call('bisect', { action: 'start', ...args }));
}

/** The bar under the tabs while a bisect is under way: what's off, and how the game went. */
function renderBisect() {
  const b = S.bisect, bar = $('#bisect-bar');
  bar.hidden = !b;
  if (!b) return void (bar.innerHTML = '');
  const list = ids => { const n = ids.map(nameOf); return n.length > 5 ? `${n.slice(0, 5).join(', ')} and ${n.length - 5} more` : n.join(', '); };
  const text = b.done ? b.message
    : `Step ${b.steps.length + 1}: ${plural(b.off.length, 'mod')} off (${list(b.off)}). Play to where the game crashed, then say how it went. ${plural(b.left.length, 'suspect')} left, about ${plural(b.steps_left, 'step')} to go.`;
  const buttons = b.done
    ? `${b.culprit ? `<button data-bi="show">Show ${esc(nameOf(b.culprit))}</button>` : ''}<button data-bi="stop">Done</button>`
    : `<button data-bi="crashed">It still crashed</button><button data-bi="ok">It didn’t crash</button><button data-bi="run" title="Play each remaining step with a test run, the game in a small window behind yours">Test each step for me…</button><button data-bi="stop" title="Turn this step's mods back on and end the search">Stop</button>`;
  bar.innerHTML = `<b>Find the culprit</b><span class="small" title="${esc(b.off.map(nameOf).join(', '))}">${esc(text)}</span><span class="spacer"></span>${buttons}`;
}
$('#bisect-bar').onclick = async e => {
  const what = e.target.closest('[data-bi]')?.dataset.bi;
  if (!what) return;
  if (what === 'show') { showTab('mods'); return goTo(S.bisect.culprit); }
  if (what === 'run') {
    const scenario = await askScenario('Test each step', '<p>For each step left, rmm deploys, starts the game and checks that it holds up, as Test run does. A crash, or with the harness a step that fails, counts as a crash. Give it the save or the cells where the game crashed.</p>');
    if (scenario) await act('Testing each step', () => call('bisect', { action: 'run', scenario }));
    return;
  }
  await act(what === 'stop' ? 'Turning the mods back on' : 'Turning on this step’s mods, and off the next half', () => call('bisect', { action: what }));
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
  const ini = { 'skyrim-se': 'SkyrimCustom.ini', fallout4: 'Fallout4Custom.ini' }[S.info.game] ?? 'the game’s Custom INI';
  const off = r.logging_enabled ? '' : `<p>The game writes no Papyrus log while its logging is off. To turn it on, add these two lines to ${esc(ini)} in the game’s My Games folder, then play again:</p><pre>[Papyrus]\nbEnableLogging=1</pre>`;
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
$('#se-log').onclick = async () => {
  const r = await act('Reading the script extender log', () => call('se_log'), { reload: false });
  if (!r || r === true) return;
  const se = basename(r.path).toLowerCase().startsWith('f4se') ? 'F4SE' : 'SKSE';
  const failed = r.plugins.filter(p => !p.loaded);
  const row = p => `<tr><td>${esc(p.file)}${p.changed ? ' <span class="muted">(its mod changed since)</span>' : ''}</td>
    <td>${p.loaded ? 'loaded' : `<span class="pill lose">${esc(p.problem ?? 'not loaded: the log ends after checking it')}</span>`}</td><td>${modLink(p.mod_id)}</td></tr>`;
  const body = !r.modified ? `<p class="muted">There is no ${esc(basename(r.path))} yet. ${se} writes it each time the game starts through it.</p>` : `
    <p class="muted small">${esc(basename(r.path))} · ${esc(when(r.modified))} · ${plural(r.plugins.length - failed.length, 'plugin')} loaded${failed.length ? `, ${failed.length} didn't` : ''}</p>
    ${r.plugins.length ? `<table class="fc">${[...failed, ...r.plugins.filter(p => p.loaded)].map(row).join('')}</table>` : `<p class="muted">No ${se} plugins were loaded.</p>`}`;
  const done = dialog({ title: 'Script extender log', body, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  $('#dlg').onclick = e => { const id = e.target.closest('[data-goto]')?.dataset.goto; if (id) { $('#dlg').close(); showTab('mods'); goTo(id); } };
  await done;
  $('#dlg').onclick = null;
};
$('#show-ignored').onclick = () => { showIgnored = !showIgnored; renderProblems(); };
$('#issues').onclick = e => {
  const dismiss = e.target.closest('[data-dismiss]')?.dataset.dismiss;
  if (dismiss) {
    failures.splice(Number(dismiss), 1);
    return renderProblems();
  }
  const link = e.target.closest('[data-link]')?.dataset.link;
  if (link) return void act('Opening the page', () => call('open_link', { url: link }), { reload: false });
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
  if (!$('#ctx').hidden && k === 'Escape') { closeCtx(); return; }
  // Shortcuts: Ctrl+1 to 9 and Ctrl+, for the tabs, Ctrl+D Deploy, Ctrl+P Play, F5 reads the instance again.
  if (e.ctrlKey && !e.shiftKey && !e.altKey) {
    const t = /^[1-9]$/.test(k) ? visibleTabs()[+k - 1] : k === ',' ? 'settings' : null;
    if (t) { e.preventDefault(); return showTab(t); }
    if (low === 'd') { e.preventDefault(); return $('#deploy').click(); }
    if (low === 'p') { e.preventDefault(); return $('#play').click(); }
  }
  if (k === 'F5') { e.preventDefault(); return void load().catch(err => toast(String(err), 'error')); }
  if (e.ctrlKey && low === 'f' && lists[tab]) {
    e.preventDefault();
    const box = tab === 'data' ? $('#data-find') : $('#filter');
    box.focus();
    box.select();
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
  else if (!(e.target.closest('button') && (k === ' ' || k === 'Enter'))) {
    if (tab === 'plugins' || (tab === 'mods' && splitOn() && side === 'plugins')) pluginKeys(e);
    else if (tab === 'mods') modKeys(e);
  }
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
  else if (e.ctrlKey && (k === 'ArrowLeft' || k === 'ArrowRight')) { e.preventDefault(); setAllGroups(k === 'ArrowRight'); }
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
    if (busy) { busyNote(); return e.preventDefault(); }
    // A checkbox in a selection of several applies to all of them.
    setEnabled(selected.has(id) && selected.size > 1 ? selIds() : [id], e.target.checked);
  } else if (e.target.closest('.twisty')) toggleGroup(id);
  else if (e.target.closest('.cat') && !e.shiftKey && !e.ctrlKey) {
    // A category label filters the list to its mods, like MO2's category filter.
    $('#filter').value = `#${e.target.closest('.cat').dataset.cat.toLowerCase()}`;
    filterChanged();
  } else if (!e.target.closest('.grip')) pick(id, { shift: e.shiftKey, ctrl: e.ctrlKey });
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
  const t = e.target.closest('button, input, img[data-pic]');
  if (!t) return;
  const id = selIds()[0];
  if (t.dataset.goto) { if (tab !== 'mods') showTab('mods'); goTo(t.dataset.goto); }
  else if (t.dataset.plugin) {
    if (S.plugins.some(p => p.name.toLowerCase() === t.dataset.plugin.toLowerCase())) pickPlugin(S.plugins.find(p => p.name.toLowerCase() === t.dataset.plugin.toLowerCase()).name);
    else toast(`${t.dataset.plugin} isn’t installed.`, 'warn');
  } else if (t.dataset.unloot) {
    const old = lootRules?.rules.find(r => r.plugin.toLowerCase() === selPlugin.toLowerCase());
    setLootRule({ after: (old?.after ?? []).filter(a => a !== t.dataset.unloot) });
  } else if (t.dataset.unhide) {
    hideFiles(id, [t.dataset.unhide], true);
  } else if (t.dataset.doc) {
    if (W.simple) showDoc(id, t.dataset.doc); // the editor is in the AI tab, which Simple hides
    else { showTab('ai'); ideOpen(`mods/${id}/${t.dataset.doc}`); }
  } else if (t.dataset.pic) {
    const url = t.src;
    dialog({ title: t.dataset.pic, body: `<img src="${url}" alt="" style="max-width:100%">`, actions: [['ok', 'Close', 'primary']] });
  } else if (t.dataset.uncat) {
    editCategories([id], [], [t.dataset.uncat]);
  } else if (t.dataset.unrule) {
    const key = t.dataset.key;
    setModRule(id, key, (S.rules.find(r => r.id === id)?.[key] ?? []).filter(a => a !== t.dataset.unrule));
  } else switch (t.dataset.act) {
    case 'enable':
      if (busy) { busyNote(); return e.preventDefault(); }
      return setEnabled([id], t.checked);
    case 'on': return setEnabled(selIds(), true);
    case 'off': return setEnabled(selIds(), false);
    case 'move': return moveDialog();
    case 'uninstall': return removeSelected();
    case 'rename': return renameSep(id);
    case 'rename-mod': return renameMod(id);
    case 'reinstall': return reinstall(id);
    case 'update-mod': {
      const path = await T.dialog.open({ title: `The new version of ${nameOf(id)}`, filters: [{ name: 'Mod archives', extensions: ['zip', '7z', 'rar'] }] });
      return path && updateMod(id, path);
    }
    case 'nocolor': return act('Changing the colour', () => call('edit_separator', { id, color: '' }));
    case 'mod-nocolor': return act('Changing the colour', () => call('set_color', { id, color: '' }));
    case 'pin':
      if (busy) { busyNote(); return e.preventDefault(); }
      return act(t.checked ? 'Holding its updates' : 'Offering its updates again', () => call('hold_updates', { ids: [id], hold: t.checked ? 'pin' : 'release' }));
    case 'skip-update': return act('Skipping the update', () => call('hold_updates', { ids: [id], hold: 'skip' }));
    case 'unskip': return act('Offering its updates again', () => call('hold_updates', { ids: [id], hold: 'release' }));
    case 'files': return fileConflicts(id);
    case 'nexus-page': return call('open_nexus_mod', { mod_id: details.meta.nexus.mod_id, page: true }).catch(err => toast(String(err), 'error'));
    case 'nexus-info': {
      const modId = details.meta.nexus.mod_id;
      nexusPages.set(modId, 'loading');
      renderDetails();
      nexusPages.set(modId, await call('nexus_mod_info', { mod_id: modId }).catch(err => ({ error: String(err) })));
      return renderDetails();
    }
    case 'mod-pics': {
      const files = details.images.slice(0, 24);
      modPics.set(id, await Promise.all(files.map(async f => [f, await call('mod_image', { id, path: f }).catch(() => null)])));
      return renderDetails();
    }
    case 'hide-files': {
      const files = await call('mod_files', { id }).catch(err => { toast(String(err), 'error'); return null; });
      const paths = files && await pickFiles(`Hide files in ${nameOf(id)}`, files, '<p class="muted small">Ticked files leave the game at the next deploy and stay hidden through reinstalls and updates; the panel lists them to show again. A hidden plugin is like MO2’s optional ESP.</p>', 'Hide');
      return paths?.length && hideFiles(id, paths);
    }
    case 'move-files': {
      const files = await call('mod_files', { id }).catch(err => { toast(String(err), 'error'); return null; });
      return files && moveFiles(id, files);
    }
    case 'cat-clear': {
      const ids = selIds().filter(i => !isSep(i));
      return editCategories(ids, [], [...new Set(ids.flatMap(i => byId.get(i).categories ?? []))]);
    }
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
  if (t.classList.contains('cat-add') && t.value.trim()) return editCategories(selIds().filter(i => !isSep(i)), [t.value.trim()], []);
  if (t.dataset.act === 'lock') act(t.checked ? 'Locking the plugin' : 'Unlocking the plugin', () => call('lock_plugin', { name: selPlugin, lock: t.checked }));
  if (!t.classList.contains('rule-add') || !t.value.trim()) return;
  const other = idByName.get(t.value.trim()), key = t.dataset.key;
  if (!other) return toast(`No mod is named “${t.value.trim()}”.`, 'warn');
  setModRule(id, key, [...(S.rules.find(r => r.id === id)?.[key] ?? []), other]);
});

/** Add and remove category labels on mods (not in undo history, like notes). */
function editCategories(ids, add, remove) {
  if (ids.length) return act('Changing categories', () => call('categories', { ids, add, remove }));
}

// Notes save when the box loses focus (a redraw can restore unsaved text, which `change` misses).
$('#details').addEventListener('focusout', e => {
  const t = e.target;
  if (t.id === 'mod-note' && byId.has(t.dataset.id) && t.value.trim() !== (byId.get(t.dataset.id).notes ?? '')) {
    act('Saving the notes', () => call('set_note', { id: t.dataset.id, note: t.value }));
  }
});

// ---- Plugins --------------------------------------------------------------------------------

$('#plugins tbody').addEventListener('click', e => {
  const tr = e.target.closest('tr[data-name]');
  if (!tr) return;
  const name = tr.dataset.name;
  if (!e.target.matches('input[type=checkbox]')) {
    const only = name === selPlugin && plugSel.size === 1 && !e.shiftKey && !e.ctrlKey;
    if (!e.target.closest('.grip')) pickPlugin(only ? null : name, { shift: e.shiftKey, ctrl: e.ctrlKey });
    return;
  }
  if (busy) { busyNote(); return e.preventDefault(); }
  // A checkbox in a selection of several applies to all of them.
  setPluginsEnabled(plugSel.has(name) && plugSel.size > 1 ? selPlugins() : [name], e.target.checked);
});
draggable(lists.plugins, {
  blockOf: p => plugSel.has(p.name) && plugSel.size > 1 ? selPlugins() : [p.name],
  drop: (names, t, after) => {
    if (names.includes(t.name)) return;
    const ti = pluginsWithout(names).indexOf(t.name);
    placePlugins(names, ti < 0 ? 0 : ti + (after ? 1 : 0));
  },
});

/** The selected plugins that can move or change, in load order. */
const selPlugins = () => S.plugins.filter(p => plugSel.has(p.name) && !p.implicit).map(p => p.name);
const pluginsWithout = names => { const set = new Set(names); return S.plugins.filter(p => !p.implicit && !set.has(p.name)).map(p => p.name); };

function setPluginsEnabled(names, enabled) {
  if (!names.length) return;
  if (busy) return busyNote();
  for (const p of S.plugins) if (names.includes(p.name)) p.enabled = enabled;
  paint(lists.plugins);
  act(names.length > 1 ? (enabled ? 'Enabling the plugins' : 'Disabling the plugins') : 'Changing the plugin', () => call('enable_plugins', { names, enabled }));
}

/** Put `names`, in their order, at `at` in the load order without them. */
function placePlugins(names, at) {
  const locked = names.filter(n => D.locks.has(n.toLowerCase()));
  if (locked.length) return toast(`${locked.join(', ')} ${locked.length > 1 ? 'are' : 'is'} locked at ${locked.length > 1 ? 'their positions' : 'its position'}. Untick “Lock at this position” in the plugin’s panel to move it.`);
  const order = pluginsWithout(names);
  order.splice(at, 0, ...names);
  const now = S.plugins.filter(p => !p.implicit).map(p => p.name);
  if (order.every((n, i) => n === now[i])) return;
  act(names.length > 1 ? 'Moving the plugins' : 'Moving the plugin', () => call('plugin_order', { names: order }));
}

/** Ctrl+Up/Down: the selected plugins one place, together. */
function nudgePlugins(dir) {
  const names = selPlugins();
  if (!names.length) return;
  const first = S.plugins.filter(p => !p.implicit).map(p => p.name).indexOf(names[0]);
  placePlugins(names, Math.max(0, Math.min(pluginsWithout(names).length, first + dir)));
}

async function movePluginsDialog() {
  const names = selPlugins();
  if (!names.length) return;
  const rest = pluginsWithout(names);
  const what = names.length > 1 ? `The ${plural(names.length, 'plugin')}, in their order,` : esc(names[0]);
  const body = `<p>${what} go just after:</p><input id="pm-after" list="pm-names" placeholder="A plugin, or empty for the top" aria-label="The plugin to go after" spellcheck="false">
    <datalist id="pm-names">${rest.map(n => `<option value="${esc(n)}">`).join('')}</datalist>
    <label class="row" style="margin-top:10px"><input type="checkbox" id="pm-end"> To the bottom instead</label>`;
  const done = dialog({ title: 'Move to', body, actions: [['cancel', 'Cancel'], ['ok', 'Move', 'primary']] });
  $('#pm-after').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); $('#dlg').close('ok'); } };
  $('#pm-after').focus();
  if (await done !== 'ok') return;
  if ($('#pm-end').checked) return placePlugins(names, rest.length);
  const v = $('#pm-after').value.trim();
  const at = v ? rest.findIndex(n => n.toLowerCase() === v.toLowerCase()) : -1;
  if (v && at < 0) return toast(`No plugin is named “${v}”.`, 'warn');
  placePlugins(names, at + 1);
}

function pluginKeys(e) {
  const view = lists.plugins.items, k = e.key;
  const i = view.findIndex(p => p.name === selPlugin);
  const page = Math.max(1, Math.floor(scroller(lists.plugins).clientHeight / rowH) - 2);
  const go = j => { if (view.length) pickPlugin(view[Math.max(0, Math.min(view.length - 1, j))].name, { shift: e.shiftKey }); };
  const moves = { ArrowUp: -1, ArrowDown: 1, PageUp: -page, PageDown: page };
  if (k in moves) {
    e.preventDefault();
    if (e.ctrlKey && Math.abs(moves[k]) === 1) nudgePlugins(moves[k]);
    else go(i < 0 ? 0 : i + moves[k]);
  } else if (k === 'Home' || k === 'End') { e.preventDefault(); go(k === 'Home' ? 0 : view.length - 1); }
  else if (e.ctrlKey && k.toLowerCase() === 'a') {
    e.preventDefault();
    plugSel = new Set(view.map(p => p.name));
    paint(lists.plugins);
  } else if (k === ' ') {
    e.preventDefault();
    const names = selPlugins();
    setPluginsEnabled(names, !names.every(n => S.plugins.find(p => p.name === n)?.enabled));
  } else if (k === 'Escape') {
    if (filterText()) { $('#filter').value = ''; filterChanged(); } else if (plugSel.size) pickPlugin(null);
  }
}

// ---- Right-click menus ------------------------------------------------------------------------

/** A menu at the pointer: items are [label, fn, class] or '-' for a line. */
function ctxMenu(e, items) {
  e.preventDefault();
  const pop = $('#ctx');
  pop.innerHTML = items.map((it, i) => it === '-' ? '<hr>' : `<button class="item ${it[2] ?? ''}" role="menuitem" data-i="${i}"${it[3] ? ` title="${esc(it[3])}"` : ''}>${esc(it[0])}</button>`).join('');
  pop.hidden = false;
  const r = pop.getBoundingClientRect();
  pop.style.left = `${Math.max(4, Math.min(e.clientX, innerWidth - r.width - 4))}px`;
  pop.style.top = `${Math.max(4, Math.min(e.clientY, innerHeight - r.height - 4))}px`;
  pop.onclick = ev => {
    const b = ev.target.closest('[data-i]');
    if (!b) return;
    pop.hidden = true;
    items[+b.dataset.i][1]();
  };
  $('button', pop)?.focus();
}
const closeCtx = () => { $('#ctx').hidden = true; };
addEventListener('blur', closeCtx);
for (const L of Object.values(lists)) scroller(L).addEventListener('scroll', closeCtx);

$('#mods tbody').addEventListener('contextmenu', e => {
  const tr = e.target.closest('tr[data-id]');
  if (!tr) return;
  const id = tr.dataset.id;
  if (!byId.has(id)) return;
  if (!selected.has(id)) pick(id);
  const ids = selIds(), m = byId.get(id), mods = ids.filter(x => !isSep(x)), one = ids.length === 1;
  const items = [];
  if (mods.length) items.push([mods.length > 1 ? `Enable ${mods.length} mods` : 'Enable', () => setEnabled(mods, true)], [mods.length > 1 ? `Disable ${mods.length} mods` : 'Disable', () => setEnabled(mods, false)]);
  items.push(['Move to…', moveDialog]);
  if (one && m.separator) items.push(['Rename…', () => renameSep(id)], ['Select its mods', () => { toggleGroup(id, true); selected = new Set(D.kids.get(id).map(k => k.id)); selectionChanged(); }]);
  if (one && !m.separator) items.push(['Rename…', () => renameMod(id)], ['Reinstall…', () => reinstall(id)], ['File conflicts', () => fileConflicts(id)]);
  items.push('-', ['Collapse every group', () => setAllGroups(false)], ['Expand every group', () => setAllGroups(true)], '-');
  items.push([ids.length > 1 ? `Remove ${ids.length} rows…` : m.separator ? 'Remove the separator…' : 'Uninstall…', removeSelected, 'danger']);
  ctxMenu(e, items);
});

$('#plugins tbody').addEventListener('contextmenu', e => {
  const tr = e.target.closest('tr[data-name]');
  if (!tr) return;
  if (!plugSel.has(tr.dataset.name)) pickPlugin(tr.dataset.name);
  const names = selPlugins(), p = S.plugins.find(x => x.name === tr.dataset.name);
  if (!names.length) return e.preventDefault(); // base-game plugins: nothing to change
  const n = names.length > 1 ? ` ${names.length} plugins` : '';
  const items = [[`Enable${n}`, () => setPluginsEnabled(names, true)], [`Disable${n}`, () => setPluginsEnabled(names, false)], ['Move to…', movePluginsDialog]];
  if (names.length === 1) {
    const locked = D.locks.has(p.name.toLowerCase());
    items.push([locked ? 'Unlock its position' : 'Lock at this position', () => act(locked ? 'Unlocking the plugin' : 'Locking the plugin', () => call('lock_plugin', { name: p.name, lock: !locked }))]);
    if (p.mod_id) items.push(['Show its mod', () => { showTab('mods'); goTo(p.mod_id); }], ['Hide the plugin', () => hideFiles(p.mod_id, [p.name]), '', 'Keeps it out of the game (MO2’s optional ESP); its mod’s panel shows it again']);
  }
  ctxMenu(e, items);
});

/** Collapse or expand every separator's group. */
function setAllGroups(open) {
  if (filtering()) return toast('Clear the filter first: it shows every matching mod anyway.');
  collapsed = open ? new Set() : new Set(S.mods.filter(m => m.separator).map(m => m.id));
  saveCollapsed();
  renderMods();
  renderDetails();
}

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
    if (!tr || e.button !== 0) return;
    if (busy) return busyNote();
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
  if (busy) return busyNote();
  let plan;
  setBusy(true, 'Checking the game folder');
  try { plan = await call('plan'); } catch (e) { plan = { error: String(e) }; } finally { setBusy(false); }
  if (plan.error) {
    return dialog({ title: 'Deploy is blocked', body: `<p>${esc(plan.error)}</p>`, actions: [['cancel', 'Close']] });
  }
  const n = pending(plan);
  const part = (label, files, note) => files.length
    ? `<details><summary><b>${files.length.toLocaleString()}</b> ${label}</summary>${note ? `<p class="muted small">${note}</p>` : ''}<pre>${fileList(files)}</pre></details>` : '';
  const body = n
    ? part('to link', plan.link)
      + part('to remove', plan.remove)
      + part('original files to back up', plan.backup, 'The game’s own files a mod replaces. They come back when the mod goes away, or on Purge.')
      + part('changed outside rmm', plan.external_changes, 'Kept in Overwrite (the bar above the mod list), where they keep winning over every mod.')
      + part('written by the game during play', plan.played ?? [], 'Moved to Overwrite and linked back.')
      + `<p class="muted small" style="margin-top:10px">${plural(plan.unchanged, 'file')} already in place.</p>`
    : `<p>The game folder already matches this profile (${plural(plan.unchanged, 'file')}).</p>`;
  const r = n && !W.deployPreview ? 'apply' : await dialog({ title: 'Deploy to the game folder', body: body + '<p class="muted small">Purge… takes every mod rmm put in the game folder out again and puts the game’s own files back.</p>', actions: [['purge', 'Purge…'], ...(n ? [['cancel', 'Cancel'], ['apply', 'Deploy', 'primary']] : [['cancel', 'Close']])] });
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
  const part = (label, list) => list?.length ? `<details><summary><b>${list.length.toLocaleString()}</b> ${label}</summary><pre>${fileList(list)}</pre></details>` : '';
  const body = `<p>The game folder goes back to how it was before rmm: deployed files are removed and backed-up originals restored. Your mods and profiles stay; Deploy puts everything back.</p>`
    + part('to remove', p.remove) + part('changed outside rmm, copied to Overwrite first', p.external_changes) + part('originals to restore', p.backup);
  if (await dialog({ title: 'Purge the game folder', body, actions: [['cancel', 'Cancel'], ['apply', 'Purge', 'danger']] }) !== 'apply') return;
  const r = await act('Purging', () => call('purge', { dry_run: false }));
  if (r) toast(`Purged: ${plural(r.remove.length, 'file')} removed.`);
}

$('#play').onclick = async () => {
  let error = '';
  const r = await act('Starting the game', () => call('launch').catch(e => { error = String(e); throw e; }), { problem: true });
  if (/script extender is built for game version/.test(error)) {
    $$('#toasts .toast.error').pop()?.remove(); // the dialog says it
    const v = await dialog({ title: 'The script extender doesn’t match the game', body: `<p>${esc(error)}</p><p class="muted small">Until there’s a matching one, Play without mods starts the game with none: rmm takes them out of the game folder (Purge), and the next Play puts them back. Saves made with mods are best left alone until then.</p>`,
      actions: [['cancel', 'Close'], ['vanilla', 'Play without mods', 'primary']] });
    if (v === 'vanilla') playVanilla(true);
    return;
  }
  if (!r) return;
  const what = [`game ${r.game_version}`, r.script_extender].filter(Boolean).join(', ');
  toast(`Started ${basename(r.exe)} (${what}).${r.warnings.length ? '\n' + r.warnings.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
  if (!prefs.playHint) {
    prefs.playHint = true;
    savePrefs();
    toast('Start the game with Play from now on: starting it from Steam or a desktop shortcut skips the script extender and any changes not deployed yet.');
  }
};

/** Purge and start the game's own exe; `asked`: the user already chose it in a dialog. */
async function playVanilla(asked = false) {
  if (!asked && !await confirmBox('Play without mods?', 'rmm takes your mods out of the game folder (Purge) and starts the game itself, without the script extender. The next Play puts the mods back. Saves made with mods are best left alone meanwhile.', 'Play without mods', 'primary')) return;
  const r = await act('Starting the game without mods', () => call('launch', { without_mods: true }));
  if (r && r !== true) toast(`Started ${basename(r.exe)} (game ${r.game_version}) without mods. Play puts them back.`);
}

/** Make the game's Steam manifest read-only (hold) or writable again. */
async function steamHold(hold) {
  if (hold && !await confirmBox('Hold game updates on Steam?', 'rmm makes the game’s Steam manifest read-only, so Steam can’t update the game, and keeps a copy of it. A game update breaks the script extender and the mods that need it until they catch up. Steam may say the game needs an update; Let Steam update the game, in this menu, undoes it.', 'Hold updates', 'primary')) return;
  if (!hold && !await confirmBox('Let Steam update the game?', 'Its Steam manifest becomes writable again, and Steam may update the game right away. A game update breaks the script extender and the mods that need it until they catch up, and undoes a downgraded game.', 'Let Steam update it')) return;
  const r = await act(hold ? 'Holding game updates' : 'Letting Steam update the game', () => call('steam_updates', { hold }), { reload: false });
  if (r && r !== true) toast(r.held ? 'Steam can’t update the game now. Problems reminds you while it’s held.' : 'Steam can update the game again.');
  runHealth();
}

$('#play-more').onclick = async e => {
  e.stopPropagation();
  const b = e.currentTarget.getBoundingClientRect();
  const st = await call('steam_updates').catch(() => null);
  const items = [['Play without mods…', () => playVanilla(), '', 'Takes your mods out of the game folder (Purge) and starts the game itself, without the script extender, as when a game update broke it. Play puts them back.'], '-'];
  if (!st?.manifest) items.push(['Hold game updates on Steam', () => toast('rmm can’t find the game’s Steam manifest beside its folder, so this isn’t a Steam install it can hold updates for.'), 'muted', 'Not a Steam install rmm can find the manifest for']);
  else if (st.held) items.push(['Let Steam update the game', () => steamHold(false), '', 'Makes the game’s Steam manifest writable again']);
  else items.push(['Hold game updates on Steam…', () => steamHold(true), '', 'Makes the game’s Steam manifest read-only, so Steam can’t update the game and break the script extender']);
  items.push('-', ['Creation Club content…', creations, '', 'Make the game’s Creation Club content mods you can turn off, or put it back']);
  ctxMenu({ preventDefault() {}, clientX: b.right - 240, clientY: b.bottom + 4 }, items);
};

// ---- Profiles -------------------------------------------------------------------------------

/** A Desktop shortcut that switches to `profile` and starts the game, as MO2's do; Steam can start
 *  the same. It says what it would make first: nothing is written until Make the shortcut. */
async function profileShortcut(profile) {
  const plan = await act('Getting the shortcut ready', () => call('desktop_shortcut', { profile, dry_run: true }), { reload: false });
  if (!plan || plan === true) return;
  const field = v => `<input type="text" readonly spellcheck="false" value="${esc(v)}">`;
  const how = await dialog({
    title: 'Desktop shortcut',
    body: `<p>Makes “${esc(plan.path.split(/[\\/]/).pop())}” on your Desktop. It switches to “${esc(profile)}”, deploys and starts the game with its script extender, as Play does. If something stops it, its window stays open to say why. A shortcut of that name is replaced.</p>
      <p class="muted small">To start it from Steam instead: Games → Add a Non-Steam Game to My Library → Browse, pick the program below, then paste the launch options into its Properties. The game’s own Steam entry skips the script extender and anything not yet deployed.</p>
      <p class="small">Program</p>${field(plan.target)}<p class="small">Launch options</p>${field(plan.arguments)}`,
    actions: [['cancel', 'Cancel'], ['ok', 'Make the shortcut', 'primary']],
  });
  if (how !== 'ok') return;
  const r = await act('Making the shortcut', () => call('desktop_shortcut', { profile }), { reload: false });
  if (r && r !== true) toast(`“${r.path.split(/[\\/]/).pop()}” is on your Desktop.`);
}

$('#profile-btn').onclick = e => {
  e.stopPropagation();
  const pop = $('#profile-pop');
  if (!pop.hidden) { pop.hidden = true; return; }
  const cur = S.info.profile;
  pop.innerHTML = S.profiles.map(p => `<button class="item" data-switch="${esc(p)}"><span class="check">${p === cur ? '✓' : ''}</span>${esc(p)}${p === cur ? '' : `<span class="x" data-delete="${esc(p)}" title="Delete this profile">×</span>`}</button>`).join('')
    + '<hr><button class="item" data-new="playthrough" title="A copy of this profile with its own saves, for a new character"><span class="check"></span>New playthrough…</button>'
    + '<button class="item" data-new="copy"><span class="check"></span>Copy this profile…</button>'
    + '<button class="item" data-new="empty"><span class="check"></span>New empty profile…</button>'
    + '<button class="item" data-rename="1"><span class="check"></span>Rename this profile…</button>'
    + '<button class="item" data-shortcut="1" title="A shortcut on your Desktop that switches to this profile and starts the game, as Play does"><span class="check"></span>Desktop shortcut…</button>'
    + `<hr><button class="item" data-opt="local_inis" title="Deploy puts this profile's copies of the game's INI files in My Games; what the game changes there comes back to the profile. Purge puts yours back."><span class="check">${S.options?.local_inis ? '✓' : ''}</span>Own INI files</button>`
    + `<button class="item" data-opt="local_saves" title="Deploy points the game's saves at Saves\\${esc(cur)}\\. Purge puts your save folder back."><span class="check">${S.options?.local_saves ? '✓' : ''}</span>Own saves</button>`
    + '<hr><button class="item" data-io="mo2"><span class="check"></span>Import from MO2…</button>'
    + '<button class="item" data-io="vortex"><span class="check"></span>Import from Vortex…</button>'
    + '<button class="item" data-io="export" title="A lockfile, a pack with your own files, or a readable list"><span class="check"></span>Share this setup…</button>'
    + '<button class="item" data-io="diff"><span class="check"></span>Compare with a lockfile…</button>'
    + '<button class="item" data-io="apply"><span class="check"></span>Apply a lockfile…</button>';
  pop.hidden = false;
};
document.addEventListener('click', e => { if (!e.target.closest('.menu')) $('#profile-pop').hidden = true; if (!e.target.closest('#ctx')) closeCtx(); });

$('#profile-pop').onclick = async e => {
  const t = e.target.closest('[data-delete],[data-switch],[data-new],[data-rename],[data-io],[data-opt],[data-shortcut]');
  if (!t) return;
  $('#profile-pop').hidden = true;
  const cur = S.info.profile;
  if (t.dataset.io) return importExport(t.dataset.io);
  if (t.dataset.shortcut) return profileShortcut(cur);
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
  } else if (t.dataset.new === 'playthrough') {
    const name = await ask('New playthrough', `A copy of “${cur}”: the same mods, order and plugins, with saves of its own. From the next Play the game saves in a folder of its own under Saves, named after the profile, so this character’s saves don’t mix with the others. Your saves so far stay with “${cur}”; the Saves tab copies or moves saves between profiles.`, 'New playthrough');
    if (name && await act('Starting the playthrough', () => call('new_playthrough', { name }))) {
      toast(`Switched to ${name}. Play starts the game with its own saves.`);
    }
  } else if (t.dataset.new) {
    const copy = t.dataset.new === 'copy';
    const name = await ask(copy ? 'Copy this profile' : 'New empty profile',
      copy ? `A copy of “${cur}”: same mods, order and plugins.` : 'Every installed mod, disabled, except the script extender.', copy ? `${cur} copy` : '');
    if (name && await act('Creating the profile', async () => {
      await call('create_profile', { name, copy });
      await call('switch_profile', { name });
      const se = copy ? [] : await call('script_extender_mods');
      if (se.length) await call('enable_mods', { ids: se, enabled: true });
    })) {
      toast(`Switched to ${name}.`);
    }
  } else if (t.dataset.rename) {
    const to = await ask('Rename this profile', `New name for “${cur}”.`, cur);
    if (to && to !== cur) act('Renaming the profile', () => call('rename_profile', { from: cur, to }));
  }
};

// ---- Getting started: what a new setup needs before its first game ---------------------------

const WHY = {
  script_extender: 'Most mods need it.',
  address_library: 'Script extender plugins need it to work with your game version.',
  crash_logger: 'When the game crashes, it writes a log that names the mods behind it.',
};
let startRun = 0, gameVersion = null;
async function renderStart() {
  const card = $('#start-card'), run = ++startRun;
  const e = W.gettingStarted ? await call('essentials').catch(() => null) : null;
  if (run !== startRun) return;
  if (!e) return void (card.hidden = true);
  gameVersion = e.game_version;
  const u = nexus.user;
  // [done, what, a note, [action, button]]
  const steps = [
    [!!u, 'Sign in to Nexus Mods', u ? `as ${u.name}` : 'so rmm can download mods and check for updates.', !u && ['login', 'Sign in…']],
    [!!nexus.handler_here, 'Let the website’s “Mod manager download” buttons install here', nexus.handler_here ? '' : handlerNow(), !nexus.handler_here && ['handler', 'Set it up']],
    ...e.items.map(i => [!!i.have, i.name, i.have ? `in ${i.have}` : WHY[i.what], !i.have && i.nexus_id && [`get:${i.nexus_id}`, 'Get it…']]),
    [e.played, 'Start the game with ▶ Play', 'Play deploys your changes and starts the script extender; starting from Steam skips both.', false],
  ];
  const done = steps.filter(s => s[0]).length;
  card.hidden = done === steps.length;
  if (card.hidden) return;
  card.innerHTML = `<div class="start-head"><b>Getting started</b><span class="muted small">${done} of ${steps.length} done${W.simple ? ' · The Simple view hides the Data, Tools and AI tabs until you turn it off in Settings.' : ''}</span><div class="spacer"></div><button class="link" data-start="hide">Hide</button></div>
    <ul>${steps.map(([ok, what, note, b]) => `<li class="${ok ? 'ok' : ''}"><span class="tick">${ok ? '✓' : '○'}</span><span>${esc(what)} <span class="muted small">${esc(note)}</span></span>${b ? `<button data-start="${b[0]}">${esc(b[1])}</button>` : ''}</li>`).join('')}</ul>`;
}
$('#start-card').onclick = async e => {
  const act = e.target.closest('[data-start]')?.dataset.start;
  if (act === 'hide') {
    W.gettingStarted = false;
    savePrefs();
    $('#start-card').hidden = true;
    toast('Settings, under This window, shows the list again.');
  } else if (act === 'login') {
    nexusLogin('Sign in so rmm can download mods and check for updates with your account.', nexus.sso);
  } else if (act === 'handler') {
    await setHandler();
  } else if (act?.startsWith('get:')) {
    try { await call('open_nexus_mod', { mod_id: Number(act.slice(4)) }); } catch (err) { return toast(String(err), 'error'); }
    const pick = gameVersion ? `the file for game version ${gameVersion}` : 'the file for your game version';
    toast(`On its Files tab, click “Mod manager download” on ${pick}. ${nexus.handler_here ? 'It installs here.' : 'Set up the download buttons first, or download it and drop it on this window.'}`);
  }
};

// ---- Nexus ----------------------------------------------------------------------------------

/** Ask for the API key (the user pastes it; it is checked with Nexus and kept in their config
 *  folder, never in the instance). Resolves to true once a key works. */
async function nexusLogin(why, sso) {
  const body = `<p>${esc(why)}</p><p class="muted small">${sso ? 'Sign in on nexusmods.com and approve rmm there, or paste' : 'Paste'} your personal API key from your Nexus account’s API Keys page. rmm keeps it in your user folder, never in the instance or a lockfile.</p>
    <p><button type="button" class="link" id="nexus-keys">Open the API Keys page</button></p>
    <input id="nexus-key" type="password" autocomplete="off" spellcheck="false" placeholder="API key" aria-label="Nexus API key">`;
  const done = dialog({ title: 'Sign in to Nexus Mods', body, actions: [['cancel', 'Cancel'], ...(sso ? [['sso', 'Sign in on nexusmods.com']] : []), ['ok', 'Save key', 'primary']] });
  $('#nexus-keys').onclick = () => call('nexus_keys_page').catch(e => toast(`Couldn't open the browser.\n${e}`, 'error'));
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
  if (who) nexusShow().then(offerHandler);
  return !!who;
}

/** Who opens nxm:// links (“Mod manager download”) now, in a sentence. */
function handlerNow() {
  if (nexus.handler_here) return 'Nexus links open with rmm, into this instance.';
  if (!nexus.handler) return 'No program opens Nexus links now.';
  const exe = basename(nexus.handler.match(/^"?([^"]+)/)?.[1] ?? nexus.handler);
  const app = { 'nxmhandler.exe': 'Mod Organizer 2', 'vortex.exe': 'Vortex', 'rmm.exe': 'rmm, for another instance' }[exe.toLowerCase()] ?? exe;
  return `Nexus links open with ${app} now.`;
}

/** After signing in: offer to take the website's “Mod manager download” links. */
async function offerHandler() {
  if (nexus.handler_here || !nexus.user) return;
  const free = nexus.user.is_premium ? '' : ' That’s how free accounts download.';
  const v = await dialog({ title: 'Open Nexus links with rmm?', body: `<p>The “Mod manager download” buttons on nexusmods.com would then install into this instance, in this window (it opens if it’s closed), where installers show their options.${free}</p><p class="muted small">${esc(handlerNow())} The Nexus button can change this later.</p>`,
    actions: [['cancel', 'Not now'], ['ok', 'Open them with rmm', 'primary']] });
  if (v === 'ok') setHandler();
}

async function setHandler() {
  if (await act('Setting rmm to open Nexus links', () => call('nexus_handler'), { reload: false })) toast(`“Mod manager download” buttons on nexusmods.com now install into ${S.info.root}.`);
  nexusShow();
}

/** True once a Nexus API key is saved, asking for one if needed. */
async function nexusReady(why) {
  const st = await call('nexus_status').catch(() => ({ key: false }));
  return st.key || nexusLogin(why, st.sso);
}

/** The account button: who is signed in (asks Nexus, so only at start and after changes). */
let nexus = { key: false };
async function nexusShow() {
  nexus = await call('nexus_status').catch(() => ({ key: false }));
  if (nexus.categories_read) load(); // the Nexus category names just arrived
  const u = nexus.user;
  $('#nexus-btn').textContent = u ? `Nexus: ${u.name}${u.is_premium ? ' · Premium' : ''}` : nexus.key ? 'Nexus: sign in again' : 'Sign in to Nexus';
  $('#nexus-btn').classList.toggle('warn', nexus.key && !u);
  renderStart();
}

$('#nexus-btn').onclick = async () => {
  if (!nexus.user) {
    const why = nexus.error ? `Nexus didn't accept the saved key: ${nexus.error}` : 'Sign in so rmm can download mods and check for updates with your account. AI clients you connect use it too.';
    return nexusLogin(why, nexus.sso);
  }
  const u = nexus.user;
  const how = u.is_premium
    ? 'Premium: From Nexus… takes a mod page link or a mod id and downloads straight away, and so can AI clients.'
    : 'Free accounts download through the website: click “Mod manager download” on a file. Let rmm open those links and it installs the file here. Premium accounts can also download straight from a mod page link.';
  const v = await dialog({
    title: 'Nexus Mods',
    body: `<p>Signed in as <b>${esc(u.name)}</b>${u.is_premium ? ' (Premium)' : ''}.</p><p class="muted small">${esc(how)}</p><p class="muted small">${esc(handlerNow())}</p>`,
    actions: [['cancel', 'Close'], ['out', 'Sign out'], ...(nexus.handler_here ? [] : [['handler', 'Open Nexus links with rmm', 'primary']])],
  });
  if (v === 'out') {
    if (await act('Signing out of Nexus', () => call('nexus_forget_key'), { reload: false })) toast('Signed out of Nexus. The key is deleted from this computer.');
    nexusShow();
  } else if (v === 'handler') setHandler();
};

// Links from nexusmods.com's "Mod manager download" buttons, which `rmm nxm` leaves for this window:
// downloaded one at a time and installed like a dropped file (FOMOD options, the update offer).
// From Nexus… queues its link here too. A failed download stays on the Downloads tab with Retry,
// which carries on from what it got.
let nxmQueue = [], nxmRunning = false, nxmNow = null, nxmFailed = [];
async function takeLinks() {
  nxmQueue.push(...(await call('nxm_links').catch(() => [])).map(link => ({ link, at: null })));
  runQueue();
}
async function runQueue() {
  renderQueue();
  if (nxmRunning || !nxmQueue.length) return;
  nxmRunning = true;
  try {
    if (!await nexusReady('Downloads from Nexus Mods need your API key.')) return void (nxmQueue = []);
    while (nxmQueue.length) {
      while (busy) await new Promise(r => setTimeout(r, 400));
      nxmNow = nxmQueue.shift();
      renderQueue();
      let error = '';
      const f = await act('Downloading from Nexus', () => call('nexus_fetch', { link: nxmNow.link }).catch(e => { error = String(e); throw e; }), { reload: false, problem: true });
      const job = nxmNow;
      nxmNow = null;
      if (f && f !== true) {
        renderQueue();
        if (downloadsData) loadDownloads();
        await install([f.path], job.at, f);
      } else nxmFailed.push({ ...job, error });
    }
  } finally { nxmRunning = false; nxmNow = null; renderQueue(); }
}
/** A link as words: an nxm link's mod and file, or the link itself. */
function linkName(link) {
  const m = /^nxm:\/\/[^/]+\/mods\/(\d+)\/files\/(\d+)/i.exec(link);
  return m ? `Nexus mod ${m[1]}, file ${m[2]}` : link;
}
function renderQueue() {
  const rows = [];
  if (nxmNow) rows.push(`<div>Downloading ${esc(linkName(nxmNow.link))}…${nxmQueue.length ? ` <span class="muted small">${nxmQueue.length} more waiting</span>` : ''}</div>`);
  else if (nxmQueue.length) rows.push(`<div class="muted small">${plural(nxmQueue.length, 'download')} waiting.</div>`);
  nxmFailed.forEach((f, i) => rows.push(`<div><span class="pill bad">Failed</span> ${esc(linkName(f.link))} <span class="muted small">${esc(f.error)}</span> <button data-retry="${i}">Retry</button> <button class="link" data-drop="${i}">Remove</button></div>`));
  $('#dl-queue').innerHTML = rows.join('');
  $('#dl-queue').hidden = !rows.length;
  $('#n-downloads').classList.toggle('bad', nxmFailed.length > 0);
}
$('#dl-queue').onclick = e => {
  const retry = e.target.closest('[data-retry]')?.dataset.retry, drop = e.target.closest('[data-drop]')?.dataset.drop;
  if (retry !== undefined) nxmQueue.push(...nxmFailed.splice(+retry, 1));
  else if (drop !== undefined) nxmFailed.splice(+drop, 1);
  runQueue();
};
T.event.listen('nxm', () => takeLinks());

$('#nxm').onclick = async () => {
  const free = nexus.user && !nexus.user.is_premium;
  const link = await ask('Install from Nexus Mods', free
    ? 'Free accounts download from the website: on the mod’s Files tab, click “Mod manager download” (with rmm opening Nexus links, the file installs here). Or paste that button’s nxm:// link.'
    : 'Paste a mod page link or a mod id (Premium: downloads its newest main file), a collection’s link, or the nxm:// link of a file’s “Mod manager download” button.', '');
  if (!link || !await nexusReady('Downloads from Nexus Mods need your API key.')) return;
  if (link.includes('/collections/')) return installCollection(link);
  nxmQueue.push({ link, at: slotAfter(selIds().length === 1 ? selIds()[0] : null) });
  runQueue();
};

/** A Nexus collection: what it would install, then install it into a new profile (as Vortex
 *  does, so the user's setup stays as it was) or into this one. */
async function installCollection(link) {
  if (!nexus.user?.is_premium) return toast('Installing a collection downloads each of its files directly, which Nexus allows Premium accounts only. Its mods can still be installed one by one from the website.', 'error');
  const r = await act('Reading the collection', () => call('install_collection', { link, dry_run: true }), { reload: false, problem: true });
  if (!r || r === true) return;
  const list = (xs, n = 8) => xs.length ? `<ul class="small">${xs.slice(0, n).map(x => `<li>${esc(x)}</li>`).join('')}${xs.length > n ? `<li class="muted">and ${xs.length - n} more</li>` : ''}</ul>` : '';
  if (r.update_from != null) return updateCollection(link, r, list);
  const name = r.name.replace(/[<>:"/\\|?*\x00-\x1f]/g, '').trim().replace(/^\.+|\.+$/g, '').slice(0, 64) || 'Collection';
  const body = `<p>${plural(r.install.length, 'mod')} to download and install (${bytes(r.download_size)})${r.installed.length ? `; ${plural(r.installed.length, 'mod')} you have already are turned on` : ''}. Its mod rules, plugin list and tools come too, then LOOT sorts the plugins.</p>
    ${r.warnings.map(w => `<p class="small">${esc(w)}</p>`).join('')}
    ${r.game_folder.length ? `<p class="muted small">The collection puts these in the game folder: ${esc(r.game_folder.join(', '))}.</p>` : ''}
    ${r.optional.length ? `<label class="row"><input id="co-opt" type="checkbox"> Also its ${plural(r.optional.length, 'optional mod')}</label>${list(r.optional)}` : ''}
    <label class="row" style="margin-top:12px"><input id="co-new" type="checkbox" checked> Into a new profile, so your current setup stays as it is</label>
    <label class="field">New profile’s name <input id="co-name" type="text" value="${esc(name)}" spellcheck="false"></label>`;
  if (await dialog({ title: `Install ${r.name} (revision ${r.revision})`, body, actions: [['cancel', 'Cancel'], ['go', 'Install', 'primary']] }) !== 'go') return;
  const args = { link, revision: r.revision, optional: $('#co-opt')?.checked ?? false, profile: $('#co-new').checked ? $('#co-name').value.trim() : null };
  const done = await act(`Installing ${r.name}`, () => call('install_collection', args), { problem: true });
  if (!done || done === true) return;
  const part = (title, xs) => xs.length ? `<p><b>${esc(title)}</b></p>${list(xs, 20)}` : '';
  await dialog({
    title: `${done.name} is installed`,
    body: `<p>${plural(done.install.length - done.failed.length, 'mod')} installed${done.profile ? `, in the profile “${esc(done.profile)}”, now the active one` : ''}. Deploy or Play applies it to the game folder.</p>
      ${part('Couldn’t install', done.failed)}${part('The curator’s notes (each is also on its mod)', done.instructions)}${part('Tools added', done.tools)}${part('Warnings', done.warnings)}`,
    actions: [['ok', 'OK', 'primary']],
  });
}

/** The profile already has an earlier revision: update it, keeping the user's own changes. */
async function updateCollection(link, r, list) {
  if (r.revision === r.update_from && !r.install.length && !r.updated.length && !r.turned_off.length) return toast(`${r.name} is up to date (revision ${r.revision}).`);
  const body = `<p>${plural(r.updated.length, 'mod')} to update and ${plural(r.install.length, 'new mod')} to install (${bytes(r.download_size)}). Its mod rules and plugin list come too, then LOOT sorts the plugins.</p>
    ${r.turned_off.length ? `<p>To turn off, still installed: the revision dropped them, or has a newer file of them that goes in beside (another profile uses these).</p>${list(r.turned_off.map(nameOf))}` : ''}
    ${r.kept_off.length ? `<p class="muted small">You turned these off; they stay off: ${esc(r.kept_off.join(', '))}.</p>` : ''}
    ${r.yours.length ? `<p class="muted small">Your own ${plural(r.yours.length, 'mod')} stay as they are, and so do plugins you turned on or off.</p>` : ''}
    ${r.warnings.map(w => `<p class="small">${esc(w)}</p>`).join('')}`;
  if (await dialog({ title: `Update ${r.name} from revision ${r.update_from} to ${r.revision}`, body, actions: [['cancel', 'Cancel'], ['go', 'Update', 'primary']] }) !== 'go') return;
  const done = await act(`Updating ${r.name}`, () => call('install_collection', { link, revision: r.revision }), { problem: true });
  if (!done || done === true) return;
  const part = (title, xs) => xs.length ? `<p><b>${esc(title)}</b></p>${list(xs, 20)}` : '';
  await dialog({
    title: `${done.name} is at revision ${done.revision}`,
    body: `<p>Deploy or Play applies it to the game folder.</p>${part('Updated', done.updated)}${part('Installed', done.install)}${part('Turned off', done.turned_off.map(nameOf))}${part('Couldn’t install', done.failed)}${part('Warnings', done.warnings)}`,
    actions: [['ok', 'OK', 'primary']],
  });
}

/** A newer revision of the profile's collection: offer the update. */
async function checkCollection() {
  const c = S.collection;
  if (!c) return;
  const n = await call('collection_newest').catch(() => null);
  if (n > c.revision) toast(`${c.name} has a newer revision: ${n} (this profile has ${c.revision}).`, 'info', ['Update…', () => installCollection(c.slug)]);
}

$('#updates').onclick = () => checkUpdates(false);
/** Ask Nexus for newer files; quietly (at start) only says so when there are some. */
async function checkUpdates(quiet) {
  if (!quiet && !await nexusReady('Update checks ask Nexus Mods about your mods, which needs your API key.')) return;
  checkCollection();
  // The mod list's Update tags come with the reload.
  const r = quiet ? await call('check_updates').then(async u => (await load(), u)).catch(() => null) : await act('Checking for updates', () => call('check_updates'));
  if (!Array.isArray(r)) return;
  if (quiet) return r.length && toast(`${plural(r.length, 'update')} on Nexus; Check for updates lists them.`);
  const held = S.mods.filter(m => m.pinned).length;
  if (!r.length) return toast(`Every mod with Nexus ids is up to date${held ? ` (${plural(held, 'mod')} held at their versions)` : ''}.`);
  const rows = r.map(u => `<tr data-id="${esc(u.id)}"><td>${esc(nameOf(u.id))}</td><td>${esc(u.installed ?? '?')} → <b>${esc(u.latest ?? u.file_name)}</b></td><td><span class="small">${esc(u.url)}</span></td>
    <td style="white-space:nowrap"><button type="button" data-hold="skip" title="Don’t offer this file; a later one is offered">Skip</button> <button type="button" data-hold="pin" title="Keep the mod at its version until you release it in its panel">Hold</button></td></tr>`).join('');
  const done = dialog({ title: `${plural(r.length, 'update')} on Nexus`, body: `<table class="fc">${rows}</table><p class="muted small">Download a new file with its “Mod manager download” button (once nxm links open rmm), or use From Nexus…, or download it and drop it on the mod’s row. Skip passes over this file; Hold keeps the mod at its version.</p>`, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  let changed = false;
  $('#dlg').onclick = async e => {
    const b = e.target.closest('button[data-hold]');
    if (!b) return;
    try {
      await call('hold_updates', { ids: [b.closest('tr').dataset.id], hold: b.dataset.hold });
      changed = true;
      b.parentElement.textContent = b.dataset.hold === 'pin' ? 'Held' : 'Skipped';
    } catch (err) { toast(`Holding the update failed.\n${err}`, 'error'); }
  };
  await done;
  $('#dlg').onclick = null;
  if (changed) load();
}

// ---- Add-ons ----------------------------------------------------------------------------------

const CAPS = { read_instance: 'read your setup', modify_profile: 'change mod and plugin order and what is enabled', mod_files: 'read mod files and write to Overwrite', run_tools: 'run your tools', network: 'reach the internet' };
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
  const ok = await confirmBox(`Install ${a.name} ${a.version}?`, `It may ${capText(a)}.${a.hooks.length ? ` It runs ${hookText(a)}.` : ''} Only install add-ons from people you trust.`, 'Install', 'primary');
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
    const format = await dialog({ title: 'Share this setup', body: `<dl class="choices">
      <dt>Lockfile</dt><dd>Every mod’s download, installer choices and Nexus page, the order, the plugins, your notes, and the game and script extender versions. rmm rebuilds the setup from it with Apply a lockfile; the mods are downloaded again.</dd>
      <dt>Pack</dt><dd>A zip of the lockfile and the files of the mods no download brings back: patches you made, mods made from Overwrite or a folder.</dd>
      <dt>Readable list</dt><dd>Markdown for a post or a friend: the mods under their separators with Nexus links and notes, then the plugins.</dd></dl>`,
      actions: [['cancel', 'Cancel'], ['markdown', 'Readable list'], ['pack', 'Pack'], ['toml', 'Lockfile', 'primary']] });
    if (!['toml', 'pack', 'markdown'].includes(format)) return;
    const dir = await T.dialog.open({ directory: true, title: 'Where to save it' });
    if (!dir) return;
    const path = `${dir}\\${{ toml: 'modlist.lock.toml', pack: 'modlist.pack.zip', markdown: 'modlist.md' }[format]}`;
    const r = await act('Writing it', () => call('export_lockfile', { path, format }), { reload: false });
    if (r && r !== true) toast(`Saved ${r.path}${format === 'pack' ? ` with ${plural(r.packed.length, 'mod')} of your own` : ''}.`);
    return;
  }
  if (what === 'diff') {
    const path = await T.dialog.open({ title: 'A lockfile to compare with', filters: [{ name: 'rmm lockfile or pack', extensions: ['toml', 'zip'] }] });
    if (!path) return;
    const d = await act('Comparing', () => call('diff_lockfile', { path }), { reload: false });
    if (!d || d === true) return;
    const sec = (label, list) => list.length ? `<details><summary><b>${label}</b> (${list.length.toLocaleString()})</summary><pre>${esc(list.join('\n'))}</pre></details>` : '';
    const body = sec('Versions', d.versions) + sec('Mods added', d.added) + sec('Mods removed', d.removed)
      + sec('Mods with other files (another version or other installer choices)', d.changed) + sec('Mods turned on', d.turned_on) + sec('Mods turned off', d.turned_off)
      + sec('Mods moved', d.moved) + sec('Plugins added', d.plugins_added) + sec('Plugins removed', d.plugins_removed)
      + sec('Plugins turned on', d.plugins_on) + sec('Plugins turned off', d.plugins_off) + sec('Plugins moved', d.plugins_moved);
    return dialog({ title: `This profile and ${basename(path)}`, body: body || '<p>This profile matches the lockfile.</p>', actions: [['cancel', 'Close']] });
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
    // MO2: the instances found so far, and Auto search for portable ones anywhere.
    const game = GAMES[S.info.game] ?? S.info.game;
    const forGame = dirs => call('mo2_for_game', { dirs }).catch(() => dirs);
    let mo2 = what === 'mo2' ? await forGame(src.mo2) : [];
    while (what === 'mo2') {
      const body = mo2.length
        ? `<p>MO2 instances for ${esc(game)} on this PC:</p><select id="mo2-dir" aria-label="MO2 instance">${mo2.map(p => `<option value="${esc(p)}">${esc(p)}</option>`).join('')}</select>`
        : `<p>No MO2 instance for ${esc(game)} in the usual place (AppData\\Local\\ModOrganizer). Auto search looks through every drive, for portable ones too.</p>`;
      const v = await dialog({ title: 'Import from MO2', body, actions: [['cancel', 'Cancel'], ['search', 'Auto search all drives'], ['pick', 'Choose a folder…'], ...(mo2.length ? [['ok', 'Next', 'primary']] : [])] });
      if (v === 'ok') path = $('#mo2-dir').value;
      if (v === 'ok' || v === 'pick') break;
      if (v !== 'search') return;
      const r = await act('Searching every drive', () => call('search_drives'), { reload: false });
      if (!r || r === true) continue;
      const more = (await forGame(r.mo2)).filter(p => !mo2.some(q => samePath(p, q)));
      if (!more.length) toast(`No other MO2 instances for ${game} on any drive.`);
      mo2 = [...more, ...mo2];
    }
    path ??= what === 'mo2'
      ? await T.dialog.open({ directory: true, title: 'The MO2 instance folder (with ModOrganizer.ini)', defaultPath: src.mo2[0] })
      : await T.dialog.open({ title: 'A Vortex state backup', filters: [{ name: 'Vortex backup', extensions: ['json'] }], defaultPath: src.vortex.find(p => p.endsWith('.json')) });
    if (!path) return;
    const args = what === 'mo2' ? { dir: path } : { path };
    const profiles = what === 'mo2' ? await call('mo2_profiles', { dir: path }).catch(() => []) : [];
    if (profiles.length > 1) {
      const body = `<p>Which MO2 profile? Its mods come in with their order, and its plugin order.</p><select id="mo2-profile" aria-label="MO2 profile">${profiles.map((p, i) => `<option value="${esc(p)}">${esc(p)}${i ? '' : ' (selected in MO2)'}</option>`).join('')}<option value="*">All ${profiles.length} profiles, each into an rmm profile of its name</option></select>`;
      if (await dialog({ title: 'Import from MO2', body, actions: [['cancel', 'Cancel'], ['ok', 'Next', 'primary']] }) !== 'ok') return;
      if ($('#mo2-profile').value === '*') args.all = true;
      else args.profile = $('#mo2-profile').value;
    }
    const run = dry_run => call(what === 'mo2' ? 'import_mo2' : 'import_vortex', { ...args, dry_run });
    const r = await act('Reading the setup', () => run(true), { reload: false });
    if (!r || r === true) return;
    // One report, or one per MO2 profile; mods and tools come in with the first.
    const all = [].concat(r), first = all[0];
    const sum = k => all.reduce((n, x) => n + (x[k] ?? 0), 0);
    const warnings = [...new Set(all.flatMap(x => x.warnings))];
    const into = args.all
      ? `<p>${plural(all.length, 'MO2 profile')}, each into the rmm profile of its name (${all.map(x => esc(x.into)).join(', ')}; missing ones are made): ${plural(first.mods.length, 'mod')} to bring in, ${plural(first.separators, 'separator')}.</p>`
      : `<p>${what === 'mo2' ? 'MO2' : 'Vortex'} profile <b>${esc(r.profile)}</b>: ${plural(r.mods.length, 'mod')} to bring in, ${plural(r.separators, 'separator')}, ${plural(r.plugins, 'plugin')}${what === 'vortex' ? `, ${plural(r.rules, 'mod rule')}` : ''}.</p>`;
    const own = [sum('inis') && `${plural(sum('inis'), 'INI')} into the profile’s own`, sum('saves') && `${plural(sum('saves'), 'save file')} into its own save folder`].filter(Boolean);
    const body = into + (own.length ? `<p>MO2 kept INIs and saves per profile, and so will rmm: ${own.join(', and ')} are copied in.</p>` : '')
      + `<p class="muted small">Files are hard-linked when they’re on the same drive (no extra space), copied otherwise. The list becomes ${args.all ? 'each' : 'this'} profile’s order in one step that Undo reverts; mods already here stay as they are.${what === 'vortex' ? ' Purge Vortex’s deployment before deploying with rmm.' : ''}</p>`
      + part('to bring in', first.mods) + part('already installed, kept as they are', first.existing)
      + part('MO2 programs that become tools', first.tools ?? []) + part('made by a program, replaced when you run it in rmm', first.outputs ?? [])
      + part('warnings', warnings);
    if (await dialog({ title: `Import from ${what === 'mo2' ? 'MO2' : 'Vortex'}`, body, actions: [['cancel', 'Cancel'], ['ok', 'Import', 'primary']] }) !== 'ok') return;
    const done = await act('Importing', () => run(false));
    if (!done || done === true) return;
    const res = [].concat(done), n = k => res.reduce((t, x) => t + (x[k]?.length ?? x[k] ?? 0), 0);
    const notes = [...new Set(res.flatMap(x => x.warnings))];
    if (n('hidden')) notes.push(`${plural(n('hidden'), 'file')} MO2 had hidden came in hidden too (Hide files… in a mod’s panel shows them).`);
    toast(`Imported ${plural(n('mods'), 'mod')}${args.all ? ` into ${plural(res.length, 'profile')}` : ''} (${n('linked').toLocaleString()} files linked, ${n('copied').toLocaleString()} copied)${n('tools') ? `, and ${plural(n('tools'), 'tool')}` : ''}.${notes.length ? '\n' + notes.join('\n') : ''}`, notes.length ? 'warn' : 'info');
    if (n('tools')) loadTools();
    return;
  }
  // Apply a lockfile.
  const path = await T.dialog.open({ title: 'A lockfile', filters: [{ name: 'rmm lockfile or pack', extensions: ['toml', 'zip'] }] });
  if (!path) return;
  const dl = await T.dialog.open({ directory: true, title: 'Where the mod archives are (cancel to skip installing)' });
  const downloads = dl ? [dl] : [];
  const run = dry_run => call('apply_lockfile', { path, downloads, dry_run });
  const r = await act('Reading the lockfile', () => run(true), { reload: false });
  if (!r || r === true) return;
  const missing = r.missing.map(m => `${m.name ?? m.id}${m.tool ? ` (made by the tool “${m.tool}”)` : m.archive ? ` (${m.archive})` : ''}${m.url ? `\n    ${m.url}` : ''}`);
  const body = `<p>${r.ok.toLocaleString()} mods already match. ${plural(r.install.length, 'mod')} to install from the archives${/\.zip$/i.test(path) ? ' and the pack' : ''}.</p>`
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
      : (last.edit_in_place ? (last.files ? `${plural(last.files, 'file')} saved into mods` : 'Changed nothing')
        : last.mod_id ? `${plural(last.files, 'file')} captured` : 'Captured nothing')
        + (last.timed_out ? ' · timed out' : last.exit_code ? ` · exit code ${last.exit_code}` : '')
        + (last.warnings?.length ? ` · <span title="${esc(last.warnings.join('\n'))}">⚠ ${last.warnings.length}</span>` : '');
    return `<tr data-tool="${esc(id)}">
      <td class="name"><b>${esc(tool.name)}</b> <span class="muted small">${esc(id)}</span><div class="muted small" title="${esc(tool.exe)}">${esc(tool.exe)}</div></td>
      <td>${tool.edit_in_place ? `<span title="Files it changes go back to the mods they came from; new files go to ${esc(out)}">Edits in place · ${esc(out)}</span>` : esc(out)}</td><td>${how}</td>
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
    <label class="field">Output mod<input id="t-out" list="mod-names" spellcheck="false" placeholder="Where new files go (default: “Name Output”)"></label>
    <label class="row" style="margin:0"><input type="checkbox" id="t-inplace"> Save edits into the mods they change, like xEdit patching (new files go to the output mod, which can be one you have)</label>
    <p class="muted small">Placeholders: {data} {game} {plugins} {ini} {ini_dir} {output} {state}; Run asks for any other name in braces. A preset fills in its own arguments.</p>`;
  const d = $('#dlg');
  const wait = dialog({ title: 'Add a tool', body, actions: [['cancel', 'Cancel'], ['ok', 'Add', 'primary']] });
  const preset = $('#t-preset', d), id = $('#t-id', d);
  preset.onchange = () => {
    $('#t-args-f', d).hidden = !!preset.value;
    $('#t-inplace', d).checked = preset.value === 'xedit';
    if (preset.value && !id.value) id.value = preset.value;
  };
  $('#t-browse', d).onclick = async () => {
    const exe = await T.dialog.open({ title: 'Choose the program' });
    if (exe) { $('#t-exe', d).value = exe; if (!id.value) id.value = basename(exe).replace(/\.[^.]+$/, '').toLowerCase().replace(/[^a-z0-9-]+/g, '-'); }
  };
  if (await wait !== 'ok') return;
  const spec = { exe: $('#t-exe', d).value.trim(), preset: preset.value || null, name: $('#t-name', d).value.trim() || null, output_mod: $('#t-out', d).value.trim() || null, edit_in_place: $('#t-inplace', d).checked };
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
  const what = plan.edit_in_place
    ? `rmm deploys and runs the tool. Files it changes go back into the mods they came from, and new files into <b>${esc(plan.previous ? nameOf(plan.previous) : plan.output_mod)}</b>${plan.previous ? '' : ' (a new mod)'}. Then it deploys again. Undo puts those mods back as they were.`
    : `rmm deploys, runs the tool, and captures what it writes into <b>${esc(plan.output_mod)}</b>${plan.previous ? `, replacing ${esc(nameOf(plan.previous))}` : ''}. Then it deploys again. Undo brings the previous output back.`;
  const body = `<div class="report"><p>${what}</p>
    <pre>${esc(plan.command.join(' '))}</pre>
    ${list('deployed files swapped for copies while it runs', plan.swapped)}
    ${plan.deploy ? `<p class="muted small">The first deploy: ${plural(plan.deploy.link.length, 'file')} to link, ${plural(plan.deploy.remove.length, 'file')} to remove.</p>` : ''}
    ${plan.warnings.length ? `<p>⚠ ${esc(plan.warnings.join(' · '))}</p>` : ''}
    <p class="muted small">The window waits until the tool closes.</p></div>`;
  if (await dialog({ title: `Run ${tool.name}`, body, actions: [['cancel', 'Cancel'], ['run', 'Run', 'primary']] }) !== 'run') return;
  const r = await act(`Running ${tool.name}`, () => call('run_tool', { id, vars, dry_run: false }));
  if (!r) return;
  loadTools();
  const head = r.edit_in_place
    ? (r.files ? `Saved ${plural(r.files, 'file')} into mods${r.mod_id ? `; new files are in <b>${esc(nameOf(r.mod_id))}</b>` : ''}.` : 'The tool changed nothing.')
    : r.mod_id ? `Captured ${plural(r.files, 'file')} into <b>${esc(nameOf(r.mod_id))}</b>.` : 'The tool wrote nothing rmm could capture.';
  const through = r.edit_in_place
    ? `It wrote straight into mod files through their links: ${esc(r.modified_in_place.join(', '))}. Undo can’t bring back their old contents.`
    : `It rewrote mod files through their links: ${esc(r.modified_in_place.join(', '))}. Reinstall those mods.`;
  const out = `<div class="report"><p>${head}
    ${r.timed_out ? ' It timed out.' : r.exit_code ? ` It exited with code ${r.exit_code}.` : ''}</p>
    ${list('saved back into their mods', r.into_mods)}${list('new', r.new)}${list('changed', r.changed)}${list('deleted', r.deleted)}
    ${r.modified_in_place.length ? `<p>⚠ ${through}</p>` : ''}
    ${r.warnings.length ? `<p>⚠ ${esc(r.warnings.join(' · '))}</p>` : ''}
    ${r.log_tail ? `<details><summary>Log</summary><pre>${esc(r.log_tail)}</pre></details>` : ''}</div>`;
  dialog({ title: `${tool.name} finished`, body: out, actions: [['cancel', 'Close']] });
}

// ---- Data, Saves, Downloads -------------------------------------------------------------------

let dataDir = '';            // the folder of Data shown ('' = Data itself)
let dataLoaded = false;      // the Data list matches the mods; load() resets it
let dataRun = 0, dataTimer = 0;
let savesData = null;        // { dir, saves }
let downloadsData = null;    // downloads, newest first

const OVER = '<overwrite>';
const srcName = id => id === OVER ? 'Overwrite' : nameOf(id);
const srcLink = id => id === OVER ? 'Overwrite' : `<button class="link" data-goto="${esc(id)}">${esc(nameOf(id))}</button>`;
const bytes = n => n >= 2 ** 30 ? `${(n / 2 ** 30).toFixed(1)} GB` : n >= 2 ** 20 ? `${(n / 2 ** 20).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`;
const finding = () => $('#data-find').value.trim();

function dataRow(e, i) {
  if (e.dir) {
    return `<tr data-i="${i}" class="dir"><td class="name" title="${esc(e.name)}">${esc(e.name)}${e.files == null ? '' : ` <span class="muted small">${plural(e.files, 'file')}</span>`}</td>
      <td class="muted">${e.files == null ? 'Not from a mod' : ''}</td><td></td></tr>`;
  }
  const mods = e.mods ?? []; // none: the game's own file
  const win = mods.at(-1), rest = mods.slice(0, -1).reverse().map(srcName).join(', ');
  const from = !win ? '<span class="muted">Not from a mod</span>'
    : srcLink(win) + (e.archive ? ` <span class="muted small">in ${esc(e.archive)}</span>` : '');
  return `<tr data-i="${i}"${finding() ? ' class="found" title="Open its folder"' : ''}>
    <td class="name" title="${esc(e.name)}">${esc(e.name)}</td><td class="name">${from}</td>
    <td class="muted" title="${esc(rest)}">${esc(rest)}</td></tr>`;
}

/** The shown folder of Data (or the search), with who provides each file. */
async function loadData(top = true) {
  const run = ++dataRun, find = finding();
  dataLoaded = true;
  let rows;
  try {
    rows = await call('data', { dir: dataDir, find, conflicts_only: $('#data-conflicts').checked, archives: $('#data-archives').checked });
  } catch (e) {
    rows = [];
    toast(`Couldn’t read the Data folder.\n${e}`, 'error');
  }
  if (run !== dataRun) return; // a newer request is on its way
  const parts = dataDir ? dataDir.split('/') : [];
  $('#data-path').innerHTML = find
    ? `<span class="muted">${!rows.length ? 'Search results' : `${rows.length >= 1000 ? 'The first 1,000 matches' : `${rows.length.toLocaleString()} match${rows.length === 1 ? '' : 'es'}`}. Click one to open its folder.`}</span>`
    : [parts.length ? '<button class="link" data-dir="">Data</button>' : '<b>Data</b>',
      ...parts.map((p, i) => i === parts.length - 1 ? `<b>${esc(p)}</b>` : `<button class="link" data-dir="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</button>`)]
      .join('<span class="muted">/</span>');
  lists.data.items = rows;
  if (top) scroller(lists.data).scrollTop = 0;
  $('#data-empty').hidden = rows.length > 0;
  $('#data-empty').textContent = find ? 'No file matches.' : $('#data-conflicts').checked ? 'No conflicts here.' : 'Nothing here.';
  paint(lists.data);
}

const openDir = dir => { dataDir = dir; $('#data-find').value = ''; loadData(); };
$('#data-path').onclick = e => { const b = e.target.closest('[data-dir]'); if (b) openDir(b.dataset.dir); };
$('#data tbody').addEventListener('click', e => {
  const id = e.target.closest('[data-goto]')?.dataset.goto;
  if (id) { showTab('mods'); return goTo(id); }
  const row = lists.data.items[e.target.closest('tr[data-i]')?.dataset.i];
  if (row?.dir) openDir(dataDir ? `${dataDir}/${row.name}` : row.name);
  else if (row && finding()) openDir(row.name.split('/').slice(0, -1).join('/'));
});
$('#data-find').oninput = () => { clearTimeout(dataTimer); dataTimer = setTimeout(loadData, 250); };
$('#data-conflicts').onchange = $('#data-archives').onchange = () => loadData();
$('#data-find').onkeydown = e => { if (e.key === 'Escape' && finding()) { e.stopPropagation(); openDir(dataDir); } };

async function loadSaves() {
  if (!savesData) $('#saves-note').textContent = 'Reading the saves…';
  try { savesData = await call('saves'); } catch (e) { savesData = { dir: null, saves: [], error: String(e) }; }
  renderSaves();
}

function renderSaves() {
  const { dir, saves, error } = savesData;
  const short = saves.filter(s => s.missing.length).length;
  $('#n-saves').textContent = saves.length || '';
  $('#saves-note').textContent = error ? `Couldn’t read the saves: ${error}`
    : !dir ? 'The game’s My Games folder isn’t known for this instance.'
    : `${plural(saves.length, 'save')} in ${dir}${short ? ` · ${short} use plugins this profile doesn’t load` : ''}`;
  $('#saves tbody').innerHTML = saves.map(s => `<tr data-file="${esc(s.file)}">
    <td class="name">${s.error ? `<span class="pill bad" title="${esc(s.error)}">Can’t read it</span>` : esc(s.player || '(no name yet)')}</td>
    <td class="num">${s.level || ''}</td>
    <td class="name" title="${esc(s.location)}">${esc(s.location)}</td>
    <td class="muted">${esc(when(s.modified))}</td>
    <td>${s.error ? '' : s.missing.length ? `<button class="pill bad" data-missing title="Show them">${s.missing.length} not loaded</button>` : `<span class="muted">All ${s.plugins} loaded</span>`}</td>
    <td class="muted" title="${esc(s.file)}">${esc(s.file)}</td></tr>`).join('');
  $('#saves-empty').hidden = saves.length > 0;
  $('#saves-empty').textContent = dir ? 'No saves yet for this profile.' : '';
}

/** A save's plugins the profile doesn't load, and the button that enables the installed ones. */
async function savePlugins(file) {
  const s = savesData.saves.find(x => x.file === file);
  const fixable = s.missing.filter(m => m.mod_id).length;
  const rows = s.missing.map(m => `<tr><td>${esc(m.plugin)}</td><td>${m.mod_id ? modLink(m.mod_id) : '<span class="muted">Not installed</span>'}</td></tr>`).join('');
  const body = `<p>${esc(s.player || file)} was saved with ${plural(s.missing.length, 'plugin')} this profile doesn’t load. Playing the save without them drops what they added and can break it.</p>
    <table class="fc">${rows}</table>${fixable ? '' : '<p class="muted small">None of them is installed; install them from where they came from.</p>'}`;
  const actions = [['cancel', 'Close']];
  if (fixable) actions.push(['fix', `Enable ${plural(fixable, 'plugin')}`, 'primary']);
  const done = dialog({ title: 'Plugins the save needs', body, actions });
  $('#dlg').onclick = e => { const id = e.target.closest('[data-goto]')?.dataset.goto; if (id) { $('#dlg').close(); showTab('mods'); goTo(id); } };
  const r = await done;
  $('#dlg').onclick = null;
  if (r !== 'fix') return;
  const on = await act('Enabling the plugins', () => call('enable_save_plugins', { file }));
  if (Array.isArray(on)) toast(`Enabled ${on.join(', ')}. Deploy or Play to use them.`);
}

$('#saves tbody').addEventListener('click', e => {
  const file = e.target.closest('tr[data-file]')?.dataset.file;
  if (file && e.target.closest('[data-missing]')) savePlugins(file);
});
$('#saves-reload').onclick = loadSaves;

$('#saves-transfer').onclick = async () => {
  const cur = S.info.profile, others = S.profiles.filter(p => p !== cur), saves = savesData?.saves ?? [];
  if (!others.length) return toast('There’s no other profile yet. New playthrough… in the profile menu makes one with saves of its own.');
  if (!saves.length) return toast('This profile has no saves to copy or move.');
  const row = s => `<label class="row" style="margin:0" title="${esc(s.file)}"><input type="checkbox" value="${esc(s.file)}"> ${esc(s.player || s.file)}${s.level ? `, level ${s.level}` : ''}${s.location ? `, ${esc(s.location)}` : ''} <span class="muted small">${esc(when(s.modified))}</span></label>`;
  const body = `<label class="field">To the profile<select id="st-to">${others.map(p => `<option value="${esc(p)}">${esc(p)}</option>`).join('')}</select></label>
    <label class="row" style="margin:12px 0 4px"><input type="checkbox" id="st-all"> All ${plural(saves.length, 'save')}</label>
    <div class="stack" id="st-saves" style="max-height:40vh;overflow:auto">${saves.map(row).join('')}</div>
    <p class="muted small" style="margin-top:10px">Each save goes with its script extender co-save. A save the other profile already has stops it.</p>`;
  const done = dialog({ title: 'Copy or move saves', body, actions: [['cancel', 'Cancel'], ['move', 'Move'], ['copy', 'Copy', 'primary']] });
  $('#st-all').onchange = e => $$('#st-saves input').forEach(c => { c.checked = e.target.checked; });
  const how = await done;
  const files = $$('#st-saves input:checked').map(c => c.value), profile = $('#st-to').value;
  if (!how) return;
  if (!files.length) return toast('No saves were ticked.');
  const r = await act(how === 'move' ? 'Moving the saves' : 'Copying the saves', () => call('transfer_saves', { files, profile, move: how === 'move' }), { reload: false });
  if (!r) return;
  toast(`${how === 'move' ? 'Moved' : 'Copied'} ${plural(files.length, 'save')} to ${profile} (${r.to}).`);
  loadSaves();
};

async function loadDownloads() {
  try { downloadsData = await call('downloads'); } catch (e) { downloadsData = []; toast(`Couldn’t read the downloads.\n${e}`, 'error'); }
  renderDownloads();
}

function renderDownloads() {
  const l = downloadsData;
  for (const n of dlSel) if (!l.some(d => d.name === n)) dlSel.delete(n);
  $('#n-downloads').textContent = l.length || '';
  $('#downloads tbody').innerHTML = l.map(d => `<tr data-name="${esc(d.name)}"${dlSel.has(d.name) ? ' class="sel"' : ''}>
    <td class="check-col"><input type="checkbox" data-pick ${dlSel.has(d.name) ? 'checked' : ''}></td>
    <td class="name" title="${esc(d.name)}">${esc(d.name)}</td>
    <td class="name">${d.nexus ? `<button class="link" data-page="${d.nexus.nexus.mod_id}" title="Open its Files tab on Nexus">${esc(d.nexus.name ?? `mod ${d.nexus.nexus.mod_id}`)}</button>${d.nexus.nexus.version ? ` <span class="muted small">${esc(d.nexus.nexus.version)}</span>` : ''}` : '<span class="muted">—</span>'}</td>
    <td class="name">${d.mods.length ? d.mods.map(srcLink).join(', ') : '<span class="muted">Not installed</span>'}</td>
    <td class="num">${bytes(d.size)}</td><td class="muted">${esc(when(d.modified))}</td>
    <td class="run-col">${d.mods.length ? '<button data-reinstall title="Install it again over the mod made from it">Reinstall…</button>' : '<button data-install class="primary">Install</button>'} <button data-delete class="link">Delete</button></td></tr>`).join('');
  $('#downloads-empty').hidden = l.length > 0;
  $('#dl-all').checked = l.length > 0 && dlSel.size === l.length;
  const sel = selDownloads();
  $('#dl-install').hidden = $('#dl-delete').hidden = !sel.length;
  $('#dl-install').textContent = `Install ${plural(sel.length, 'file')}`;
  $('#dl-delete').textContent = `Delete ${plural(sel.length, 'file')}…`;
  const unknown = (sel.length ? sel : l).filter(d => !d.nexus).length;
  $('#dl-identify').disabled = !unknown;
  $('#dl-identify').textContent = unknown ? `Identify ${plural(unknown, 'file')} on Nexus` : 'Identify on Nexus';
}
const dlSel = new Set();
const selDownloads = () => (downloadsData ?? []).filter(d => dlSel.has(d.name));
/** What install() records about a download from Nexus. */
const nxOf = d => d.nexus ? { name: d.nexus.name, nexus: d.nexus.nexus } : null;

$('#downloads tbody').addEventListener('click', async e => {
  const id = e.target.closest('[data-goto]')?.dataset.goto;
  if (id) { showTab('mods'); return goTo(id); }
  const d = downloadsData.find(x => x.name === e.target.closest('tr[data-name]')?.dataset.name);
  if (!d) return;
  const page = e.target.closest('[data-page]')?.dataset.page;
  if (page) return call('open_nexus_mod', { mod_id: Number(page) }).catch(err => toast(String(err), 'error'));
  if (e.target.closest('[data-pick]')) {
    if (e.target.checked) dlSel.add(d.name); else dlSel.delete(d.name);
    return renderDownloads();
  }
  if (e.target.closest('[data-install]')) install([{ path: d.path, nx: nxOf(d) }]);
  else if (e.target.closest('[data-reinstall]')) reinstall(d.mods[0]);
  else if (e.target.closest('[data-delete]')) {
    const left = d.mods.length ? ' The mods installed from it stay, but Reinstall can’t use it any more.' : '';
    if (!await confirmBox(`Delete ${d.name}?`, `The file is deleted for good.${left}`, 'Delete')) return;
    if (await act('Deleting the download', () => call('delete_download', { name: d.name }), { reload: false })) loadDownloads();
  }
});
$('#downloads-reload').onclick = loadDownloads;
$('#dl-all').onchange = e => {
  dlSel.clear();
  if (e.target.checked) for (const d of downloadsData ?? []) dlSel.add(d.name);
  renderDownloads();
};
$('#dl-install').onclick = () => {
  const sel = selDownloads();
  dlSel.clear();
  renderDownloads();
  install(sel.map(d => ({ path: d.path, nx: nxOf(d) })));
};
$('#dl-delete').onclick = async () => {
  const sel = selDownloads(), used = sel.filter(d => d.mods.length).length;
  if (!await confirmBox(`Delete ${plural(sel.length, 'file')}?`, `They’re deleted for good.${used ? ` ${used} of them ${used === 1 ? 'has' : 'have'} installed mods, which stay but can’t be reinstalled from them.` : ''}`, 'Delete')) return;
  await act('Deleting the downloads', async () => { for (const d of sel) await call('delete_download', { name: d.name }); }, { reload: false });
  dlSel.clear();
  loadDownloads();
};
$('#dl-identify').onclick = async () => {
  const sel = selDownloads();
  const names = (sel.length ? sel : downloadsData ?? []).filter(d => !d.nexus).map(d => d.name);
  if (!names.length || !await nexusReady('Identifying files on Nexus needs your API key.')) return;
  const r = await act('Identifying downloads on Nexus', () => call('identify_downloads', { names }), { reload: false });
  if (!r || r === true) return;
  const lines = [`Found ${r.found.length} of ${plural(names.length, 'file')} on Nexus.`];
  if (r.not_found.length) lines.push(`${plural(r.not_found.length, 'file')} Nexus doesn’t know (not from Nexus, or changed since): ${r.not_found.join(', ')}`);
  if (r.failed.length) lines.push(`${plural(r.failed.length, 'file')} couldn’t be looked up: ${r.failed[0][1]}`);
  if (r.mods.length) lines.push(`${plural(r.mods.length, 'installed mod')} got ${r.mods.length === 1 ? 'its' : 'their'} Nexus ids, so update checks see ${r.mods.length === 1 ? 'it' : 'them'}.`);
  toast(lines.join('\n'), r.failed.length ? 'warn' : 'info');
  loadDownloads();
  if (r.mods.length) load();
};

// ---- Install --------------------------------------------------------------------------------

$('#install').onclick = async () => {
  const picked = await T.dialog.open({ multiple: true, title: 'Install mods', filters: [{ name: 'Mod archives', extensions: ['zip', '7z', 'rar'] }] });
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
async function install(paths, at = null, nx0 = null) {
  let plugins = 0;
  for (const item of paths) {
    // A path, or { path, nx } with that file's Nexus details.
    const { path, nx } = typeof item === 'string' ? { path: item, nx: nx0 } : item;
    // Another version of an installed mod (the same Nexus mod): offer to update it in place.
    const same = await call('same_mod', { path, mod_id: nx?.nexus?.mod_id ?? null }).catch(() => null);
    if (same) {
      const [id, name] = same;
      const v = await dialog({ title: `Update ${name}?`, body: `<p><b>${esc(basename(path))}</b> looks like another version of <b>${esc(name)}</b>: the same Nexus mod.</p><p class="muted small">Update replaces its files and keeps its place, notes, categories, colour and rules; Undo puts the old files back. Install as a new mod keeps both.</p>`,
        actions: [['cancel', 'Cancel'], ['new', 'Install as a new mod'], ['ok', 'Update', 'primary']] });
      if (v === 'ok') await updateMod(id, path, nx);
      if (v !== 'new') continue;
    }
    // Unpacks the archive once; the install below reuses it.
    const walk = await act(`Reading ${basename(path)}`, () => call('fomod', { path }), { reload: false, problem: true });
    if (!walk) continue;
    let choices, name = nx?.name ?? undefined;
    if (walk !== true) { // it has a FOMOD installer, which names the mod better than most file names
      choices = walk.steps.length ? await fomodWizard(path, walk) : walk.choices;
      if (!choices) continue;
      name = walk.module;
    }
    const r = await act(`Installing ${basename(path)}`, () => call('install', { path, choices, name, priority: at, nexus: nx?.nexus }), { problem: true });
    if (!r) continue;
    if (at !== null) at++;
    plugins += r.plugins.length;
    const notes = [r.plugins.length ? `Plugins: ${r.plugins.join(', ')}` : '', ...r.warnings].filter(Boolean);
    toast(`Installed ${r.name} (${plural(r.files, 'file')}).${notes.length ? '\n' + notes.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
  }
  if (plugins) offerLoot(plugins);
}

/** New plugins load last; offer LOOT's order, which shows what moves before anything changes. */
function offerLoot(n) {
  toast(`${n === 1 ? 'The new plugin loads' : `The ${n} new plugins load`} last. LOOT can put ${n === 1 ? 'it' : 'them'} where ${n === 1 ? 'it belongs' : 'they belong'}; you see what moves first.`,
    'info', ['Sort with LOOT…', () => $('#loot').click()]);
}

T.webview.getCurrentWebview().onDragDropEvent(async e => {
  const type = e.payload.type;
  if (tab === 'ai') return aiDrop(e.payload);
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
    const paths = type === 'drop' && S ? e.payload.paths ?? [] : [];
    if (paths.length && (!W.confirmDrop || await confirmBox('Install', `Install ${paths.map(basename).join(', ')}?`, 'Install', 'primary'))) install(paths, at);
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
      <div class="stepline muted small">Step ${at + 1} of ${w.steps.length}${step.name.trim() ? ` · <b>${esc(step.name)}</b>` : ''}</div>
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

// ---- AI tab: the IDE ---------------------------------------------------------------------------
// Files (mods, overwrite/, the game's INIs) with an editor, saved chats, and rmm's own AI
// (agent.rs in the rmm crate) with the user's API key. The AI works through the same tools AI
// clients get over MCP: reads and dry runs run at once, each change waits for Allow here, and
// edit_file shows as a diff. The Inspector shows each request, its tokens and cost.

const aiCall = (name, args = {}) => T.core.invoke('ai', { name, args });
const ROLES = { find_mods: 'Researcher', install_safely: 'Installer', test_mods: 'Tester', mod_team: 'Lead' };
const PICTURE = /\.(png|jpe?g|gif|webp)$/i;
let aiProviders = null;     // status of each provider, once the tab was opened
let aiRunning = false;
let aiChat = null;          // { id, provider, model, title, app } of the chat on screen; null: a new one
let aiPics = [];            // attached pictures: data: URLs (picked) or file paths (dropped)
let aiLastCall = null;      // { el, name, args } of the tool call running now
let aiExchanges = [];       // requests of the chat on screen, since it was opened here
let aiCtx = null;           // { path, from, to, text }: editor context for the next message
let ideReady = false;        // the instance root the tab was filled for
const ide = { files: [], cur: null }; // open files: { path, text, saved, hash, editable, note }

const num = n => (n ?? 0).toLocaleString();
const tokens = u => `${num(u.input + u.cache_read + u.cache_write)} in${u.cache_read ? ` (${num(u.cache_read)} cached)` : ''} · ${num(u.output)} out${u.cost != null ? ` · $${u.cost.toFixed(u.cost < 0.1 ? 4 : 2)}` : ''}`;
const provName = id => aiProviders?.find(p => p.id === id)?.name ?? id;

function ideInit() {
  if (!aiProviders) aiLoad();
  // Files and chats belong to the open instance; another one starts the tab over.
  const root = S?.info?.root ?? null;
  if (ideReady !== root) {
    ideReady = root;
    ide.files = [];
    ide.cur = null;
    ideShow(null);
    ideDir('', $('#ide-tree'), 0);
    if (aiChat && !aiRunning) aiNewChat();
    aiChats();
  }
  $('#ai-text').focus();
}

// -- Providers, keys and the mod team (the Settings tab)

const ROLE_WHAT = { chat: 'A message without a role', lead: 'Runs the team for a request', researcher: 'Finds mods on Nexus that fit', installer: 'Installs a mod and reviews it', tester: 'Plays the game to try mods' };
let aiTeam = null;          // each role and who does it
const aiModels = {};        // each provider's model ids, fetched once

/** Who answers a message with `role` (a prompt name, or '' for none): the role's AI, else the open
 *  chat's, else the provider in use. */
function aiWho(role) {
  if (role) return aiTeam?.find(r => r.prompt === role)?.seat ?? null;
  if (aiChat && !aiChat.app) return { provider: aiChat.provider, model: aiChat.model ?? '' };
  const p = aiProviders?.find(p => p.current);
  return p ? { provider: p.id, model: '' } : null;
}
const seatName = s => !s?.provider ? 'nobody yet' : s.provider === 'mcp' ? 'your AI app' : provName(s.provider);

async function aiLoad() {
  try { [aiProviders, aiTeam] = await Promise.all([aiCall('status'), aiCall('team')]); } catch (e) { return toast(String(e), 'error'); }
  const p = aiProviders.find(p => p.current);
  $('#ai-note').textContent = !p ? 'Pick an AI and add your API key in Settings.' : !p.key ? `Add your ${p.name} API key in Settings.` : '';
  // The role picker says who answers each.
  $$('#ai-role option').forEach(o => {
    o.dataset.label ??= o.textContent;
    o.textContent = `${o.dataset.label} · ${seatName(o.value ? aiTeam.find(r => r.prompt === o.value)?.seat : p && { provider: p.id })}`;
  });
  if (tab === 'settings') drawSettings();
}

function drawSettings() {
  if (!aiProviders) return;
  const cur = aiProviders.find(p => p.current);
  const models = id => (aiModels[id] ?? []).map(m => `<option value="${esc(m)}">`).join('');
  $('#set-providers').innerHTML = '<tr><th>Provider</th><th>API key</th><th>Model</th><th></th></tr>' + aiProviders.map(p => `<tr data-id="${esc(p.id)}">
      <td><b>${esc(p.name)}</b></td>
      <td>${p.key ? `<span class="pill win">${p.key === 'saved' ? 'Saved' : `From ${esc(p.key)}`}</span>` : '<span class="muted small">No key</span>'}
        <button class="small" data-act="key">${p.key ? 'Change…' : 'Add…'}</button></td>
      <td><input class="model" list="models-${esc(p.id)}" value="${esc(p.model)}" spellcheck="false" aria-label="${esc(p.name)} model" title="Any model id ${esc(p.name)} offers"><datalist id="models-${esc(p.id)}">${models(p.id)}</datalist></td>
      <td><button class="small" data-act="options" title="Your instructions, effort, temperature and reply length">Options…</button></td></tr>`).join('');
  const pick = (role, sel) => aiProviders.map(p => `<option value="${esc(p.id)}"${sel === p.id ? ' selected' : ''}>${esc(p.name)}${p.key ? '' : ' (no key)'}</option>`).join('')
    + (role === 'chat' ? '' : `<option value="mcp"${sel === 'mcp' ? ' selected' : ''}>Your AI app, over MCP</option>`);
  const rows = [{ role: 'chat', name: 'Chat', seat: { provider: cur?.id ?? '', model: '' }, set: !!cur }, ...aiTeam];
  $('#set-team').innerHTML = '<tr><th>Role</th><th>AI</th><th>Model</th></tr>' + rows.map(r => {
    const first = r.role === 'chat' ? (cur ? '' : '<option value="">Pick a provider…</option>') : `<option value="">Same as Chat${cur ? ` (${esc(cur.name)})` : ''}</option>`;
    const own = r.set && r.role !== 'chat' && r.seat.provider !== 'mcp';
    const model = own
      ? `<input class="seat-model" list="models-${esc(r.seat.provider)}" value="${esc(r.seat.model)}" placeholder="${esc(aiProviders.find(p => p.id === r.seat.provider)?.model ?? '')}" spellcheck="false" aria-label="${esc(r.name)}'s model" title="Empty: the provider's model above">`
      : `<span class="muted small">${r.set && r.seat.provider === 'mcp' ? "The app's own" : esc(cur?.model ?? '')}</span>`;
    return `<tr data-role="${esc(r.role)}"><td><b>${esc(r.name)}</b><br><span class="muted small">${esc(ROLE_WHAT[r.role])}</span></td>
      <td><select class="seat" aria-label="${esc(r.name)}'s AI">${first}${pick(r.role, r.set ? r.seat.provider : null)}</select></td><td>${model}</td></tr>`;
  }).join('');
}

/** Suggest a provider's models in its list, fetched once (with a key). */
async function fillModels(id) {
  if (!id || aiModels[id] || !aiProviders?.find(p => p.id === id)?.key) return;
  try { aiModels[id] = await aiCall('models', { provider: id }); } catch { return; }
  const list = document.getElementById(`models-${id}`);
  if (list) list.innerHTML = aiModels[id].map(m => `<option value="${esc(m)}">`).join('');
}

$('#set-providers').onclick = e => {
  const b = e.target.closest('button[data-act]');
  if (b) (b.dataset.act === 'key' ? aiKey : aiOptions)(b.closest('tr').dataset.id);
};
$('#set-providers').onchange = async e => {
  if (!e.target.matches('input.model')) return;
  try { await aiCall('use', { provider: e.target.closest('tr').dataset.id, model: e.target.value.trim(), switch: false }); } catch (err) { toast(String(err), 'error'); }
  aiLoad();
};
$('#set-team').onchange = async e => {
  const tr = e.target.closest('tr'), role = tr.dataset.role, provider = $('select.seat', tr).value;
  if (role === 'chat' && !provider) return;
  try {
    if (role === 'chat') await aiCall('use', { provider });
    else await aiCall('seat', { role, provider, model: e.target.matches('.seat-model') ? e.target.value.trim() : '' });
  } catch (err) { toast(String(err), 'error'); }
  await aiLoad();
  if (provider && provider !== 'mcp' && !aiProviders.find(p => p.id === provider)?.key) aiKey(provider);
};
for (const t of ['#set-providers', '#set-team']) {
  $(t).addEventListener('focusin', e => {
    const tr = e.target.closest('tr');
    fillModels(e.target.matches('input.model') ? tr.dataset.id : e.target.matches('.seat-model') ? $('select.seat', tr).value : null);
  });
}
$('#set-access').onclick = () => $('#ai-access').click();

// ---- Settings: rmm's (settings.json, shared with the CLI and AI apps) and this window's -------

let conf = null, healthKinds = [], dlDir = null;
async function loadConf() {
  try { ({ settings: conf, health_kinds: healthKinds } = await call('settings')); } catch {}
  return conf;
}
const LANGS = [['en', 'English'], ['de', 'Deutsch'], ['es', 'Español'], ['fr', 'Français'], ['it', 'Italiano'], ['ja', '日本語'], ['ko', '한국어'], ['pl', 'Polski'], ['pt_BR', 'Português (Brasil)'], ['ru', 'Русский'], ['sv', 'Svenska'], ['uk', 'Українська'], ['zh_CN', '中文']];
const TABS = [['mods', 'Mods'], ['plugins', 'Plugins'], ['data', 'Data'], ['saves', 'Saves'], ['downloads', 'Downloads'], ['problems', 'Problems'], ['tools', 'Tools'], ['history', 'History'], ['ai', 'AI']];
// [key, label, kind, options or [min, max, step], help]. Keys with "win." are this window's.
const SETTINGS = [
  ['This window', 'Kept by this window on this computer.', [
    ['win.theme', 'Theme', 'select', [['system', 'Follow Windows'], ['light', 'Light'], ['dark', 'Dark']]],
    ['win.density', 'Rows', 'select', [['comfortable', 'Comfortable'], ['compact', 'Compact']]],
    ['win.zoom', 'Size', 'select', [[0.8, '80%'], [0.9, '90%'], [1, '100%'], [1.1, '110%'], [1.25, '125%'], [1.5, '150%']]],
    ['win.simple', 'Simple view', 'bool', null, 'Hides the Data, Tools and AI tabs, and with them AI access and the file editor.'],
    ['win.gettingStarted', 'Show the getting-started list on the Mods tab until it’s all done', 'bool'],
    ['win.startTab', 'Open on', 'select', [['last', 'The last tab'], ...TABS]],
    ['win.poll', 'Look for changes made by the CLI or AI apps', 'select', [[1.5, 'Every 1.5 seconds'], [5, 'Every 5 seconds'], [15, 'Every 15 seconds'], [0, 'Never']]],
    ['win.toast', 'Seconds a message stays', 'num', [2, 60], 'Errors stay until you click them.'],
    ['win.recentDays', 'The Recent filter shows mods installed in the last', 'num', [1, 365], 'days'],
    ['win.confirmDrop', 'Ask before installing files dropped on the window', 'bool'],
    ['win.deployPreview', 'Show what a deploy changes before it runs', 'bool'],
    ['win.tabSize', 'Editor tab width', 'num', [1, 16]],
    ['win.spaces', 'The Tab key inserts spaces in the editor', 'bool'],
    ['win.wrap', 'Wrap long lines in the editor', 'bool', null, 'Hides the line numbers.'],
    ['win.enterSends', 'Enter sends an AI message', 'bool', null, 'Otherwise Ctrl+Enter sends. Shift+Enter is always a new line.'],
  ]],
  ['Deploy', 'How mod files reach the game folder.', [
    ['deploy.method', 'Files in the game folder', 'select', [['link', 'Hard links (copies across drives)'], ['copy', 'Copies']], 'Copies take space, and nothing the game does to them reaches the mod. The next deploy redoes links as copies.'],
    ['deploy.exclude', 'Never deploy', 'lines', null, 'One pattern a line, by the path inside Data: *.txt, readme*, fomod/*. A pattern without / matches file names.'],
  ]],
  ['Play', '', [
    ['launch.script_extender', 'Start the game through its script extender when it’s installed', 'bool'],
    ['launch.args', 'Arguments for the game', 'text', null, 'Quotes keep spaces in one argument.'],
  ]],
  ['Installs', '', [
    ['install.enable', 'Mods installed here start enabled', 'bool'],
    ['@downloads', 'Downloads folder', 'folder', null, 'For this instance. Files already downloaded stay where they are.'],
    ['history.limit', 'Undo steps kept per profile', 'num', [1, 10000]],
  ]],
  ['LOOT', '', [
    ['loot.language', 'Messages in', 'select', LANGS, 'Where the masterlist has them; English otherwise.'],
    ['loot.show_info', 'Show LOOT’s notes too, not only its warnings and errors', 'bool'],
    ['loot.sort_after_tools', 'Place the plugins a tool run adds where LOOT would', 'bool', null, 'Every other plugin keeps its place.'],
    ['loot.refresh_hours', 'Download the masterlist again after', 'num', [1, 8760], 'hours'],
  ]],
  ['Problems', 'What the Problems tab and the AI’s health check report.', [
    ['health.hidden', 'Report', 'kinds'],
    ['health.ba2_limit', 'Fallout 4’s limit on BA2 archives', 'select', [[0, 'Work it out (1,024, a warning above 256)'], [256, '256 (before the August 2025 update)'], [1024, '1,024']]],
  ]],
  ['Test runs', 'Defaults for a test run; the Test run dialog and AI apps can change them for one run.', [
    ['test.window', 'Game window', 'text', null, 'A size such as 960x540 (behind your other windows), or fullscreen.'],
    ['test.dwell', 'Seconds in each cell', 'num', [1, 600]],
    ['test.timeout', 'Seconds to wait for the menu or a cell to load', 'num', [30, 3600]],
    ['test.screenshots', 'A screenshot in each cell', 'bool'],
    ['test.keep_runs', 'Keep the screenshots of the last', 'num', [0, 100000], 'test runs; 0 keeps all.'],
  ]],
  ['Nexus Mods', '', [
    ['nexus.check_on_start', 'Check for updates when the window opens (signed in)', 'bool'],
    ['nexus.recheck_days', 'Ask Nexus again about a mod it hasn’t listed as updated after', 'num', [1, 365], 'days'],
  ]],
  ['Tools', '', [['tools.timeout_mins', 'Stop a tool run from the window after', 'num', [0, 1440], 'minutes; 0: never.']]],
  ['rmm’s AI', '', [
    ['ai.max_steps', 'Steps in one turn before it pauses', 'num', [1, 500]],
    ['ai.budget', 'Stop a chat once it has cost', 'num', [0, 100000, 0.5], 'US dollars; 0: no limit. Only Claude and OpenRouter report costs, so chats with other AIs aren’t limited.'],
    ['ai.auto_allow', 'Tools it may run without asking', 'lines', null, 'One tool name a line (deploy, sort_plugins…), on top of reads and previews.'],
    ['ai.role_instructions.lead', 'Your instructions for the Lead', 'area'],
    ['ai.role_instructions.researcher', 'For the Researcher', 'area'],
    ['ai.role_instructions.installer', 'For the Installer', 'area'],
    ['ai.role_instructions.tester', 'For the Tester', 'area', null, 'Added to the role’s prompt, for rmm’s AI and for AI apps over MCP.'],
  ]],
  ['AI access (MCP)', '', [
    ['mcp.port', 'Port on 127.0.0.1', 'num', [1024, 65535], 'AI apps’ settings name this port; change them too.'],
    ['mcp.record_sessions', 'Keep each AI app’s session as a chat rmm’s AI can read', 'bool'],
  ]],
];
const pathGet = (o, k) => k.split('.').reduce((v, p) => v?.[p], o);
const pathSet = (o, k, v) => { const ps = k.split('.'), last = ps.pop(); const t = ps.reduce((x, p) => (x[p] ??= {}), o); if (v === '' && ps.at(-1) === 'role_instructions') delete t[last]; else t[last] = v; };

async function drawMore() {
  if (!conf) await loadConf();
  if (!conf) return;
  dlDir = await call('downloads_dir').catch(() => null);
  const cur = k => k.startsWith('win.') ? W[k.slice(4)] : pathGet(conf, k);
  const control = ([k, label, kind, opt]) => {
    const v = cur(k), at = `data-key="${k}" data-kind="${kind}" aria-label="${esc(label)}"`;
    if (kind === 'bool') return `<input type="checkbox" ${at} ${v ? 'checked' : ''}>`;
    if (kind === 'num') return `<input type="number" ${at} min="${opt[0]}" max="${opt[1]}" step="${opt[2] ?? 1}" value="${esc(v)}">`;
    if (kind === 'text') return `<input type="text" ${at} spellcheck="false" value="${esc(v)}">`;
    if (kind === 'area') return `<textarea ${at} rows="2">${esc(v)}</textarea>`;
    if (kind === 'lines') return `<textarea ${at} rows="3" spellcheck="false">${esc((v ?? []).join('\n'))}</textarea>`;
    if (kind === 'select') return `<select ${at}>${opt.map(([o, l]) => `<option value="${esc(o)}"${String(o) === String(v) ? ' selected' : ''}>${esc(l)}</option>`).join('')}</select>`;
    if (kind === 'kinds') return `<div class="kinds">${healthKinds.map(([id, what]) => `<label><input type="checkbox" data-key="${k}" data-kind="kind" value="${esc(id)}" ${(v ?? []).includes(id) ? '' : 'checked'}> ${esc(what)}</label>`).join('')}</div>`;
    if (kind === 'folder') return dlDir ? `<div class="set-path"><code title="${esc(dlDir.path)}">${esc(dlDir.path)}</code><button class="small" data-act="dl-pick">Choose…</button>${dlDir.custom ? '<button class="small" data-act="dl-reset" title="downloads/ in the instance">Default</button>' : ''}</div>` : '';
    return '';
  };
  const section = ([title, about, fields]) => `<h2>${esc(title)}</h2>${about ? `<p class="muted small">${esc(about)}</p>` : ''}
    <table class="set">${fields.map(f => `<tr><td>${esc(f[1])}</td><td>${control(f)}${f[4] ? `<span class="muted small">${esc(f[4])}</span>` : ''}</td></tr>`).join('')}</table>`;
  const ai = s => s[0] === 'rmm’s AI' || s[0] === 'AI access (MCP)';
  $('#set-more').innerHTML = SETTINGS.filter(s => !ai(s)).map(section).join('');
  $('#set-more-ai').innerHTML = SETTINGS.filter(ai).map(section).join('')
    + `<p class="set-reset"><button data-act="reset">Reset all settings…</button> <span class="muted small">rmm’s settings are in settings.json in your user folder, also changed with <code>rmm settings</code>.</span></p>`;
}

$('#set-more').onchange = $('#set-more-ai').onchange = async e => {
  const el = e.target.closest('[data-key]');
  if (!el) return;
  const { key, kind } = el.dataset;
  let v = kind === 'bool' ? el.checked
    : kind === 'num' ? Number(el.value)
    : kind === 'lines' ? el.value.split('\n').map(l => l.trim()).filter(Boolean)
    : kind === 'kind' ? $$('input[data-kind="kind"]', $('#set-more')).filter(c => !c.checked).map(c => c.value)
    : kind === 'area' ? el.value.trim() : el.value;
  if (kind === 'select' && el.value !== '' && !isNaN(el.value)) v = Number(el.value);
  if (key.startsWith('win.')) {
    W[key.slice(4)] = v;
    savePrefs();
    return applyWin();
  }
  const next = structuredClone(conf);
  pathSet(next, key, v);
  try { conf = await call('set_settings', { settings: next }); } catch (err) { toast(String(err), 'error'); return drawMore(); }
  if (key === 'mcp.port' && mcpOn) setMcp(true);
};
$('#set-more').onclick = $('#set-more-ai').onclick = async e => {
  const b = e.target.closest('button[data-act]');
  if (!b) return;
  if (b.dataset.act === 'reset') {
    if (!await confirmBox('Reset all settings', 'Put every setting on this page back to its default? The AI providers, keys and mod team stay.', 'Reset')) return;
    try { conf = await call('reset_settings'); } catch (err) { toast(String(err), 'error'); }
    Object.assign(W, WIN);
    savePrefs();
    applyWin();
  } else {
    const path = b.dataset.act === 'dl-reset' ? '' : await T.dialog.open({ directory: true, title: 'Where downloads go for this instance' });
    if (path === null) return;
    try { await call('set_downloads_dir', { path }); } catch (err) { toast(String(err), 'error'); }
  }
  drawMore();
};
$('#ai-to-settings').onclick = () => showTab('settings');

async function aiKey(id) {
  const p = aiProviders?.find(p => p.id === id);
  if (!p) return;
  const body = `<p>Paste your ${esc(p.name)} API key. rmm checks it with ${esc(p.name)}, then keeps it in your user folder, never in the instance.</p>
    <input id="ai-key-input" type="password" autocomplete="off" spellcheck="false" placeholder="API key" aria-label="API key">
    <p class="muted small">Your messages, pictures, open-file context and what rmm's tools answer go to ${esc(p.name)}, billed to your account. API address (change it for another region or a proxy):</p>
    <input id="ai-base" type="text" spellcheck="false" aria-label="API address" value="${esc(p.base)}">`;
  const done = dialog({ title: `${p.name} API key`, body, actions: [['cancel', 'Cancel'], ...(p.key === 'saved' ? [['forget', 'Delete key']] : []), ['ok', 'Save', 'primary']] });
  $('#ai-key-input').focus();
  $('#ai-key-input').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); $('#dlg').close('ok'); } };
  const how = await done;
  const key = $('#ai-key-input').value, base = $('#ai-base').value.trim();
  $('#ai-key-input').value = '';
  if (how === 'forget') {
    try { await aiCall('forget', { provider: p.id }); toast(`Deleted the ${p.name} key from this computer.`); } catch (e) { toast(String(e), 'error'); }
  } else if (how === 'ok') {
    if (base !== p.base) await aiCall('use', { provider: p.id, base, switch: false }).catch(e => toast(String(e), 'error'));
    if (key.trim()) {
      toast(`Checking the key with ${p.name}…`);
      try { toast(`${p.name} took the key (${plural(await aiCall('key', { provider: p.id, key }), 'model')}).`); } catch (e) { toast(String(e), 'error'); }
    }
  }
  aiLoad();
}

async function aiOptions(id) {
  const p = aiProviders?.find(p => p.id === id);
  if (!p) return;
  const o = p.options ?? {}, claude = p.api === 'anthropic';
  const effort = claude
    ? `<select id="o-effort"><option value="">rmm's default (high)</option>${['low', 'medium', 'high', 'xhigh', 'max'].map(v => `<option${o.effort === v ? ' selected' : ''}>${v}</option>`).join('')}</select>`
    : `<input id="o-effort" spellcheck="false" placeholder="the model's default" value="${esc(o.effort ?? '')}"><span class="muted small">Sent as reasoning_effort; models that reason take low, medium or high (Kimi: low, high, max).</span>`;
  const body = `<label class="field">Instructions for the AI <span class="muted small">Added to rmm's own, in every chat with ${esc(p.name)}.</span><textarea id="o-ins" rows="6">${esc(o.instructions ?? '')}</textarea></label>
    <label class="field">Effort ${effort}</label>
    ${claude ? '' : `<label class="field">Temperature <input id="o-temp" type="number" min="0" max="2" step="0.1" placeholder="the model's default" value="${o.temperature ?? ''}"></label>`}
    <label class="field">Most output tokens per reply <input id="o-max" type="number" min="1" max="128000" step="1" placeholder="${claude ? '16000' : "the model's default"}" value="${o.max_tokens ?? ''}"></label>`;
  if (await dialog({ title: `${p.name} options`, body, actions: [['cancel', 'Cancel'], ['ok', 'Save', 'primary']] }) !== 'ok') return;
  const n = sel => { const v = $(sel)?.value.trim(); return v ? Number(v) : null; };
  const options = { instructions: $('#o-ins').value, effort: $('#o-effort').value.trim() || null, temperature: claude ? null : n('#o-temp'), max_tokens: n('#o-max') };
  try { await aiCall('options', { provider: p.id, options }); toast('Saved; they apply from your next message.'); } catch (e) { toast(String(e), 'error'); }
  aiLoad();
}

// -- Side panel: files and chats

$$('.ide-side .seg button').forEach(b => {
  b.onclick = () => {
    $$('.ide-side .seg button').forEach(x => x.setAttribute('aria-selected', x === b));
    $('#ide-files').hidden = b.dataset.side !== 'files';
    $('#ide-chats').hidden = b.dataset.side !== 'chats';
    if (b.dataset.side === 'chats') aiChats();
  };
});

/** Fill `box` with the folder at `path`, one level down from `depth`. */
async function ideDir(path, box, depth) {
  let list;
  try { list = await call('ide_list', { path }); } catch (e) { box.innerHTML = `<div class="node muted small" style="padding-left:${10 + depth * 14}px">${esc(e)}</div>`; return; }
  const label = e => path === 'mods' ? nameOf(e.name) : e.name === 'ini' && depth === 0 ? 'INI files (My Games)' : e.name;
  box.innerHTML = list.map(e => `<div class="node${e.dir ? ' dir' : ''}" role="treeitem" data-path="${esc(e.path)}" data-depth="${depth}" style="padding-left:${10 + depth * 14}px" title="${esc(e.path)}"${e.dir ? ' aria-expanded="false"' : ''}><span class="tw">${e.dir ? '▸' : ''}</span>${esc(label(e))}${e.dir ? '' : ` <span class="muted small">${bytes(e.size)}</span>`}</div>${e.dir ? '<div class="kids" role="group" hidden></div>' : ''}`).join('')
    || `<div class="node muted small" style="padding-left:${10 + depth * 14}px">Empty</div>`;
}

async function ideToggle(node, open) {
  const kids = node.nextElementSibling;
  open ??= kids.hidden;
  if (open && !kids.dataset.loaded) {
    kids.dataset.loaded = '1';
    await ideDir(node.dataset.path, kids, +node.dataset.depth + 1);
  }
  kids.hidden = !open;
  node.setAttribute('aria-expanded', open);
  $('.tw', node).textContent = open ? '▾' : '▸';
}

$('#ide-tree').onclick = e => {
  const node = e.target.closest('.node[data-path]');
  if (!node) return;
  if (node.classList.contains('dir')) ideToggle(node);
  else ideOpen(node.dataset.path);
};

$('#ide-find').oninput = async () => {
  const q = $('#ide-find').value.trim().toLowerCase();
  const mods = $('#ide-tree .node[data-path="mods"]');
  if (mods && q) await ideToggle(mods, true);
  $$('#ide-tree .node[data-depth="1"][data-path^="mods/"]').forEach(n => {
    const hide = q && !n.textContent.toLowerCase().includes(q);
    n.hidden = hide;
    if (n.nextElementSibling?.classList.contains('kids') && hide) n.nextElementSibling.hidden = true;
  });
};

async function aiChats() {
  let list = [];
  try { list = await aiCall('chats'); } catch (e) { return toast(String(e), 'error'); }
  $('#ide-chat-list').innerHTML = list.map(c => `<div class="chat${c.id === aiChat?.id ? ' on' : ''}" data-id="${esc(c.id)}" tabindex="0">
      <div class="t">${esc(c.title || 'Chat')}</div>
      <div class="muted small">${c.provider === 'mcp' ? `${esc(c.model)} over MCP` : `${esc(provName(c.provider))} · ${esc(c.model)}`}<br>${when(c.updated)}${c.usage?.cost != null ? ` · $${c.usage.cost.toFixed(2)}` : ''}</div>
      <button class="x" data-del="${esc(c.id)}" title="Delete this chat" aria-label="Delete this chat">×</button></div>`).join('')
    || '<p class="muted small">No chats yet. They are kept in this instance, in ai/chats.</p>';
}

$('#ide-chat-list').onclick = async e => {
  const del = e.target.closest('[data-del]'), row = e.target.closest('.chat');
  if (del) {
    if (!await confirmBox('Delete this chat?', 'The conversation is deleted from this instance. Changes the AI made stay.', 'Delete')) return;
    try { await aiCall('delete_chat', { id: del.dataset.del }); } catch (err) { toast(String(err), 'error'); }
    if (aiChat?.id === del.dataset.del) aiNewChat();
    aiChats();
  } else if (row) aiOpenChat(row.dataset.id);
};
$('#ide-chat-list').onkeydown = e => { if (e.key === 'Enter' && e.target.classList.contains('chat')) aiOpenChat(e.target.dataset.id); };

function aiNewChat() {
  if (aiRunning) return toast('Stop the AI first.');
  aiChat = null;
  aiExchanges = [];
  $('#ai-log').innerHTML = '';
  $('#ai-empty').hidden = false;
  $('#ai-usage').textContent = '';
  aiInspect();
  $$('#ide-chat-list .chat').forEach(c => c.classList.remove('on'));
  $('#ai-text').focus();
}
$('#ai-new').onclick = aiNewChat;

async function aiOpenChat(id) {
  if (aiRunning) return toast('Stop the AI first.');
  let c;
  try { c = await aiCall('chat', { id }); } catch (e) { return toast(String(e), 'error'); }
  // An AI app's session over MCP (its calls and notes) can't be resumed, only read by a new chat.
  const app = c.provider === 'mcp' ? c.model : null;
  aiChat = { id: c.id, provider: c.provider, model: c.model, title: c.title, app };
  aiExchanges = [];
  $('#ai-log').innerHTML = '';
  $('#ai-empty').hidden = c.log.length > 0;
  for (const e of c.log) aiEvent(e, true);
  if (app) aiAdd('note', `${esc(app)}'s session over MCP: rmm kept its tool calls${c.log.some(e => e.kind === 'text') ? ' and the notes it saved' : ''}, not the conversation. A message here starts a new chat that reads this session first.`);
  $('#ai-usage').textContent = c.usage ? tokens(c.usage) : '';
  aiInspect();
  aiChats();
}

// -- Editor

const dirty = f => (f === ide.cur ? $('#ide-text').value : f.text) !== f.saved;

function ideTabs() {
  $('#ide-tabs').innerHTML = ide.files.map((f, i) => `<div class="ide-tab${f === ide.cur ? ' on' : ''}" role="tab" aria-selected="${f === ide.cur}" data-i="${i}" title="${esc(f.path)}">${esc(basename(f.path))}${dirty(f) ? ' <b aria-label="unsaved">•</b>' : ''}<button class="x" data-close="${i}" aria-label="Close ${esc(basename(f.path))}">×</button></div>`).join('');
}

function ideGutter() {
  const n = $('#ide-text').value.split('\n').length;
  if (+$('#ide-gutter').dataset.n === n) return;
  $('#ide-gutter').dataset.n = n;
  $('#ide-gutter').textContent = Array.from({ length: n }, (_, i) => i + 1).join('\n');
}

function ideShow(f) {
  if (ide.cur && ide.cur !== f) ide.cur.text = $('#ide-text').value;
  ide.cur = f;
  const has = !!f;
  $('#ide-filebar').hidden = $('#ide-code').hidden = !has;
  $('#ide-empty').hidden = has;
  const t = $('#ide-text');
  if (f) t.value = f.text; // before the tabs, which compare it with the saved text
  ideTabs();
  if (!f) return;
  t.readOnly = !f.editable;
  t.scrollTop = 0;
  $('#ide-path').textContent = f.path + (f.note ? ` · ${f.note}` : '');
  $('#ide-save').disabled = !f.editable;
  ideGutter();
}

async function ideOpen(path, reload = false) {
  let f = ide.files.find(f => f.path === path);
  if (!f || reload) {
    let d;
    try { d = await call('ide_read', { path }); } catch (e) { return toast(String(e), 'error'); }
    // A textarea holds \n only: Windows' \r\n go back in on save, so the file keeps them.
    const crlf = d.text.includes('\r\n'), text = d.text.replace(/\r\n/g, '\n');
    const fresh = { path, text, saved: text, crlf, hash: d.hash, editable: d.editable, note: d.note };
    if (f) Object.assign(f, fresh); else ide.files.push(f = fresh);
    if (f === ide.cur) { ide.cur = null; } // shown again below without keeping the old text
  }
  ideShow(f);
}

/** After a change on disk (an AI edit, a revert): reload the file if it's open and unsaved work won't be lost. */
function ideChanged(path) {
  const f = ide.files.find(f => f.path === path);
  if (!f) return;
  if (dirty(f)) toast(`${path} changed on disk. Your unsaved changes are still in the editor; Reload shows the file as it is now.`);
  else ideOpen(path, true);
}

async function ideSave() {
  const f = ide.cur;
  if (!f?.editable) return;
  const text = $('#ide-text').value, disk = f.crlf ? text.replace(/\n/g, '\r\n') : text;
  try {
    await call('ide_write', { path: f.path, text: disk, expect: f.hash });
  } catch (e) {
    if (!String(e).includes('changed since')) return toast(String(e), 'error');
    if (!await confirmBox('The file changed', `${f.path} changed on disk since you opened it (an AI edit or another program?). Save yours over it? The other version is kept in Edits….`, 'Save over it')) return;
    try { await call('ide_write', { path: f.path, text: disk }); } catch (err) { return toast(String(err), 'error'); }
  }
  try { f.hash = (await call('ide_read', { path: f.path })).hash; } catch {}
  f.saved = f.text = text;
  ideTabs();
  toast(`Saved ${basename(f.path)}.`);
}

$('#ide-tabs').onclick = async e => {
  const close = e.target.closest('[data-close]'), tab = e.target.closest('.ide-tab');
  if (close) {
    const i = +close.dataset.close, f = ide.files[i];
    if (dirty(f) && !await confirmBox('Close without saving?', `${f.path} has changes that aren't saved.`, 'Close')) return;
    ide.files.splice(i, 1);
    if (ide.cur === f) { ide.cur = null; ideShow(ide.files[Math.min(i, ide.files.length - 1)] ?? null); } else ideTabs();
  } else if (tab) ideShow(ide.files[+tab.dataset.i]);
};

$('#ide-text').oninput = () => {
  ideGutter();
  const was = $('#ide-tabs .ide-tab.on b') !== null;
  if (was !== dirty(ide.cur)) ideTabs();
};
$('#ide-text').onscroll = () => { $('#ide-gutter').scrollTop = $('#ide-text').scrollTop; };
$('#ide-text').onkeydown = e => {
  if (e.ctrlKey && e.key.toLowerCase() === 's') { e.preventDefault(); ideSave(); }
  else if (e.key === 'Tab' && !e.ctrlKey && !e.altKey && !e.shiftKey && !e.target.readOnly) { e.preventDefault(); document.execCommand('insertText', false, W.spaces ? ' '.repeat(W.tabSize) : '\t'); }
  else if (e.key === 'Escape') e.target.blur(); // then Tab moves on
};
$('#ide-save').onclick = ideSave;
$('#ide-reload').onclick = async () => {
  const f = ide.cur;
  if (f && (!dirty(f) || await confirmBox('Reload?', 'Your unsaved changes are lost.', 'Reload'))) ideOpen(f.path, true);
};
$('#ide-ask').onclick = () => {
  const t = $('#ide-text'), f = ide.cur;
  if (!f) return;
  const [a, b] = [t.selectionStart, t.selectionEnd];
  const line = i => t.value.slice(0, i).split('\n').length;
  aiCtx = a === b ? { path: f.path } : { path: f.path, from: line(a), to: line(b), text: t.value.slice(a, b).slice(0, 8000) };
  aiCtxDraw();
  $('#ai-text').focus();
};

function aiCtxDraw() {
  $('#ai-ctx').hidden = !aiCtx;
  if (aiCtx) $('#ai-ctx').innerHTML = `<span class="pill plain">${esc(aiCtx.path)}${aiCtx.from ? `, lines ${aiCtx.from}–${aiCtx.to}` : ''}</span><button type="button" class="link small" id="ai-ctx-x">Remove</button>`;
  $('#ai-ctx-x')?.addEventListener('click', () => { aiCtx = null; aiCtxDraw(); });
}

$('#ide-edits').onclick = async () => {
  let list;
  try { list = await call('ide_edits'); } catch (e) { return toast(String(e), 'error'); }
  const rows = list.slice(0, 300).map(e => `<tr><td>${esc(e.path)}</td><td>#${e.n} ${esc(e.by)}</td><td class="muted">${when(e.at)}</td><td class="muted">${e.created ? 'new file' : `${num(e.lines_before)} → ${num(e.lines_after)} lines`}</td><td><button type="button" data-revert="${e.n}">Revert</button></td></tr>`).join('');
  const done = dialog({ title: 'File edits', body: list.length ? `<table class="fc">${rows}</table><p class="muted small">Revert puts a file back as it was before that edit. A revert is an edit too, so it can be reverted.</p>` : '<p>No edits yet.</p>', actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  $$('#dlg [data-revert]').forEach(b => {
    b.onclick = async () => {
      try {
        const r = await call('ide_revert', { n: +b.dataset.revert });
        toast(`Put back ${r.path} as it was before edit #${b.dataset.revert}.`);
        $('#dlg').close();
        ideChanged(r.path);
      } catch (e) { toast(String(e), 'error'); }
    };
  });
  await done;
};

// -- Diffs for the AI's edits

function diffHtml(a, b) {
  const x = a ? a.split('\n') : [], y = b.split('\n');
  let s = 0;
  while (s < x.length && s < y.length && x[s] === y[s]) s++;
  let e = 0;
  while (e < x.length - s && e < y.length - s && x[x.length - 1 - e] === y[y.length - 1 - e]) e++;
  const xm = x.slice(s, x.length - e), ym = y.slice(s, y.length - e), n = xm.length, m = ym.length;
  const ops = [];
  if (n * m > 4e6) {
    // ponytail: too big for the line table; shown as all removed, then all added.
    xm.forEach(l => ops.push(['-', l]));
    ym.forEach(l => ops.push(['+', l]));
  } else {
    const w = m + 1, dp = new Uint32Array((n + 1) * w);
    for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i * w + j] = xm[i] === ym[j] ? dp[(i + 1) * w + j + 1] + 1 : Math.max(dp[(i + 1) * w + j], dp[i * w + j + 1]);
    let i = 0, j = 0;
    while (i < n && j < m) {
      if (xm[i] === ym[j]) { ops.push([' ', xm[i++]]); j++; }
      else if (dp[(i + 1) * w + j] >= dp[i * w + j + 1]) ops.push(['-', xm[i++]]);
      else ops.push(['+', ym[j++]]);
    }
    while (i < n) ops.push(['-', xm[i++]]);
    while (j < m) ops.push(['+', ym[j++]]);
  }
  const line = ([k, t]) => `<span class="${k === '+' ? 'add' : k === '-' ? 'del' : ''}">${k} ${esc(t)}</span>`;
  const around = [...x.slice(Math.max(0, s - 3), s).map(t => [' ', t]), ...ops, ...x.slice(x.length - e, x.length - e + 3).map(t => [' ', t])];
  const adds = ops.filter(o => o[0] === '+').length, dels = ops.filter(o => o[0] === '-').length;
  return `<p class="muted small">${a ? `From line ${Math.max(1, s - 2)}` : 'A new file'}: ${plural(adds, 'line')} added, ${plural(dels, 'line')} removed.</p><pre class="diff">${around.map(line).join('')}</pre>`;
}

async function editPreview(args) {
  let cur = null;
  try { cur = (await call('ide_read', { path: args.path })).text; } catch {}
  if (args.old == null) return diffHtml(cur ?? '', args.new ?? '');
  const i = cur?.indexOf(args.old) ?? -1;
  if (i < 0) return '<p class="small bad">The text to replace isn\'t in the file, so this edit will fail.</p>';
  return diffHtml(cur, cur.slice(0, i) + args.new + cur.slice(i + args.old.length));
}

// -- Chat

function aiAdd(cls, html) {
  $('#ai-empty').hidden = true;
  const el = document.createElement('div');
  el.className = 'ai-msg ' + cls;
  el.innerHTML = html;
  $('#ai-log').append(el);
  $('#ai-scroll').scrollTop = $('#ai-scroll').scrollHeight;
  return el;
}

function aiBusy(on) {
  aiRunning = on;
  $('#ai-send').disabled = on;
  $('#ai-stop').hidden = !on;
  $('#ai-status').textContent = on ? 'Working…' : '';
}

const argsLine = a => { const s = JSON.stringify(a ?? {}); return s === '{}' ? '' : s.length > 120 ? s.slice(0, 120) + '…' : s; };
const userHtml = (text, role, pics) => `${role ? `<span class="pill plain">${esc(ROLES[role] ?? role)}</span> ` : ''}${esc(text)}${pics}`;

/** One event of a turn, live or from a saved chat (`replay`). */
function aiEvent(e, replay = false) {
  if (e.kind === 'user') { if (replay) aiAdd('me', userHtml(e.text, e.role, e.pictures ? ` <span class="pill plain">${plural(e.pictures, 'picture')}</span>` : '')); }
  else if (e.kind === 'text') aiAdd('bot', esc(e.text));
  else if (e.kind === 'notice') aiAdd('note', esc(e.text));
  else if (e.kind === 'call') {
    const el = aiAdd('tool', `<details><summary><span class="pill plain">${esc(e.name)}</span> <span class="muted small">${esc(argsLine(e.args))}</span> <span class="ai-state muted small">${replay ? '' : 'running…'}</span></summary><pre class="sel">${esc(JSON.stringify(e.args, null, 2))}</pre></details>`);
    aiLastCall = { el, name: e.name, args: e.args };
  } else if (e.kind === 'result' && aiLastCall) {
    const st = $('.ai-state', aiLastCall.el);
    st.textContent = e.ok ? 'done' : 'failed';
    st.className = `ai-state small ${e.ok ? 'muted' : 'bad'}`;
    $('details', aiLastCall.el).insertAdjacentHTML('beforeend', `<pre class="sel">${esc(e.text)}</pre>`);
    if (!replay && e.ok && aiLastCall.name === 'edit_file') ideChanged(String(aiLastCall.args?.path ?? '').replace(/\\/g, '/').replace(/^\/+|\/+$/g, ''));
    if (!replay && e.ok && aiLastCall.name === 'revert_edit') { try { ideChanged(JSON.parse(e.text).path); } catch {} }
    aiLastCall = null;
  }
}

// What a change the AI asks to make does, in plain words, and how it's undone. The tool and its
// arguments fold away under Details.
const UNDO = 'You can undo this.';
const modName = id => byId?.get(id) ? nameOf(id) : String(id ?? '');
const someMods = (ids = []) => ids.length > 3 ? `${ids.slice(0, 3).map(modName).join(', ')} and ${ids.length - 3} more` : ids.map(modName).join(', ');
const PLAIN = {
  install_mod: a => [`Install ${basename(String(a.path ?? ''))}`, UNDO],
  reinstall_mod: a => [`Reinstall ${modName(a.id)}`, UNDO],
  update_mod: a => [`Update ${modName(a.id)}`, 'Undo puts the old files back.'],
  rename_mod: a => [`Rename ${modName(a.id)} to “${a.name}”`, UNDO],
  uninstall_mods: a => [`Uninstall ${someMods(a.ids)}`, 'Their files go to the trash; Undo brings them back.'],
  overwrite_to_mod: a => [`Make the files in Overwrite into the mod “${a.name}”`, UNDO],
  clear_overwrite: () => ['Clear Overwrite', 'Its files go to the trash; Undo brings them back.'],
  desktop_shortcut: a => [`Put a shortcut to ${a.profile ? `“${a.profile}”` : 'this profile'} on the Desktop`, 'It switches to the profile, deploys and starts the game.'],
  rebuild_outputs: () => ['Run again the tools whose output is out of date', 'Each run is its own undo step.'],
  clean_dirty_plugins: () => ['Clean the dirty plugins with xEdit QuickAutoClean', 'Each clean copy goes into its own mod; Undo brings the original back.'],
  hide_files: a => [`${a.show ? 'Show' : 'Hide'} ${plural(a.paths?.length ?? 0, 'file')} in ${nameOf(a.id)}`, a.show ? '' : 'They stay out of the game until shown again.'],
  move_files: a => [`Move ${plural(a.paths?.length ?? 0, 'file')} from ${a.from === OVERWRITE_ID ? 'Overwrite' : nameOf(a.from)} into ${a.to === OVERWRITE_ID ? 'Overwrite' : nameOf(a.to)}`, a.replace ? 'Files it replaces go to the trash.' : ''],
  creation_club: a => [a.restore ? 'Put the Creation Club content back into the game’s Data folder' : 'Make the game’s Creation Club content mods', 'This moves the game’s own files; the other way round undoes it.'],
  set_mods_enabled: a => [`${a.enabled ? 'Turn on' : 'Turn off'} ${someMods(a.ids)}`, UNDO],
  move_mods: a => [`Move ${someMods(a.ids)} in the mod list`, UNDO],
  set_mod_order: () => ['Reorder the mod list', UNDO],
  add_separator: a => [`Add the separator “${a.name}”`, UNDO],
  edit_separator: a => [`Change the separator ${modName(a.id)}`, UNDO],
  set_mod_note: a => [`Change ${modName(a.id)}’s notes`, ''],
  set_mod_categories: a => [`Change ${modName(a.id)}’s categories`, ''],
  set_color: () => ['Change a row’s colour', UNDO],
  set_plugin_enabled: a => [`${a.enabled ? 'Turn on' : 'Turn off'} ${(a.names ?? [a.name]).filter(Boolean).join(', ')}`, UNDO],
  lock_plugin: a => [`${a.lock === false ? 'Unlock' : 'Lock'} ${a.name} at its place in the load order`, UNDO],
  set_plugin_order: () => ['Reorder your plugins', UNDO],
  sort_plugins: () => ['Sort your plugins with LOOT', UNDO],
  sort_mods: () => ['Reorder your mods by the mod rules', UNDO],
  set_loot_rule: a => [`Change LOOT’s rules for ${a.plugin}`, ''],
  set_mod_rule: a => [`Change the mod rules for ${modName(a.id)}`, ''],
  update_masterlist: () => ['Download LOOT’s newest masterlist', ''],
  deploy: () => ['Deploy: put your mods into the game folder', 'Purge takes them out again.'],
  purge: () => ['Purge: take the mods out of the game folder', 'Deploy puts them back.'],
  launch_game: a => a.without_mods ? ['Start the game without mods', 'It purges them; the next Play puts them back.'] : ['Start the game', 'It deploys your changes first.'],
  steam_updates: a => [a.hold ? 'Stop Steam updating the game (its manifest becomes read-only)' : 'Let Steam update the game again', ''],
  add_tool: a => [`Add the tool ${a.id}`, ''],
  run_tool: a => [`Run ${a.id}`, 'What it makes becomes a mod; Undo takes it out.'],
  create_profile: a => [`Make the profile “${a.name}”`, ''],
  new_playthrough: a => [`Start a new playthrough, “${a.name}”`, 'Your current profile and its saves stay as they are.'],
  switch_profile: a => [`Switch to the profile “${a.name}”`, ''],
  rename_profile: a => [`Rename the profile “${a.from}” to “${a.to}”`, ''],
  delete_profile: a => [`Delete the profile “${a.name}”`, 'It’s kept in profiles\\.deleted.'],
  set_profile_options: () => ['Change whether this profile has its own INIs or saves', UNDO],
  transfer_saves: a => [`${a.move_them ? 'Move' : 'Copy'} ${plural(a.files?.length ?? 0, 'save')} to “${a.profile}”`, ''],
  enable_save_plugins: a => [`Turn on the plugins ${a.file} needs`, UNDO],
  test_run: () => ['Start the game for a test run', ''],
  test_session_start: () => ['Start the game to test it step by step', ''],
  bisect: () => ['Turn off half of the suspects to find the culprit', UNDO],
  hold_updates: a => [`${a.hold === false ? 'Stop holding back' : 'Hold back'} updates for ${someMods(a.ids)}`, ''],
  install_from_nexus: () => ['Download a mod from Nexus and install it', UNDO],
  install_collection: () => ['Install a Nexus collection', ''],
  import_mo2: () => ['Import your setup from MO2', UNDO],
  import_vortex: () => ['Import your setup from Vortex', UNDO],
  apply_lockfile: () => ['Apply a lockfile to this profile', UNDO],
  undo: () => ['Undo the last change', 'Redo brings it back.'],
  redo: () => ['Redo the change you undid', ''],
  restore: () => ['Go back to an earlier point in History', UNDO],
  revert_edit: () => ['Put an edited file back as it was', ''],
  support_bundle: () => ['Save a support bundle', ''],
  export_lockfile: a => [a.format === 'pack' ? 'Save a pack of this setup' : a.format === 'markdown' ? 'Save a readable list of your mods' : 'Save a lockfile', ''],
  call_addon: a => [`Run the add-on ${a.addon}`, ''],
};
function plainCall(name, args) {
  try { const p = PLAIN[name]?.(args ?? {}); if (p) return p; } catch { /* the fallback */ }
  const words = name.replace(/_/g, ' ');
  return [words[0].toUpperCase() + words.slice(1), 'It changes your setup or starts something.'];
}

T.event.listen('ai', async ({ payload: e }) => {
  if (e.kind === 'approve') {
    const edit = e.name === 'edit_file';
    const [what, undo] = edit ? [`Edit ${e.args?.path}`, 'Edits…, by the file editor, puts it back.'] : plainCall(e.name, e.args);
    const el = aiAdd('ask', `<p><b>${esc(what)}?</b> <span class="muted small">${esc(undo)}</span></p>
      <div class="ai-body">${edit ? '<p class="muted small">Reading the file…</p>' : `<details><summary class="muted small">Details: ${esc(e.name)}</summary><pre class="sel">${esc(JSON.stringify(e.args, null, 2))}</pre></details>`}</div>
      <div class="row">${edit ? '<button type="button" class="link" data-open>Open the file</button><span class="spacer"></span>' : ''}<button data-a="no">Decline</button><button data-a="always" title="Allow every change for the rest of this chat">Allow all in this chat</button><button data-a="yes" class="primary">Allow</button></div>`);
    $$('.row [data-a]', el).forEach(b => {
      b.onclick = () => {
        T.core.invoke('ai_answer', { answer: b.dataset.a });
        const said = { yes: 'Allowed.', always: 'Allowed, and every change for the rest of this chat.', no: 'Declined.' }[b.dataset.a];
        $('.row', el).outerHTML = `<p class="muted small">${said}</p>`;
      };
    });
    $('[data-open]', el)?.addEventListener('click', () => ideOpen(String(e.args.path)));
    $('button.primary', el).focus();
    if (edit) $('.ai-body', el).innerHTML = await editPreview(e.args);
  } else if (e.kind === 'done') {
    aiBusy(false);
    $$('#ai-log .ask .row').forEach(r => { r.outerHTML = '<p class="muted small">Stopped.</p>'; });
    if (e.error) aiAdd('note bad', esc(e.error));
    if (e.chat) {
      aiChat = { id: e.chat.id, provider: e.chat.provider, model: e.chat.model };
      $('#ai-usage').textContent = tokens(e.chat.usage);
      if (!$('#ide-chats').hidden) aiChats();
    }
  } else aiEvent(e);
});

T.event.listen('ai-exchange', ({ payload: x }) => {
  aiExchanges.push(x);
  if (!$('#ai-inspect').hidden) aiInspect();
});

function aiInspect() {
  const pretty = v => esc(JSON.stringify(v, null, 2));
  $('#ai-inspect').innerHTML = aiExchanges.length
    ? aiExchanges.slice().reverse().map(x => `<details class="ix"><summary><b>${new Date(x.at).toLocaleTimeString()}</b> · ${esc(x.model)} · ${(x.ms / 1000).toFixed(1)} s<br><span class="muted small">${tokens(x.usage)}</span></summary>
        <p class="muted small">${esc(x.url)}</p><h4>Request</h4><pre class="sel">${pretty(x.request)}</pre><h4>Response</h4><pre class="sel">${pretty(x.response)}</pre></details>`).join('')
    : '<p class="empty">Each request to the provider shows here once you send a message: the request (tools counted, earlier messages and pictures left out), the response, tokens and, for Claude and OpenRouter, the cost.</p>';
}

$$('.ide-ai .seg button').forEach(b => {
  b.onclick = () => {
    $$('.ide-ai .seg button').forEach(x => x.setAttribute('aria-selected', x === b));
    const inspect = b.dataset.view === 'inspect';
    $('#ai-scroll').hidden = inspect;
    $('#ai-inspect').hidden = !inspect;
    if (inspect) aiInspect();
  };
});

function drawPics() {
  $('#ai-pics').innerHTML = aiPics.map((p, i) => `<button type="button" class="ai-pic" data-i="${i}" title="Remove this picture">${p.startsWith('data:') ? `<img src="${p}" alt="Picture ${i + 1}">` : esc(basename(p))} ×</button>`).join('');
  $$('#ai-pics .ai-pic').forEach(b => { b.onclick = () => { aiPics.splice(+b.dataset.i, 1); drawPics(); }; });
}

$('#ai-attach').onclick = () => $('#ai-file').click();
$('#ai-file').onchange = async () => {
  for (const f of $('#ai-file').files) {
    if (f.size > 3750000) { toast(`${f.name} is over 3.75 MB, more than the providers take.`, 'error'); continue; }
    aiPics.push(await new Promise((ok, bad) => { const r = new FileReader(); r.onload = () => ok(r.result); r.onerror = bad; r.readAsDataURL(f); }));
  }
  $('#ai-file').value = '';
  drawPics();
};

/** Pictures dropped on the AI tab are attached, not installed. */
function aiDrop(p) {
  const over = p.type === 'enter' || p.type === 'over';
  $('#dropzone').hidden = !over;
  $('#drop-text').textContent = over ? 'Drop pictures to attach' : 'Drop to install';
  if (p.type !== 'drop') return;
  const pics = (p.paths ?? []).filter(f => PICTURE.test(f));
  if (pics.length < (p.paths ?? []).length) toast('Only PNG, JPEG, GIF and WebP pictures can be attached.');
  aiPics.push(...pics);
  drawPics();
}

$('#ai-text').onkeydown = e => { if (e.key === 'Enter' && !e.shiftKey && (W.enterSends || e.ctrlKey)) { e.preventDefault(); $('#ai-form').requestSubmit(); } };
$('#ai-form').onsubmit = async ev => {
  ev.preventDefault();
  const typed = $('#ai-text').value.trim(), role = $('#ai-role').value;
  if (aiRunning || (!typed && !role)) return;
  const who = aiWho(role), p = aiProviders?.find(x => x.id === who?.provider);
  if (who?.provider !== 'mcp' && !p?.key) { toast(p ? `Add your ${p.name} API key in Settings.` : 'Pick an AI in Settings first.', 'error'); return showTab('settings'); }
  // A role on another AI than this chat's starts a new chat: a chat keeps its provider's format.
  if (role && who.provider !== 'mcp' && aiChat && !aiChat.app && aiChat.provider !== who.provider) aiNewChat();
  const f = aiCtx && ide.files.find(f => f.path === aiCtx.path);
  const unsaved = f && dirty(f) ? ' The editor has unsaved changes to it, so the file on disk is older.' : '';
  const ctx = !aiCtx ? '' : aiCtx.text
    ? `In ${aiCtx.path}, lines ${aiCtx.from}–${aiCtx.to}:${unsaved}\n\`\`\`\n${aiCtx.text}\n\`\`\`\n\n`
    : `(About ${aiCtx.path}, open in the editor.${unsaved})\n\n`;
  const from = aiChat?.app ? aiChat : null;
  const carry = from ? `Carry on from ${from.app}'s session over MCP, "${from.title}" (chat ${from.id}): read it first with read_chat.\n\n` : '';
  const text = carry + ctx + typed;
  const pics =aiPics.map(p => p.startsWith('data:') ? `<img src="${p}" alt="">` : ` <span class="pill plain">${esc(basename(p))}</span>`).join('');
  aiAdd('me', userHtml(text, role, pics));
  aiBusy(true);
  try {
    await T.core.invoke('ai_send', { text, role: role || null, images: aiPics, chat: from ? null : aiChat?.id ?? null });
    $('#ai-text').value = '';
    $('#ai-role').value = '';
    aiPics = [];
    aiCtx = null;
    drawPics();
    aiCtxDraw();
  } catch (e) {
    aiBusy(false);
    aiAdd('note bad', esc(e));
  }
};
$('#ai-stop').onclick = () => { $('#ai-status').textContent = 'Stopping after this step…'; T.core.invoke('ai_stop'); };

// ---- Dialogs and toasts ---------------------------------------------------------------------

/** Modal dialog. actions: [[value, label, class]]. Resolves to the chosen value (the input's
 *  text when `input` is given), or null when cancelled. */
/** A mod's read-me, wrapped, to read in a dialog. */
async function showDoc(id, doc) {
  let d;
  try { d = await call('ide_read', { path: `mods/${id}/${doc}` }); } catch (e) { return toast(String(e), 'error'); }
  const shown = dialog({ title: doc.split('/').pop(), body: `<pre class="doc-view">${esc(d.text)}</pre>`, actions: [['ok', 'Close', 'primary']] });
  $('#dlg').classList.add('wide');
  await shown;
}

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
const confirmBox = async (title, text, yes, kind = 'danger') => (await dialog({ title, body: `<p>${esc(text)}</p>`, actions: [['cancel', 'Cancel'], ['ok', yes, kind]] })) === 'ok';

/** `action`: [label, fn], a button on the toast. */
function toast(msg, kind = 'info', action = null) {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = msg;
  if (action) {
    const b = document.createElement('button');
    b.textContent = action[0];
    b.onclick = e => { e.stopPropagation(); el.remove(); action[1](); };
    el.append(document.createElement('br'), b);
  }
  el.title = 'Click to dismiss';
  el.onclick = () => el.remove();
  $('#toasts').append(el);
  // Errors stay until clicked (the newest five), so a refusal can be read twice.
  if (kind === 'error') $$('#toasts .toast.error').slice(0, -5).forEach(t => t.remove());
  else setTimeout(() => el.remove(), W.toast * (action ? 3000 : 1000)); // time to reach the button
}

applyWin();
start();
