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
/** The game's name as players know it: Skyrim's 1.6 builds are the Anniversary Edition's. */
const gameName = (game, version) => game === 'skyrim-se' && /^1\.6\./.test(version ?? '') ? 'Skyrim Anniversary Edition' : GAMES[game] ?? game;
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
let whyData = null;         // { name, r }: plugin_why for the plugin panel
let recordData = null;      // { name, rows }: record_conflicts for the plugin panel; null = reload
let toolsData = null;       // { tools, presets }
let ignored = new Set();    // health findings ignored in this instance ("kind|subject")
let busy = false;
let healthTimer = 0, healthRun = 0, filterTimer = 0;
let historyData = null;     // ops::history, loaded when the History tab shows
let side = 'mods';          // which list was clicked last, when both show
let selPluginMod = null;    // the mod providing the selected plugin
let dropAt = null;          // { id, priority } while a file is dragged over a mod row

// Window preferences: columns, their widths, the tab, the filter and the split view.
const PREFS = 'rmm.prefs';
const prefs = (() => { try { return JSON.parse(localStorage.getItem(PREFS)) ?? {}; } catch { return {}; } })();
const savePrefs = () => { try { localStorage.setItem(PREFS, JSON.stringify(prefs)); } catch {} };
prefs.cols ??= { conflicts: true, tags: true, notes: false, installed: false, files: true, priority: true };
prefs.cols.category ??= true;
prefs.widths ??= {};
prefs.floating ??= {}; // tab -> { x, y, w, h }: tabs pulled out into windows inside rmm's window
prefs.docked ??= {}; // tab -> 'left' | 'right' | 'bottom': those of them docked at an edge
prefs.dockSize ??= { left: 360, right: 360, bottom: 260 };
// This window's look and habits (Settings, "This window").
const WIN = { theme: 'system', density: 'compact', zoom: 1, simple: false, gettingStarted: true, startTab: 'last', poll: 1.5, toast: 5, recentDays: 7, confirmDrop: false, deployPreview: true };
const W = prefs.win = { ...WIN, ...prefs.win };
if (!prefs.compactOnce) { prefs.compactOnce = true; W.density = 'compact'; } // compact became the default
if (!prefs.contentOnce) { prefs.contentOnce = true; prefs.cols.tags = true; } // the Content column became a default
function applyWin() {
  if (W.theme === 'system') delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = W.theme;
  document.body.classList.toggle('compact', W.density === 'compact');
  document.body.classList.toggle('simple', W.simple); // hides .adv
  T.webview.getCurrentWebview().setZoom(W.zoom).catch(() => {});
  // Simple view: the tabs a beginner doesn't need yet.
  for (const t of SIMPLE_HIDES) $(`.tabs [data-tab="${t}"]`).hidden = W.simple;
  visibleTabs().forEach((t, i) => { $(`.tabs [data-tab="${t}"]`).title = i < 9 ? `Ctrl+${i + 1}` : 'Ctrl+,'; });
  if (W.simple && SIMPLE_HIDES.includes(tab)) { tab = 'mods'; if (!$('#app').hidden) showTab('mods'); }
  if (S) renderStart();
}
const SIMPLE_HIDES = ['data', 'tools', 'mcp'];
const MAIN_ONLY = ['mods', 'plugins']; // side by side in the main window, never pulled out
const TAB_IDS = ['mods', 'plugins', 'data', 'saves', 'downloads', 'problems', 'tools', 'history', 'mcp', 'settings'];
const tabLabel = t => $(`.tabs [data-tab="${t}"]`).firstChild.textContent.trim();
let winTop = 30; // the front window's z-index; menus (40) and toasts (60) stay above
const visibleTabs = () => $$('.tabs [data-tab]').filter(b => !b.hidden).map(b => b.dataset.tab);
// `w`: the default width in pixels, as style.css sets it for `cls`.
const COLS = {
  conflicts: { title: 'Conflicts', cls: 'conf-col', w: 200 },
  category: { title: 'Category', cls: 'cat-col', w: 140, tip: 'Click a category to show only its mods' },
  tags: { title: 'Content', cls: 'tags-col', w: 160, tip: 'What the mod holds; click one to show only mods with it' },
  notes: { title: 'Notes', cls: 'notes-col', w: 200 },
  installed: { title: 'Installed', cls: 'date-col', w: 96 },
  version: { title: 'Version', cls: 'ver-col', w: 90, tip: 'The installed version, as Nexus names it' },
  files: { title: 'Files', cls: 'num', w: 64 },
  priority: { title: '#', cls: 'num', w: 64, tip: 'Priority: higher wins file conflicts' },
};
const shownCols = () => Object.keys(COLS).filter(k => prefs.cols[k]);
const splitOn = () => true; // the plugins always show beside the mods; they have no tab of their own
/** The tab shows: the main window's, or one pulled out into a window of its own. */
const shown = t => tab === t || !!prefs.floating[t];
/** The plugin list shows: its own tab, a window, or beside the mods. */
const plugVis = () => shown('plugins') || (tab === 'mods' && splitOn());
const pluginSide = () => (tab === 'plugins' && !shown('mods')) || (plugVis() && side === 'plugins' && !!selPlugin);
const nameOf = id => names.get(id) ?? id;
const isSep = id => !!byId.get(id)?.separator;

// ---- Calls ----------------------------------------------------------------------------------

/** Run one change: progress bar, errors as toasts, reload afterwards. Resolves to the call's
 *  result (true for calls that return nothing), or undefined when it failed. With `problem`, a
 *  failure is also listed in Problems until dismissed. */
async function act(label, fn, { reload = true, problem = false } = {}) {
  if (busy) return busyNote();
  // Keyboard moves still waiting go first, so this change (an undo, say) comes after them.
  const waiting = [...queued.values()];
  waiting.forEach(q => clearTimeout(q.timer));
  queued.clear();
  setBusy(true, label);
  try {
    for (const q of waiting) await q.send();
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

// Keyboard moves: each key press moves the rows in the window at once, and the result goes to
// rmm when the keys stop (or another selection moves), so holding Ctrl+Down keeps up instead of
// refusing presses while one move round-trips. `send` reads the window's order when it runs.
const queued = new Map(); // kind -> { key, send, timer }
const queuedSends = () => queued.size > 0;
function queueSend(kind, key, send) {
  const q = queued.get(kind);
  if (q && q.key !== key) flushSend(kind);
  clearTimeout(queued.get(kind)?.timer);
  ++loadRun; // a reload already under way would draw the old order over this one
  queued.set(kind, { key, send, timer: setTimeout(() => flushSend(kind), 350) });
}
function flushSend(kind) {
  const q = queued.get(kind);
  if (!q) return;
  clearTimeout(q.timer);
  if (busy) { q.timer = setTimeout(() => flushSend(kind), 150); return; }
  queued.delete(kind);
  // No reload while more moves wait: theirs brings the result.
  act('Moving', q.send, { reload: false }).then(() => { if (!queuedSends()) load(); });
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
  lightCands = null;
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
  recordData = null;
  if (shown('plugins') && selPlugin) loadLootRules();
  if (shown('tools') || toolsData) loadTools();
  issuesStale = true;
  historyData = null;
  if (shown('history')) loadHistory();
  dataLoaded = false;
  if (shown('data')) loadData(false);
  if (shown('saves')) loadSaves();
  if (shown('downloads') || downloadsData) loadDownloads();
  render();
  refreshPlan();
  renderStart();
  // The health check reads every plugin; re-run it once a burst of changes settles.
  clearTimeout(healthTimer);
  healthTimer = setTimeout(runHealth, shown('problems') ? 0 : 800);
  if (oneMod()) loadDetails();
  // Windows that came back with the app have nothing in them until the first load.
  if (!winsFilled) { winsFilled = true; Object.keys(prefs.floating).forEach(fillPane); }
}
let winsFilled = false;

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
  if (busy || polling || queuedSends() || !S || document.hidden || !W.poll || Date.now() - polled < W.poll * 1000) return;
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
  nexusShow().then(async () => {
    if (!nexus.user) return;
    // The account's endorsements and tracked mods, for the panel and the filters (two requests).
    call('nexus_account').then(load).catch(() => {});
    if ((await loadConf())?.nexus.check_on_start) checkUpdates(true);
  });
  takeLinks(); // a link that started the window
  rmmUpdateOnStart();
}

/** Once a launch, quietly: a newer rmm release, offered on a toast. */
let rmmChecked = false;
async function rmmUpdateOnStart() {
  if (rmmChecked) return;
  rmmChecked = true;
  const r = await T.core.invoke('app_update', { install: false }).catch(() => null); // hand-built or offline: nothing to say
  if (r?.newer) toast(`rmm ${r.newer} is out (you have ${r.current}).`, 'info', ['Install…', () => $('#set-update').click()]);
}

// The last tab, filter and state chips come back with the window.
const START = W.startTab === 'last' ? prefs.tab : W.startTab;
tab = TAB_IDS.includes(START) && START !== 'plugins' ? START : 'mods';
for (const t of Object.keys(prefs.floating)) {
  if (TAB_IDS.includes(t) && !MAIN_ONLY.includes(t) && !(W.simple && SIMPLE_HIDES.includes(t))) makeWin(t);
  else { delete prefs.floating[t]; delete prefs.docked[t]; }
}
if (prefs.floating[tab]) tab = TAB_IDS.find(t => !prefs.floating[t] && !(W.simple && SIMPLE_HIDES.includes(t))) ?? 'mods';
$('#filter').value = prefs.filter ?? '';
filters = new Set((prefs.chips ?? []).filter(f => f in { enabled: 1, disabled: 1, conflicted: 1, overridden: 1, rule: 1, masters: 1, plugins: 1, recent: 1, update: 1, endorsed: 1, tracked: 1, problems: 1 }));

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
    `<button data-game="${esc(d.game)}" data-path="${esc(d.game_path)}"${first && !i ? ' class="primary"' : ''}>New instance for ${esc(gameName(d.game, d.version))}<br><span class="muted small">${esc([d.version && `version ${d.version}`, d.steam ? 'Steam' : 'not Steam', d.free != null && `${bytes(d.free)} free`].filter(Boolean).join(' · '))}<br>${esc(d.game_path)}</span></button>`).join('');
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
  const d = gamesFound.find(g => samePath(g.game_path, gamePath));
  const suggested = `${there}\\rmm\\${name}`;
  // A modded setup soon takes tens of GB: downloads, the mods, and tool output.
  const free = d?.free != null ? `<p class="${d.free < 20 * 2 ** 30 ? 'bad' : 'muted'} small">${bytes(d.free)} free on drive ${esc(there.replace(':', ''))}.${d.free < 20 * 2 ** 30 ? ' A modded game soon takes tens of GB: downloads, mods and tool output such as DynDOLOD’s. Problems warns while it’s under 20 GB.' : ''}</p>` : '';
  const how = await dialog({
    title: `New instance for ${gameName(game, d?.version)}`,
    body: `<p>The instance folder holds your mods, profiles and undo history. On the game’s drive, deploys link mod files into the game folder instead of copying them.</p><p><b>${esc(suggested)}</b></p>${free}`,
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
  $('#game-name').textContent = gameName(i.game, i.game_version);
  $('#instance-path').textContent = i.root;
  $('#instance-path').title = i.root;
  $('#profile-name').textContent = i.profile;
  $('#undo').disabled = !S.history.length;
  $('#redo').disabled = !i.redo_steps;
  $('#undo').title = S.history.length ? `Undo: ${S.history[0]} (Ctrl+Z)` : 'Undo (Ctrl+Z)';
  $('#redo').title = i.redo_next ? `Redo: ${i.redo_next} (Ctrl+Y)` : 'Redo (Ctrl+Y)';
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
  endorsed: m => m.endorsed === 'Endorsed',
  tracked: m => !!m.tracked,
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
    else if ((!f || matchText(m, f)) && (!crit.length || (prefs.anyChips ? crit.some(c => c(m)) : crit.every(c => c(m))))) {
      if (head) { out.push(head); head = null; }
      out.push(m);
    }
  }
  return out;
}

/** The filter text: alternatives split by " | ", each a run of words that must all match; a
 *  word starting with "-" must not. */
function matchText(m, f) {
  return f.split(/\s+\|\s+/).some(alt => alt.split(/\s+/).filter(Boolean).every(w => w.length > 1 && w[0] === '-' ? !matches(m, w.slice(1)) : matches(m, w)));
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
  const upd = up ? `<button type="button" class="pill win" data-update title="Nexus has ${esc(up.file_name)}${up.latest ? ` (version ${esc(up.latest)})` : ''}: click to update">Update${up.latest ? ' ' + esc(up.latest) : ''}</button>`
    : m.pinned ? '<span class="pill plain" title="Held at this version: update checks don’t offer newer ones (the mod’s panel releases it)">Held</span>' : '';
  const cell = {
    conflicts: () => `<td class="conf">${relTag}${conflictPills(D.C[m.id])}${masters}</td>`,
    category: () => `<td class="cats" title="${esc(m.categories?.join(', '))}">${(m.categories ?? []).map(c => `<span class="cat" data-cat="${esc(c)}">${esc(c)}</span>`).join('')}</td>`,
    tags: () => `<td class="content">${(m.tags ?? []).map(t => `<span data-tag="${esc(t)}" title="${esc(CONTENT[t]?.[1] ?? t)}">${CONTENT[t]?.[0] ?? '•'}</span>`).join('')}</td>`,
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
      ${u ? '<button data-act="skip-update" title="Don’t offer this file; a later one is offered">Skip this version</button> <button data-act="nexus-update" class="primary" title="Download it from Nexus and update the mod in place">Update</button>' : ''}
    </div>
    ${u ? `<details class="changelog" data-mod="${u.mod_id}" data-since="${esc(m.version ?? '')}"><summary>What’s new</summary><div class="muted small">Loading…</div></details>` : ''}
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
    <td class="num idx">${p.hex}</td>
    <td class="num">${p.idx}</td></tr>`;
}

function renderPlugins() {
  const f = filterText();
  // The game numbers enabled full plugins 00, 01… and light ones FE 000, FE 001…
  let full = 0, light = 0;
  const hex = (n, w) => n.toString(16).toUpperCase().padStart(w, '0');
  S.plugins.forEach((p, i) => { p.idx = i; p.hex = !p.enabled ? '' : p.light ? `FE ${hex(light++, 3)}` : hex(full++, 2); });
  lists.plugins.items = f ? S.plugins.filter(p => p.name.toLowerCase().includes(f)) : S.plugins;
  $('#plugins').classList.toggle('filtering', !!f);
  $('#plugins-note').textContent = f
    ? `${lists.plugins.items.length.toLocaleString()} of ${plural(S.plugins.length, 'plugin')}`
    : 'Masters (ESM and ESL plugins, which others are built on) always load first. Drag to reorder; Ctrl+↑/↓ moves the selected ones, Space turns them on or off, right-click for more.';
  paint(lists.plugins);
}

function renderHistory() {
  const h = historyData;
  const list = h?.entries ?? S.history.map(label => ({ label }));
  const redo = h?.redo ?? [];
  if (!list.length && !redo.length) return $('#history').innerHTML = '<li class="muted">No changes to this profile yet.</li>';
  // Deployed and Played go where they happened among the changes, newest first.
  const marks = [['deployed', 'Deployed'], ['played', 'Played']].filter(([k]) => h?.[k]).map(([k, text]) => ({ time: h[k], text })).sort((a, b) => b.time - a.time);
  const mark = m => `<li class="mark">${m.text} · ${esc(when(m.time))}</li>`;
  const marksBefore = t => { let out = ''; while (marks.length && marks[0].time >= t) out += mark(marks.shift()); return out; };
  const undone = redo.map((label, j) => `<li class="redo"><div class="what"><span>↷ ${esc(label)}</span><span class="muted small">Undone. Redo brings it back ${j ? `after ${plural(j, 'other')}` : 'next'}.</span></div></li>`).reverse().join('');
  const rows = list.map((e, i) => `${e.time ? marksBefore(e.time) : ''}<li${i ? '' : ' class="cur"'}><div class="what"><span>${esc(e.label)}</span>${e.change ? `<span class="muted small">${esc(changeText(e.change)) || 'No change to the lists.'}</span>` : ''}${e.time ? `<span class="muted small">${esc(when(e.time))}</span>` : ''}</div>
    <button data-restore="${i}" title="Undo this change and every later one">Restore to before…</button></li>`).join('');
  const full = h && list.length >= h.limit ? `<li class="mark">History keeps the last ${h.limit} changes, so older ones were dropped. Settings › Undo steps kept per profile.</li>` : '';
  $('#history').innerHTML = undone + rows + marks.map(mark).join('') + full;
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
  if (shown('history')) renderHistory();
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
  const body = `<p>Goes back ${plural(+n + 1, 'change')}, to before “${esc(historyData?.entries[n]?.label ?? S.history[n])}”. Redo steps forward again.</p>`
    + part('to enable', d.enabled) + part('to disable', d.disabled) + part('to add back', d.added) + part('to put in the trash', d.removed) + part('to move', d.moved)
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
  box.hidden = !shown('mods') || !selected.size;
  if (box.hidden) return;
  const ids = selIds();
  if (ids.length > 1) return renderBulk(box, ids);
  const m = byId.get(ids[0]);
  if (m.separator) return renderSep(box, m);
  const c = D.C[m.id];
  const R = S.rules.find(r => r.id === m.id) ?? {};
  const ruleList = (key, title, placeholder) => `<h3>${title} (your rule)</h3>
    <div class="chips">${(R[key] ?? []).map(id => `<span class="chip">${esc(nameOf(id))}<button data-unrule="${esc(id)}" data-key="${key}" title="Remove from the rule" aria-label="Remove ${esc(nameOf(id))}">×</button></span>`).join('') || '<span class="muted">No rule.</span>'}</div>
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
    ${d?.meta?.web ? `<div class="row small">${d.meta.web.version ? `<span class="muted">Release ${esc(d.meta.web.version)}</span>` : ''}<button class="link" data-act="web-page" title="${esc(d.meta.web.page)}">${esc(new URL(d.meta.web.page).hostname.replace(/^www\./, ''))} page ↗</button></div>` : ''}
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
    ${d?.nexus_id ? needsBlock(d.needs) : ''}
    ${m.nexus_id && nexus.user ? accountBlock(m) : ''}
    ${d?.meta?.nexus ? nexusBlock(d.meta.nexus.mod_id) : ''}
    ${d?.docs?.length ? `<h3>Read me and settings</h3><div class="files">${d.docs.map(f => `<button class="link" data-doc="${esc(f)}" title="Read it">${esc(f)}</button>`).join('<br>')}</div>` : ''}
    ${d?.ini_tweaks?.length ? tweaksBlock(d.ini_tweaks) : ''}
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
    <div class="row"><button data-act="hide-files" title="Keep chosen files out of the game, through reinstalls and updates (MO2’s hide)">Hide files…</button><button data-act="move-files" title="Move chosen files into another mod">Move files…</button>${m.plugins.length ? '<button data-act="pack" class="adv" title="Pack its loose files into archives named after its plugin (MO2’s BSA packer)">Pack into archives…</button>' : ''}</div>
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

/** The mod's INI tweaks (its INI Tweaks folder). One that's on goes into the game's INIs at the next deploy. */
function tweaksBlock(tweaks) {
  const sets = t => t.settings.map(s => `${s.key}=${s.value}`);
  return `<h3>INI tweaks</h3><div class="stack">${tweaks.map(t => `<label class="row" style="margin:0" title="${esc(t.settings.map(s => `[${s.section}] ${s.key}=${s.value}`).join('\n'))}">
      <input type="checkbox" data-tweak="${esc(t.file)}"${t.enabled ? ' checked' : ''}> ${esc(t.file.replace(/\.ini$/i, ''))}
      <span class="muted small">${esc(sets(t).slice(0, 3).join(', '))}${sets(t).length > 3 ? ` and ${sets(t).length - 3} more` : ''}</span></label>`).join('')}</div>
    <p class="muted small">A tweak that’s on is written into the game’s INIs at the next deploy or Play. Turn it off and it comes out again.</p>`;
}

/** What the mod's Nexus page lists as required, and whether this setup has each. */
function needsBlock(needs) {
  if (!needs) return '<h3>Needs</h3><p class="muted small">What its Nexus page lists as required hasn’t been read yet.</p><div class="row"><button data-act="read-needs" title="No sign-in needed">Read it from Nexus</button></div>';
  if (!needs.length) return '<h3>Needs</h3><p class="muted small">Its Nexus page lists nothing it needs.</p>';
  const state = r => r.installed ? (S.mods.find(x => x.id === r.installed)?.enabled ? 'installed' : 'installed, turned off')
    : r.dlc ? 'a DLC' : r.mod_id ? 'not installed' : 'from another site';
  const name = r => r.installed ? `<button class="link" data-goto="${esc(r.installed)}">${esc(r.name)}</button>`
    : r.mod_id ? `<button class="link" data-page="${r.mod_id}" title="Open its Files tab on Nexus">${esc(r.name)}</button>` : esc(r.name);
  return `<h3>Needs</h3><ul class="small">${needs.map(r => `<li>${name(r)} <span class="muted">· ${state(r)}${r.notes ? ` · ${esc(r.notes)}` : ''}</span></li>`).join('')}</ul>
    <p class="muted small">From its Nexus page. Problems says when one is missing for this game version.</p>`;
}

/** Endorse and Track on Nexus, with the account's state as last read. Both act publicly. */
function accountBlock(m) {
  const state = m.endorsed === 'Endorsed' ? 'You endorsed it.' : m.endorsed === 'Abstained' ? 'You abstained from endorsing it.' : 'Not endorsed.';
  return `<h3>Your Nexus account</h3>
    <div class="row"><span class="small">${state}${m.tracked ? ' Tracked.' : ''}</span><div class="spacer"></div>
      ${m.endorsed === 'Endorsed' ? '<button data-act="abstain" title="Take the endorsement back (Nexus calls it abstaining)">Withdraw</button>'
        : `<button data-act="endorse" class="primary" title="Endorse it on Nexus, as your account">Endorse</button>${m.endorsed ? '' : ' <button data-act="abstain" title="Tell Nexus you won’t endorse it, so it stops asking">Abstain</button>'}`}
      <button data-act="${m.tracked ? 'untrack' : 'track'}" title="${m.tracked ? 'Stop tracking it' : 'Its updates show in your tracking centre on Nexus'}">${m.tracked ? 'Untrack' : 'Track'}</button></div>`;
}

/** Endorse, abstain, track or untrack mods on Nexus, then show the account's new state. */
async function nexusAccount(ids, what) {
  if (!await nexusReady('Endorsing and tracking act on your Nexus account.')) return;
  const [cmd, yes, verb] = { endorse: ['nexus_endorse', true, 'Endorsing'], abstain: ['nexus_endorse', false, 'Abstaining'], track: ['nexus_track', true, 'Tracking'], untrack: ['nexus_track', false, 'Untracking'] }[what];
  const ok = await act(`${verb} ${ids.length > 1 ? plural(ids.length, 'mod') : nameOf(ids[0])} on Nexus`, async () => {
    for (const id of ids) await call(cmd, { id, yes });
  });
  const done = { endorse: 'Endorsed', abstain: 'Abstained from endorsing', track: 'Now tracking', untrack: 'Stopped tracking' }[what];
  if (ok) toast(`${done} ${ids.length > 1 ? plural(ids.length, 'mod') : nameOf(ids[0])} on Nexus.`);
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
    ${lightRow(p)}
    <h3>Masters (${p.masters.length})</h3>
    ${p.masters.length ? '<p class="muted small">The plugins this one is built on. They have to be on and load before it.</p>' : ''}
    ${p.masters.length ? `<ul>${p.masters.map(m => `<li>${!lower.has(m.toLowerCase()) ? '' : loadsLater(m) ? '<span class="pill bad">Loads after it</span>' : '<span class="pill bad">Missing</span>'}<button class="link" data-plugin="${esc(m)}">${esc(m)}</button></li>`).join('')}</ul>` : '<p class="muted">None.</p>'}
    ${loaders.length ? `<h3>Needed by (${loaders.length})</h3><ul>${loaders.slice(0, 50).map(x => `<li><button class="link" data-plugin="${esc(x.name)}">${esc(x.name)}</button></li>`).join('')}</ul>` : ''}
    ${p.enabled && !p.implicit ? `<h3>Record conflicts</h3>${recordData?.name === p.name ? recordsBlock(recordData.rows) : `<p class="muted small">Other plugins that change the same records (NPCs, items, cells…): the game uses the version of the one that loads last. Reads every enabled plugin, so it can take a moment.</p><button data-act="records">Show record conflicts</button>`}` : ''}
    ${p.implicit ? '' : `<h3>LOOT’s order</h3>${whyData?.name === p.name ? whyBlock(whyData.r) : '<p class="muted small">What ties it to the plugins around it: masters, LOOT’s and your rules, groups, shared records. Runs a sort without saving it.</p>'}<div class="row"><button data-act="loot-why">${whyData?.name === p.name ? 'Ask again' : 'Why here?'}</button></div>`}
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

/** record_conflicts rows: the plugin the game takes each shared record from first. */
function recordsBlock(rows) {
  if (!rows.length) return '<p class="muted">No other enabled plugin changes its records.</p>';
  const kinds = k => Object.entries(k).sort((a, b) => b[1] - a[1]).slice(0, 5).map(([t, n]) => `${t} ${n.toLocaleString()}`).join(', ');
  return `<p class="muted small">Last to load first: where both change a record, the one higher here wins. An identical copy (ITPO) changes nothing, unless it undoes a third plugin’s change.</p><ul class="records">${rows.map(r => `<li>
    <button class="link" data-plugin="${esc(r.plugin)}">${esc(r.plugin)}</button>
    ${r.wins ? '<span class="pill lose" title="It loads later: the game uses its version of the shared records">wins</span>' : '<span class="pill win" title="It loads earlier: this plugin’s version wins">loses</span>'}${r.patch ? ' <span class="pill plain" title="The later plugin has the earlier as a master: a patch, overriding it on purpose">patch</span>' : ''}
    <div class="muted small">${plural(r.records, 'record')}${r.identical ? `, ${r.identical.toLocaleString()} identical` : ''} · ${esc(kinds(r.kinds))}</div>
    <details><summary class="small">Some of them</summary><pre>${esc(r.examples.join('\n'))}${r.records > r.examples.length ? `\n… and ${(r.records - r.examples.length).toLocaleString()} more` : ''}</pre></details></li>`).join('')}</ul>`;
}

async function loadRecords(name) {
  const rows = await act('Reading the plugins', () => call('record_conflicts', { plugin: name }), { reload: false });
  if (!rows || rows === true) return;
  recordData = { name, rows };
  if (pluginSide()) renderDetails();
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

/** Move a block so it starts at `at` in the list without it. `queue`: a key press, sent when
 * the keys stop. */
function moveTo(ids, at, queue = false) {
  const order = restOf(ids);
  order.splice(at, 0, ...ids);
  if (order.every((id, i) => id === S.mods[i].id)) return;
  if (busy && !queue) return busyNote();
  S.mods = order.map(id => byId.get(id));
  S.mods.forEach((m, i) => { m.priority = i; });
  derive();
  renderMods();
  if (queue) queueSend('mods', ids.join('\n'), () => call('move_mods', { ids, priority: S.mods.findIndex(m => m.id === ids[0]) }));
  else act('Moving', () => call('move_mods', { ids, priority: at }));
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
  moveTo(ids, at, true);
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

/** Files of the mod edited in rmm, which an update or reinstall replaces: ask first. */
async function editsOk(d, verb) {
  const ed = d?.edited ?? [];
  if (!ed.length) return true;
  const which = ed.length === 1 ? ed[0] : `${ed.length} of its files (${ed.slice(0, 4).join(', ')}${ed.length > 4 ? '…' : ''})`;
  return confirmBox(`${verb} ${nameOf(d.id)}?`, `You edited ${which} in rmm. ${verb === 'Update' ? 'Updating' : 'Reinstalling'} puts the archive’s files in their place. Undo brings your edited ones back; “rmm edits” lists what changed.`, `${verb} anyway`, 'primary');
}

/** A C# installer script is a program from the mod: it runs only when the user says so. */
const scriptOk = path => confirmBox(`Run ${basename(path)}’s installer?`,
  'This mod’s installer is a C# script: a small program its author wrote, which asks about its options in windows of its own and says which files to install. rmm runs it in a sandbox where it can read the mod’s files and the game’s Data and nothing else, then installs what it picked. It is still code from the download: run it only for a mod from a source you trust.',
  'Run the installer', 'primary');

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
  if (!await editsOk(d, 'Reinstall')) return;
  const run_script = !!walk.script;
  if (run_script) {
    if (!await scriptOk(path)) return;
  } else if (walk === true) {
    if (!await confirmBox(`Reinstall ${nameOf(id)}?`, `Its files are read again from ${path}. The old files go to the instance's trash folder, and Undo puts them back.`, 'Reinstall', 'primary')) return;
  } else {
    choices = await fomodWizard(path, walk);
    if (!choices) return;
  }
  const r = await act(`Reinstalling ${nameOf(id)}`, () => call('reinstall', { id, choices, run_script }));
  if (r) toast(`Reinstalled ${r.name} (${plural(r.files, 'file')}).${r.warnings.length ? '\n' + r.warnings.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
}

/** Replace a mod's files with a newer version; a FOMOD installer opens with the choices it was
 *  installed with. */
async function updateMod(id, path, nx = null) {
  const d = await call('details', { id }).catch(() => null);
  if (!await editsOk(d, 'Update')) return;
  const walk = await act(`Reading ${basename(path)}`, async () => {
    try { return await call('fomod', { path, choices: d?.meta?.fomod ?? null }); }
    catch { return call('fomod', { path }); } // the stored choices don't fit the new installer
  }, { reload: false, problem: true });
  if (!walk) return;
  let choices;
  const run_script = !!walk.script;
  if (run_script) {
    if (!await scriptOk(path)) return;
  } else if (walk !== true) {
    choices = walk.steps.length ? await fomodWizard(path, walk) : walk.choices;
    if (!choices) return;
  }
  const r = await act(`Updating ${nameOf(id)}`, () => call('update_mod', { id, path, choices, nexus: nx?.nexus, run_script }), { problem: true });
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
      const acts = (i.kind === 'esl_candidates' && !ign ? '<button class="link ign" data-light>Flag them light…</button>' : '')
        + (i.kind === 'missing_masters' && !ign ? (i.message.startsWith('loads before') ? '<button class="link ign" data-sort>Sort with LOOT…</button>' : `<button class="link ign" data-off="${esc(i.subject)}">Turn it off</button>`) : '')
        + (i.kind ? `<button class="link ign" data-ignore="${esc(k)}">${ign ? 'Stop ignoring' : 'Ignore'}</button>` : '')
        + (failures.includes(i) ? `<button class="link ign" data-dismiss="${failures.indexOf(i)}">Dismiss</button>` : '');
      return `<li class="${esc(i.severity)}${to ? ' link' : ''}${ign ? ' ignored' : ''}" data-key="${esc(k)}" ${to ? `data-mod="${esc(to.mod ?? '')}" data-plugin="${esc(to.plugin ?? '')}" title="Show it in the list"` : ''}>
        <span class="sev">${esc(i.severity)}</span><span class="subject">${esc(to?.mod ? nameOf(to.mod) : i.subject)}</span><span class="message">${i.kind === 'loot_message' ? markdown(i.message) : esc(i.message)}</span>
        ${acts ? `<span class="acts">${acts}</span>` : ''}</li>`;
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
  const info = shown.length - serious, lootRan = S.info.masterlist || all.some(i => i.kind === 'loot_message');
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
  const body = (r.plugins.length ? `<p>xEdit QuickAutoClean cleans ${plural(r.plugins.length, 'plugin')}, one run after another. The clean copies go into a mod above the originals, so Undo or turning that mod off brings the originals back.</p>
      <label class="row" style="margin:0 0 8px"><input type="checkbox" id="cd-one" ${prefs.cleanOneMod ?? true ? 'checked' : ''}> All in one mod, “Cleaned plugins” (otherwise a “Cleaned …” mod each)</label>
      <ul>${r.plugins.map(d => `<li><b>${esc(d.plugin)}</b> <span class="muted small">${esc(what(d))}${d.deleted_navmeshes ? '; its deleted navmeshes take xEdit by hand' : ''}</span></li>`).join('')}</ul>` : '<p>No plugin LOOT knows as dirty can be cleaned automatically.</p>')
    + (r.by_hand.length ? `<p class="muted small">Only deleted navmeshes, which QuickAutoClean can’t fix (xEdit by hand, or leave them): ${esc(r.by_hand.map(d => d.plugin).join(', '))}.</p>` : '')
    + '<p class="muted small">xEdit opens for each plugin and closes when done; this takes a while. ITMs are records identical to the master’s; deleted references can crash the game when another mod uses them.</p>';
  if (!r.plugins.length) return dialog({ title: 'Dirty plugins', body, actions: [['cancel', 'Close']] });
  if (await dialog({ title: 'Clean dirty plugins', body, actions: [['cancel', 'Cancel'], ['ok', `Clean ${plural(r.plugins.length, 'plugin')}`, 'primary']] }) !== 'ok') return;
  const one_mod = prefs.cleanOneMod = $('#cd-one').checked;
  savePrefs();
  const done = await act('Cleaning plugins', () => call('clean_dirty', { one_mod }), { problem: true });
  if (!done || done === true) return;
  const part = (title, xs) => xs.length ? `<p><b>${esc(title)}</b></p><ul class="small">${xs.map(x => `<li>${esc(x)}</li>`).join('')}</ul>` : '';
  await dialog({
    title: 'Cleaning done',
    body: part('Cleaned', done.mods.map(([p, m]) => m ? `${p} → ${nameOf(m)}` : `${p} (xEdit found nothing to remove)`)) + part('Couldn’t clean', done.failed.map(([p, e]) => `${p}: ${e}`)) + part('LOOT still lists as dirty', done.still_dirty)
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
  if (t === 'plugins') t = 'mods'; // the plugins show beside the mods
  if (prefs.floating[t] && $('#win-' + t)) return frontWin(t);
  tab = t;
  prefs.tab = t;
  savePrefs();
  const split = wasSplit = splitOn();
  $$('.tabs [data-tab]').forEach(b => { b.classList.toggle('on', b.dataset.tab === t); b.classList.toggle('floating', !!prefs.floating[b.dataset.tab]); });
  for (const p of TAB_IDS) if (!prefs.floating[p]) $('#pane-' + p).hidden = p !== t && !(split && t === 'mods' && p === 'plugins');
  $('main').classList.toggle('split', split && t === 'mods');
  $('#mp-grip').hidden = t !== 'mods';
  $('#filter').hidden = !shown('mods') && !shown('plugins');
  if (split && t === 'mods') paint(lists.plugins);
  renderDetails();
  fillPane(t);
}

/** Draw a pane that just came into view, reading what it shows when it needs to. */
function fillPane(t) {
  if (lists[t]) paint(lists[t]);
  if (t === 'mods') renderTicks();
  if (t === 'history' && !historyData) loadHistory();
  if (t === 'problems' && issuesStale) runHealth();
  if (t === 'tools' && !toolsData) loadTools();
  if (t === 'plugins' && selPlugin && !lootRules) loadLootRules();
  if (t === 'data' && !dataLoaded) loadData();
  if (t === 'saves') loadSaves(); // the game writes them; read them each time
  if (t === 'downloads' && !downloadsData) loadDownloads();
  if (t === 'mcp') { drawMcp(); drawMore(); }
  if (t === 'settings') drawMore();
}

// ---- Tabs as windows inside rmm's window: drag a tab off the tab bar (or right-click it) to pull
// it out; the window moves by its title bar and resizes from its corner. Dropped on the left, right
// or bottom edge it docks there (prefs.docked), its edge dragging to resize; Tab puts it back. ----

const DOCK_MIN = { left: 220, right: 220, bottom: 120 };

/** Keep a window's title bar on screen. */
function fitWin(r) {
  const top = $('nav.tabs').getBoundingClientRect().bottom;
  r.w = Math.min(Math.max(r.w, 320), innerWidth);
  r.h = Math.min(Math.max(r.h, 200), innerHeight - top);
  r.x = Math.min(Math.max(r.x, 80 - r.w), innerWidth - 80);
  r.y = Math.min(Math.max(r.y, top), innerHeight - 32);
  return r;
}

function frontWin(t) {
  const w = $('#win-' + t);
  if (!w || prefs.docked[t]) return;
  w.style.zIndex = ++winTop > 39 ? (winTop = 31) : winTop;
  if (winTop === 31) $$('.float-win').forEach(o => { if (o !== w) o.style.zIndex = 30; });
}

/** Pull tab `t` out into a window, at `at` (a point) or where it was last; `side` docks it. */
function floatTab(t, at, side) {
  if (MAIN_ONLY.includes(t)) return toast('The mods and plugins stay in the main window.');
  if ($('#win-' + t)) return side ? dockWin(t, side) : frontWin(t);
  const docked = visibleTabs().filter(x => !prefs.floating[x]);
  if (docked.length <= 1) return toast('One tab stays in the main window.');
  const was = prefs.floating[t] ?? prefs.lastWin?.[t];
  prefs.floating[t] = fitWin(at ? { x: at.x, y: at.y, w: was?.w ?? 760, h: was?.h ?? 480 } : { ...(was ?? { x: 160, y: 140, w: 760, h: 480 }) });
  if (side) prefs.docked[t] = side;
  savePrefs();
  makeWin(t);
  showTab(tab === t ? docked.find(x => x !== t) : tab);
}

/** Follow the pointer until the button comes up, wherever the element under it goes meanwhile. */
function follow(e, move, up) {
  e.preventDefault();
  const end = u => { removeEventListener('pointermove', move); removeEventListener('pointerup', end); up(u); };
  addEventListener('pointermove', move);
  addEventListener('pointerup', end);
}

/** The dock area a point is over: near main's left or right edge, or its bottom. */
function dockSide(x, y) {
  const m = $('main').getBoundingClientRect(), E = 48;
  if (y >= m.bottom - E) return 'bottom';
  if (y > m.top) return x < m.left + E ? 'left' : x > m.right - E ? 'right' : null;
  return null;
}

/** Show where a drop docks (side), or nothing (null). */
function dockHint(side) {
  let h = $('.dock-hint');
  if (!side) return h?.remove();
  h ??= document.body.appendChild(Object.assign(document.createElement('div'), { className: 'dock-hint' }));
  const reg = $('#dock-' + side), m = $('main').getBoundingClientRect(), s = prefs.dockSize[side];
  const r = !reg.hidden ? reg.getBoundingClientRect()
    : side === 'left' ? { left: m.left, top: m.top, width: s, height: m.height }
    : side === 'right' ? { left: m.right - s, top: m.top, width: s, height: m.height }
    : { left: m.left, top: m.bottom - s, width: m.width, height: s };
  Object.assign(h.style, { left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px' });
}

function sizeDock(side) {
  const reg = $('#dock-' + side);
  const most = side === 'bottom' ? innerHeight - 260 : innerWidth / 2;
  const s = prefs.dockSize[side] = Math.round(Math.max(DOCK_MIN[side], Math.min(prefs.dockSize[side], most)));
  reg.style[side === 'bottom' ? 'height' : 'width'] = s + 'px';
}

/** Put window `t` where prefs say: in its dock area, or floating over the window. */
function placeWin(t, win = $('#win-' + t)) {
  const side = prefs.docked[t], r = prefs.floating[t];
  win.classList.toggle('docked', !!side);
  $('[data-float]', win).hidden = !side;
  if (side) {
    win.removeAttribute('style');
    $('#dock-' + side).append(win);
  } else {
    Object.assign(win.style, { left: r.x + 'px', top: r.y + 'px', width: r.w + 'px', height: r.h + 'px' });
    document.body.append(win);
    frontWin(t);
  }
  $$('.dock').forEach(d => { d.hidden = !$('.float-win', d); });
}

/** Drag window t's edge or corner (`dir`: n, s, e, w or two of them). */
function resizeWin(t, e, dir) {
  if (e.button !== 0) return;
  const r = prefs.floating[t], win = $('#win-' + t), at = { ...r }, x0 = e.clientX, y0 = e.clientY;
  follow(e, m => {
    const dx = m.clientX - x0, dy = m.clientY - y0;
    if (dir.includes('e')) r.w = Math.max(320, at.w + dx);
    if (dir.includes('s')) r.h = Math.max(200, at.h + dy);
    if (dir.includes('w')) { r.w = Math.max(320, at.w - dx); r.x = at.x + at.w - r.w; }
    if (dir.includes('n')) { r.h = Math.max(200, at.h - dy); r.y = Math.max($('nav.tabs').getBoundingClientRect().bottom, at.y + at.h - r.h); r.h = at.y + at.h - r.y; }
    Object.assign(win.style, { left: r.x + 'px', top: r.y + 'px', width: r.w + 'px', height: r.h + 'px' });
  }, savePrefs);
}

function dockWin(t, side) {
  prefs.docked[t] = side;
  savePrefs();
  placeWin(t);
}

function makeWin(t) {
  const win = document.createElement('div');
  win.className = 'float-win';
  win.id = 'win-' + t;
  win.innerHTML = `<div class="float-bar" title="Drag to move; drop on the left, right or bottom edge to dock"><span>${esc(tabLabel(t))}</span><div class="spacer"></div><button type="button" class="link" data-float title="Make it a window again">Window</button><button type="button" class="link" data-dock title="Put it back in the main window as a tab">Tab</button></div>`;
  const pane = $('#pane-' + t);
  pane.hidden = false;
  win.append(pane);
  win.insertAdjacentHTML('beforeend', ['n', 's', 'e', 'w', 'ne', 'nw', 'se', 'sw'].map(d => `<i class="rz rz-${d}" data-rz="${d}"></i>`).join(''));
  $$('[data-rz]', win).forEach(h => h.addEventListener('pointerdown', e => resizeWin(t, e, h.dataset.rz)));
  win.addEventListener('pointerdown', () => frontWin(t), true);
  $('[data-dock]', win).onclick = () => dockTab(t);
  $('[data-float]', win).onclick = () => { delete prefs.docked[t]; savePrefs(); placeWin(t); };
  const bar = $('.float-bar', win);
  bar.addEventListener('pointerdown', e => {
    if (e.button !== 0 || e.target.closest('button')) return;
    const r = prefs.floating[t], x0 = e.clientX, y0 = e.clientY;
    let dx = x0 - r.x, dy = y0 - r.y, moved = false;
    follow(e, m => {
      if (prefs.docked[t]) { // pulled out of its dock area: a window again, under the pointer
        if (Math.hypot(m.clientX - x0, m.clientY - y0) < 8) return;
        delete prefs.docked[t];
        [dx, dy] = [60, 14];
        placeWin(t);
      }
      moved = true;
      Object.assign(r, fitWin({ ...r, x: m.clientX - dx, y: m.clientY - dy }));
      win.style.left = r.x + 'px';
      win.style.top = r.y + 'px';
      dockHint(dockSide(m.clientX, m.clientY));
    }, u => {
      dockHint(null);
      const side = moved && dockSide(u.clientX, u.clientY);
      if (side) dockWin(t, side);
      else savePrefs();
    });
  });
  // Its edges resize it: keep the size, and draw the rows that come into view.
  let saveSoon;
  new ResizeObserver(() => {
    const r = prefs.floating[t];
    if (!r || !win.isConnected) return;
    if (!prefs.docked[t]) [r.w, r.h] = [win.offsetWidth, win.offsetHeight];
    if (lists[t]) paint(lists[t]);
    clearTimeout(saveSoon);
    saveSoon = setTimeout(savePrefs, 300);
  }).observe(win);
  placeWin(t, win);
  if (S) fillPane(t);
}

function dockTab(t) {
  const win = $('#win-' + t);
  if (!win) return;
  (prefs.lastWin ??= {})[t] = prefs.floating[t];
  delete prefs.floating[t];
  delete prefs.docked[t];
  savePrefs();
  $('main').insertBefore($('#pane-' + t), $('#details'));
  win.remove();
  $$('.dock').forEach(d => { d.hidden = !$('.float-win', d); });
  showTab(t);
}

// The edge between the mods and the plugins drags to share the width between them.
const sizePlugins = () => { if (prefs.plugW) $('#pane-plugins').style.flex = `0 1 ${prefs.plugW}px`; };
sizePlugins();
$('#mp-grip').addEventListener('pointerdown', e => {
  if (e.button !== 0) return;
  const mods = $('#pane-mods').getBoundingClientRect(), right = $('#pane-plugins').getBoundingClientRect().right;
  follow(e, m => {
    prefs.plugW = Math.round(Math.min(Math.max(right - m.clientX, 300), right - mods.left - 360));
    sizePlugins();
    paint(lists.mods);
    paint(lists.plugins);
  }, savePrefs);
});
$('#mp-grip').ondblclick = () => { delete prefs.plugW; $('#pane-plugins').style.flex = ''; savePrefs(); paint(lists.mods); paint(lists.plugins); };

// A dock area's inner edge drags to resize it.
for (const side of ['left', 'right', 'bottom']) {
  const reg = $('#dock-' + side);
  sizeDock(side);
  $('.dock-grip', reg).addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    const r = reg.getBoundingClientRect();
    follow(e, m => {
      prefs.dockSize[side] = side === 'left' ? m.clientX - r.left : side === 'right' ? r.right - m.clientX : r.bottom - m.clientY;
      sizeDock(side);
    }, savePrefs);
  });
}

const DOCK_MENU = t => [['Dock on the left', () => floatTab(t, null, 'left')], ['Dock on the right', () => floatTab(t, null, 'right')], ['Dock at the bottom', () => floatTab(t, null, 'bottom')]];

// Drag a tab off the tab bar to pull it out; a frame shows where its window will go, or the dock
// area it lands in.
$$('.tabs [data-tab]').forEach(b => {
  b.addEventListener('pointerdown', e => {
    if (e.button !== 0 || prefs.floating[b.dataset.tab] || MAIN_ONLY.includes(b.dataset.tab)) return;
    const t = b.dataset.tab, x0 = e.clientX, y0 = e.clientY, bar = $('nav.tabs').getBoundingClientRect();
    let ghost = null;
    const size = prefs.lastWin?.[t] ?? { w: 760, h: 480 };
    b.setPointerCapture(e.pointerId); // moves off the button still come here
    b.onpointermove = m => {
      if (!ghost && Math.hypot(m.clientX - x0, m.clientY - y0) < 12) return;
      if (!ghost) {
        ghost = document.body.appendChild(Object.assign(document.createElement('div'), { className: 'float-ghost' }));
        ghost.style.width = size.w + 'px';
        ghost.style.height = size.h + 'px';
      }
      const side = dockSide(m.clientX, m.clientY);
      dockHint(side);
      ghost.hidden = !!side || m.clientY < bar.bottom; // dropped back on the tab bar: nothing happens
      ghost.style.left = m.clientX - 60 + 'px';
      ghost.style.top = m.clientY - 14 + 'px';
    };
    b.onpointerup = u => {
      b.onpointermove = b.onpointerup = null;
      if (!ghost) return;
      const side = dockSide(u.clientX, u.clientY), out = side || !ghost.hidden;
      ghost.remove();
      dockHint(null);
      // The click that follows would switch to the tab.
      b.addEventListener('click', c => c.stopImmediatePropagation(), { capture: true, once: true });
      if (out) floatTab(t, { x: u.clientX - 60, y: u.clientY - 14 }, side);
    };
  });
  b.addEventListener('contextmenu', e => {
    const t = b.dataset.tab, out = !!prefs.floating[t];
    if (MAIN_ONLY.includes(t)) return;
    const items = out ? [['Put back in the main window', () => dockTab(t)], prefs.docked[t] ? ['Make it a window', () => $(`#win-${t} [data-float]`).click()] : ['Bring to front', () => frontWin(t)]] : [['Open in a window', () => floatTab(t)]];
    ctxMenu(e, [...items, ...DOCK_MENU(t).filter((_, i) => !out || ['left', 'right', 'bottom'][i] !== prefs.docked[t])]);
  });
});
addEventListener('resize', () => {
  ['left', 'right', 'bottom'].forEach(sizeDock);
  for (const [t, r] of Object.entries(prefs.floating)) {
    const w = $('#win-' + t);
    if (!w || prefs.docked[t]) continue;
    fitWin(r);
    Object.assign(w.style, { left: r.x + 'px', top: r.y + 'px', width: r.w + 'px', height: r.h + 'px' });
  }
});
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
    ${r.not_yours?.length ? `<div class="not-yours"><b>This log may not be from this setup.</b><ul>${r.not_yours.map(s => `<li>${esc(s)}</li>`).join('')}</ul></div>` : ''}
    ${r.error ? `<p><b>${esc(r.error)}</b></p>` : ''}
    ${rows ? `<p>Named in the log, most likely first:</p><table class="fc">${rows}</table>` : '<p class="muted">The log names nothing that mods provide.</p>'}
    ${r.next_steps.length ? `<h3>What to try</h3><ul>${r.next_steps.map(s => `<li>${esc(s)}</li>`).join('')}</ul>` : ''}
    ${r.call_stack.length ? `<details><summary>Call stack</summary><pre>${esc(r.call_stack.join('\n'))}</pre></details>` : ''}
    <div class="row"><button type="button" id="copy-report" title="A report to paste into a forum post or chat, without your Windows user name">Copy report</button><button type="button" id="save-bundle" title="A zip of the setup and its logs to attach to a post, without your Windows user name">Save support bundle…</button><button type="button" id="find-culprit" title="Turn off half of the mods at a time and play, until one is left">Find the culprit…</button><span class="muted small" id="crash-note" role="status"></span></div>
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
  await done;
  $('#dlg').onclick = null;
}
$('#crash-log').onclick = () => crashLog(null);
$('#support-bundle').onclick = async () => {
  const b = await saveBundle(null, t => toast(t, 'error'));
  if (b) toast(`Saved ${b.path}. Your Windows user name is taken out; look inside before you share it.`);
};

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
  if (e.target.closest('[data-sort]')) return void lootSort();
  if (e.target.closest('[data-light]')) return void flagLight();
  const off = e.target.closest('[data-off]')?.dataset.off;
  if (off) return void setPluginsEnabled([off], false);
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
const drawFmode = () => { $('#fmode').textContent = prefs.anyChips ? 'Any of them' : 'All of them'; };
drawFmode();
$('#fmode').onclick = () => { prefs.anyChips = !prefs.anyChips; savePrefs(); drawFmode(); filterChanged(); };

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
    if (plugVis() && (side === 'plugins' || !shown('mods'))) pluginKeys(e);
    else if (shown('mods')) modKeys(e);
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
  else if (e.target.closest('[data-update]')) updateFromNexus(id);
  else if (e.target.closest('[data-tag]') && !e.shiftKey && !e.ctrlKey) {
    $('#filter').value = `#${e.target.closest('[data-tag]').dataset.tag.split(' ')[0]}`;
    filterChanged();
  } else if (e.target.closest('.cat') && !e.shiftKey && !e.ctrlKey) {
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
/** What kind of file a conflict is over, in a player's words. */
function fileKind(f) {
  const l = f.toLowerCase();
  return /\.(pex|psc)$/.test(l) ? 'scripts' : /\.dll$/.test(l) ? 'script extender plugins' : /\.es[pml]$/.test(l) ? 'plugins'
    : /\.hkx$/.test(l) ? 'animations' : /^interface\/|\.swf$/.test(l) ? 'menus' : /\.(dds|png|tga)$/.test(l) ? 'textures'
    : /\.(nif|tri|btr|bto)$/.test(l) ? 'meshes' : /\.(wav|xwm|fuz|lip)$/.test(l) ? 'sounds'
    : /\.(ini|json|toml|yaml|yml|xml|txt)$/.test(l) ? 'settings files' : 'other files';
}
const LOOKS = new Set(['textures', 'meshes', 'sounds']);
const ONE_KIND = { scripts: 'script', 'script extender plugins': 'script extender plugin', plugins: 'plugin', animations: 'animation', menus: 'menu', textures: 'texture', meshes: 'mesh', sounds: 'sound', 'settings files': 'settings file', 'other files': 'other file' };

/** One line per mod `id` shares files with: what kinds, who wins, and whether that's expected
 *  (a patch over what it patches, or two replacers of the same looks) or worth a look. */
function conflictSummary(id, all) {
  const norm = s => s.toLowerCase().replace(/[^a-z0-9]/g, '');
  const patchy = n => /patch|fix|compat|tweak|add-?on|plugin for/i.test(n);
  const by = new Map(); // other mod -> { kinds: Map, wins, loses }
  for (const f of all) {
    const others = f.winner === id ? f.mods.filter(m => m !== id) : [f.winner];
    for (const o of others) {
      const e = by.get(o) ?? { kinds: new Map(), wins: 0, loses: 0 };
      const k = fileKind(f.file);
      e.kinds.set(k, (e.kinds.get(k) ?? 0) + 1);
      f.winner === id ? e.wins++ : e.loses++;
      by.set(o, e);
    }
  }
  const me = nameOf(id);
  const rows = [...by].sort((a, b) => (b[1].wins + b[1].loses) - (a[1].wins + a[1].loses)).slice(0, 10).map(([o, e]) => {
    const other = nameOf(o), kinds = [...e.kinds].sort((a, b) => b[1] - a[1]);
    const what = kinds.map(([k, n]) => `${n.toLocaleString()} ${n === 1 ? ONE_KIND[k] : k}`).join(', ');
    const who = !e.loses ? 'this mod wins' : !e.wins ? 'it wins' : `each wins some (this mod ${e.wins.toLocaleString()}, it ${e.loses.toLocaleString()})`;
    // A patch names what it patches, or says it's a patch; it should win.
    const meP = patchy(me) || norm(me).includes(norm(other)), otherP = patchy(other) || norm(other).includes(norm(me));
    let ok, why;
    if (meP !== otherP) {
      const patch = meP ? 'This mod looks like a patch for it' : 'It looks like a patch for this mod', winsIt = meP ? !e.loses : !e.wins;
      [ok, why] = winsIt ? [true, `${patch}, and wins, as a patch should.`] : [false, `${patch}, but loses: move the patch below the other.`];
    } else if (kinds.every(([k]) => LOOKS.has(k))) {
      [ok, why] = [true, 'Both change how the same things look or sound. Whichever is lower in the list shows; order them for the look you want.'];
    } else {
      const risky = kinds.map(([k]) => k).filter(k => !LOOKS.has(k));
      [ok, why] = [false, `Only the winner’s ${risky.join(' and ')} are used, so the other mod may not work fully. Its page may name a patch, or the order it wants.`];
    }
    return `<li><b>${esc(other)}</b>: ${esc(what)}; ${esc(who)}. <span class="pill ${ok ? 'win' : 'lose'}">${ok ? 'Expected' : 'Worth a look'}</span> <span class="muted">${esc(why)}</span></li>`;
  });
  return `<ul class="fc-sum small">${rows.join('')}</ul>${by.size > 10 ? `<p class="muted small">And ${plural(by.size - 10, 'more mod')}.</p>` : ''}`;
}

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
    // Hide: this mod's own loose copy (one in an archive can't be hidden on its own).
    const hide = x => x.archives?.[x.mods.indexOf(id)] ? '' : `<button class="link small" data-hide="${esc(x.file)}" title="Keep ${esc(nameOf(id))}’s copy out of the game">Hide</button>`;
    return `<details open><summary><b>${m.length.toLocaleString()}</b> ${title}</summary><table class="fc">${m.slice(0, MAX).map(x => `<tr><td>${esc(x.file)}</td><td>${chain(x)}</td><td>${hide(x)}</td></tr>`).join('')}</table>`
      + (m.length > MAX ? `<p class="muted small">${(m.length - MAX).toLocaleString()} more; filter to narrow them down.</p>` : '') + '</details>';
  };
  const done = dialog({
    title: `File conflicts: ${nameOf(id)}`,
    body: `<p class="small">When two mods have the same file, the game uses one: the mod lower in the list wins, and a loose file beats one packed in an archive.</p>${conflictSummary(id, all)}
      <p class="muted small">Each file below lists the mods that have it, in load order; the copy the game uses is in bold.</p><input id="fc-filter" type="search" placeholder="Filter files" spellcheck="false"><div id="fc"></div>`,
    actions: [['cancel', 'Close']],
  });
  $('#dlg').classList.add('wide');
  const draw = () => {
    const f = $('#fc-filter').value.trim().toLowerCase();
    $('#fc').innerHTML = part('winning', win, f) + part('losing', lose, f);
  };
  $('#fc-filter').oninput = draw;
  $('#fc').onclick = e => {
    const f = e.target.closest('[data-hide]')?.dataset.hide;
    if (!f) return;
    $('#dlg').close();
    setTimeout(() => hideFiles(id, [f]));
  };
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
    showDoc(id, t.dataset.doc);
  } else if (t.dataset.pic) {
    const url = t.src;
    dialog({ title: t.dataset.pic, body: `<img src="${url}" alt="" style="max-width:100%">`, actions: [['ok', 'Close', 'primary']] });
  } else if (t.dataset.page) {
    call('open_nexus_mod', { mod_id: Number(t.dataset.page) }).catch(err => toast(String(err), 'error'));
  } else if (t.dataset.uncat) {
    editCategories([id], [], [t.dataset.uncat]);
  } else if (t.dataset.unrule) {
    const key = t.dataset.key;
    setModRule(id, key, (S.rules.find(r => r.id === id)?.[key] ?? []).filter(a => a !== t.dataset.unrule));
  } else switch (t.dataset.act) {
    case 'records': return loadRecords(selPlugin);
    case 'light-on': return flagLight([selPlugin]);
    case 'light-off': return act('Removing the light flag', () => call('set_light', { plugin: selPlugin, light: false })).then(() => { lightCands = null; });
    case 'pack': return packDialog(id);
    case 'loot-why': return loadWhy(selPlugin);
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
    case 'update-mod': return updateFromFile(id);
    case 'nocolor': return act('Changing the colour', () => call('edit_separator', { id, color: '' }));
    case 'mod-nocolor': return act('Changing the colour', () => call('set_color', { id, color: '' }));
    case 'pin':
      if (busy) { busyNote(); return e.preventDefault(); }
      return act(t.checked ? 'Holding its updates' : 'Offering its updates again', () => call('hold_updates', { ids: [id], hold: t.checked ? 'pin' : 'release' }));
    case 'skip-update': return act('Skipping the update', () => call('hold_updates', { ids: [id], hold: 'skip' }));
    case 'nexus-update': return updateFromNexus(id);
    case 'endorse': case 'abstain': case 'track': case 'untrack': return nexusAccount([id], t.dataset.act);
    case 'unskip': return act('Offering its updates again', () => call('hold_updates', { ids: [id], hold: 'release' }));
    case 'files': return fileConflicts(id);
    case 'web-page': return call('open_page', { url: details.meta.web.page }).catch(err => toast(String(err), 'error'));
    case 'nexus-page': return call('open_nexus_mod', { mod_id: details.meta.nexus.mod_id, page: true }).catch(err => toast(String(err), 'error'));
    case 'read-needs': return act('Reading its requirements on Nexus', () => call('read_requirements', { mod_id: details.nexus_id }));
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
    case 'hide-files': return hideFilesDialog(id);
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
  if (t.dataset.tweak !== undefined) return act(t.checked ? 'Turning the INI tweak on' : 'Turning the INI tweak off', () => call('set_ini_tweak', { id, file: t.dataset.tweak, on: t.checked }));
  if (t.dataset.act === 'mod-color') act('Changing the colour', () => call('set_color', { id, color: t.value }));
  if (t.classList.contains('cat-add') && t.value.trim()) return editCategories(selIds().filter(i => !isSep(i)), [t.value.trim()], []);
  if (t.dataset.act === 'lock') act(t.checked ? 'Locking the plugin' : 'Unlocking the plugin', () => call('lock_plugin', { name: selPlugin, lock: t.checked }));
  if (!t.classList.contains('rule-add') || !t.value.trim()) return;
  const other = idByName.get(t.value.trim()), key = t.dataset.key;
  if (!other) return toast(`No mod is named “${t.value.trim()}”.`, 'warn');
  setModRule(id, key, [...(S.rules.find(r => r.id === id)?.[key] ?? []), other]);
});

/** The update the last check found, from Nexus: Premium downloads it and updates the mod in
 *  place; free accounts get the mod's Files tab, whose Mod manager download comes back here. */
async function updateFromNexus(id) {
  const u = byId.get(id)?.update;
  if (!u || !await nexusReady('Updates from Nexus Mods need your API key.')) return;
  if (!nexus.user?.is_premium) {
    toast(`Free accounts download from the website: on its Files tab, click “Mod manager download” on ${u.file_name}. With rmm opening Nexus links, it comes back here as an update of ${nameOf(id)}.`);
    return call('open_nexus_mod', { mod_id: u.mod_id }).catch(err => toast(String(err), 'error'));
  }
  nxmQueue.push({ link: `${u.mod_id}/${u.file_id}`, at: null, update: id });
  runQueue();
}

/** Fill a <details class="changelog"> with Nexus's notes for the versions after the installed one. */
async function loadChangelog(el) {
  const out = el.querySelector('div');
  if (!nexus.user) return void (out.textContent = 'Sign in to Nexus to read it.');
  if (el.dataset.loaded) return;
  el.dataset.loaded = 1;
  try {
    const log = await call('nexus_changelog', { mod_id: +el.dataset.mod, since: el.dataset.since || null });
    out.className = 'small';
    out.innerHTML = log.length ? log.map(([v, lines]) => `<b>${esc(v)}</b><ul>${lines.map(l => `<li>${esc(l)}</li>`).join('')}</ul>`).join('') : '<span class="muted">The author left no changelog for it.</span>';
  } catch (err) { out.textContent = `Couldn’t read the changelog: ${err}`; }
}
document.addEventListener('toggle', e => { if (e.target.matches?.('details.changelog') && e.target.open) loadChangelog(e.target); }, true);

async function updateFromFile(id) {
  const path = await T.dialog.open({ title: `The new version of ${nameOf(id)}`, filters: [{ name: 'Mod archives', extensions: ['zip', '7z', 'rar'] }] });
  return path && updateMod(id, path);
}

async function hideFilesDialog(id) {
  const files = await call('mod_files', { id }).catch(err => { toast(String(err), 'error'); return null; });
  const paths = files && await pickFiles(`Hide files in ${nameOf(id)}`, files, '<p class="muted small">Ticked files leave the game at the next deploy and stay hidden through reinstalls and updates; the panel lists them to show again. A hidden plugin is like MO2’s optional ESP.</p>', 'Hide');
  return paths?.length && hideFiles(id, paths);
}

async function notesDialog(id) {
  const body = `<textarea id="nd-text" rows="6" spellcheck="true" placeholder="Notes for this mod…" style="width:100%">${esc(byId.get(id)?.notes ?? '')}</textarea>`;
  const done = dialog({ title: `Notes: ${nameOf(id)}`, body, actions: [['cancel', 'Cancel'], ['ok', 'Save', 'primary']] });
  $('#nd-text').focus();
  if (await done !== 'ok') return;
  act('Saving the notes', () => call('set_note', { id, note: $('#nd-text').value }));
}

/** A row colour for mods and separators; a separator's tints its group. */
async function colourDialog(ids) {
  const first = byId.get(ids[0])?.color ?? '#808080';
  const body = `<p>${ids.length > 1 ? `The ${plural(ids.length, 'row')}’ colour` : `${esc(nameOf(ids[0]))}’s colour`}:</p><input type="color" id="cd-color" value="${esc(first)}" aria-label="Colour">`;
  const r = await dialog({ title: 'Colour', body, actions: [['cancel', 'Cancel'], ['none', 'No colour'], ['ok', 'Set', 'primary']] });
  if (r !== 'ok' && r !== 'none') return;
  const color = r === 'ok' ? $('#cd-color').value : '';
  act('Changing the colour', () => call('set_color', { ids, color }));
}

/** Add a category to the mods, or take off ones they have. */
async function categoriesDialog(ids) {
  const have = [...new Set(ids.flatMap(i => byId.get(i)?.categories ?? []))].sort((a, b) => a.localeCompare(b));
  const body = `<label class="field">Add<input id="cg-add" list="cat-names" spellcheck="false" placeholder="A category, new or one you use"></label>
    ${have.length ? `<p class="small">Take off:</p>${have.map(c => `<label class="row" style="margin:0"><input type="checkbox" class="cg-off" value="${esc(c)}"> ${esc(c)}</label>`).join('')}` : ''}`;
  const done = dialog({ title: ids.length > 1 ? `Categories of ${plural(ids.length, 'mod')}` : `Categories: ${nameOf(ids[0])}`, body, actions: [['cancel', 'Cancel'], ['ok', 'Apply', 'primary']] });
  $('#cg-add').focus();
  if (await done !== 'ok') return;
  const add = $('#cg-add').value.trim();
  editCategories(ids, add ? [add] : [], $$('.cg-off').filter(c => c.checked).map(c => c.value));
}

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
  return act(names.length > 1 ? (enabled ? 'Enabling the plugins' : 'Disabling the plugins') : 'Changing the plugin', () => call('enable_plugins', { names, enabled }));
}

/** Put `names`, in their order, at `at` in the load order without them. */
function placePlugins(names, at, queue = false) {
  const locked = names.filter(n => D.locks.has(n.toLowerCase()));
  if (locked.length) return toast(`${locked.join(', ')} ${locked.length > 1 ? 'are' : 'is'} locked at ${locked.length > 1 ? 'their positions' : 'its position'}. Untick “Lock at this position” in the plugin’s panel to move it.`);
  const order = pluginsWithout(names);
  order.splice(at, 0, ...names);
  const now = S.plugins.filter(p => !p.implicit).map(p => p.name);
  if (order.every((n, i) => n === now[i])) return;
  if (!queue) return act(names.length > 1 ? 'Moving the plugins' : 'Moving the plugin', () => call('plugin_order', { names: order, moved: names }));
  const byName = new Map(S.plugins.map(p => [p.name, p]));
  S.plugins = [...S.plugins.filter(p => p.implicit), ...order.map(n => byName.get(n))];
  renderPlugins();
  queueSend('plugins', names.join('\n'), () => call('plugin_order', { names: S.plugins.filter(p => !p.implicit).map(p => p.name), moved: names }));
}

/** Ctrl+Up/Down: the selected plugins one place, together. */
function nudgePlugins(dir) {
  const names = selPlugins();
  if (!names.length) return;
  const first = S.plugins.filter(p => !p.implicit).map(p => p.name).indexOf(names[0]);
  placePlugins(names, Math.max(0, Math.min(pluginsWithout(names).length, first + dir)), true);
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
  if (one && !m.separator) {
    items.push(['Rename…', () => renameMod(id)], ['Reinstall…', () => reinstall(id)]);
    if (m.update) items.push([`Update from Nexus (${m.update.latest ?? m.update.file_name})`, () => updateFromNexus(id), '', 'Download the newer file and update the mod in place']);
    items.push(['Update from a file…', () => updateFromFile(id), '', 'Install a newer archive of it in its place, keeping its order, rules and notes'], '-');
    items.push(['File conflicts', () => fileConflicts(id)], ['Hide files…', () => hideFilesDialog(id), '', 'Keep chosen files out of the game (MO2’s hide)'], ['Notes…', () => notesDialog(id)], ...(m.plugins.length && !W.simple ? [['Pack into archives…', () => packDialog(id), '', 'Pack its loose files into archives named after its plugin']] : []), ['Open folder', () => call('open_mod_folder', { id }).catch(err => toast(String(err), 'error'))]);
  }
  // Nexus: endorse and track the selected mods that have Nexus ids (public, on the account).
  const onNexus = mods.filter(i => byId.get(i)?.nexus_id);
  if (onNexus.length && nexus.user) {
    const all = (f) => onNexus.every(i => f(byId.get(i)));
    items.push('-', all(x => x.endorsed === 'Endorsed') ? ['Withdraw endorsement', () => nexusAccount(onNexus, 'abstain')] : ['Endorse on Nexus', () => nexusAccount(onNexus, 'endorse'), '', 'Endorse as your Nexus account'],
      all(x => x.tracked) ? ['Untrack on Nexus', () => nexusAccount(onNexus, 'untrack')] : ['Track on Nexus', () => nexusAccount(onNexus, 'track'), '', 'Its updates show in your tracking centre on Nexus']);
  }
  items.push('-', ['Colour…', () => colourDialog(ids)]);
  if (mods.length) items.push(['Categories…', () => categoriesDialog(mods)]);
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

$('#loot').onclick = () => lootSort();

/** LOOT's order as a preview: only the plugins that really move, each with the plugin it then
 *  loads after and LOOT's reason; Apply saves it (one undo step). `without`: plugins whose rules
 *  of the user's this sort leaves out (after a circle of rules). True once the order matches LOOT. */
async function lootSort(title = 'Sort with LOOT', intro = '', without = []) {
  toast('Sorting with LOOT. The first sort downloads LOOT’s masterlist.');
  const r = await act('Sorting with LOOT', () => call('sort_plugins', { dry_run: true, without }), { reload: false });
  if (!r) return false;
  if (r.cycle) return lootCycle(r.cycle, title, intro, without);
  const rest = r.warnings.filter(w => !w.startsWith('LOOT reads which plugins are on'));
  const warnings = rest.length ? `<details><summary>${plural(rest.length, 'warning')}</summary><pre>${esc(rest.join('\n'))}</pre></details>` : '';
  const stale = r.undeployed ? `<p class="small">LOOT saw ${plural(r.undeployed, 'plugin')} on or off differently from this profile, so its rules about other plugins may be off. <b>Deploy, then sort again</b> may fix that.</p>` : '';
  const left = without.length ? `<p class="small">This sort leaves out your rules for ${esc(without.join(', '))}. They stay saved; change them in the plugin’s panel.</p>` : '';
  const again = r.undeployed ? [['deploy', 'Deploy, then sort again']] : [];
  let choice;
  if (!r.moved.length) {
    choice = await dialog({ title, body: `${intro}<p>The load order already matches LOOT.</p>${left}${stale}${warnings}`, actions: [...again, ['cancel', 'Close']] });
  } else {
    const line = m => `<li>${esc(m.name)} <span class="muted">${m.after ? 'moves after' : 'moves to the top'}</span> ${esc(m.after ?? '')}${m.why?.length ? `<details class="small"><summary>Why</summary>${m.why.map(w => `<div>${esc(w)}</div>`).join('')}</details>` : ''}</li>`;
    const body = `${intro}<p>${plural(r.moved.length, 'plugin')} ${r.moved.length === 1 ? 'moves' : 'move'}; the others keep their order.</p><ul class="moves">${r.moved.map(line).join('')}</ul>${left}${stale}${warnings}`;
    choice = await dialog({ title, body, actions: [['cancel', 'Cancel'], ...again, ['apply', 'Apply order', 'primary']] });
  }
  if (choice === 'deploy') return !!await act('Deploying', () => call('deploy')) && lootSort(title, intro, without);
  if (choice === 'apply') return !!await act('Saving the LOOT order', () => call('sort_plugins', { dry_run: false, without }));
  return !r.moved.length;
}

/** LOOT's rules go in a circle: show them, and for the user's own rules in it, sort without one
 *  just this once or remove it. */
async function lootCycle(c, title, intro, without) {
  const yours = c.yours.length
    ? `<p>Your rules for ${esc(c.yours.join(', '))} are part of it. Sort without them just this once (they stay saved), or remove them.</p>`
    : c.rules.some(r => !/\.es[mpl]$/i.test(r.from)) ? '<p>The circle is in the order of LOOT’s groups; change the group settings involved.</p>'
    : '<p>None of these rules are yours, so it needs a fix in LOOT’s masterlist or the plugins.</p>';
  const body = `${intro}<p>LOOT can’t sort: its rules go in a circle.</p><ul class="small">${c.text.map(t => `<li>${esc(t)}</li>`).join('')}</ul>${yours}`;
  const v = await dialog({ title, body, actions: [['cancel', 'Close'], ...c.yours.flatMap((y, i) => [[`r${i}`, `Remove your rule for ${y}`], [`w${i}`, `Sort without it`, 'primary']])] });
  const y = v && c.yours[+v.slice(1)];
  if (!y) return false;
  if (v[0] === 'w') return lootSort(title, intro, [...without, y]);
  if (!await act('Removing the LOOT rule', () => call('set_loot_rule', { plugin: y }), { reload: false })) return false;
  lootRules = null;
  return lootSort(title, intro, without);
}

/** Why LOOT puts a plugin where it does, for its panel. */
async function loadWhy(name) {
  const r = await act('Sorting with LOOT', () => call('plugin_why', { plugin: name }), { reload: false });
  if (!r || r === true) return;
  whyData = { name, r };
  if (pluginSide()) renderDetails();
}

function whyBlock(r) {
  const at = r.profile_position === r.loot_position ? `LOOT puts it here, at ${r.loot_position}.` : `It is at ${r.profile_position ?? '?'}; LOOT would put it at ${r.loot_position}.`;
  return `<p class="small">${at}</p><ul class="small">${r.why.map(w => `<li>${esc(w)}</li>`).join('')}</ul>`;
}

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

$('#play').onclick = () => play();

/** Enabled plugins whose masters are missing or load later, which crash the game at start (the
 *  ones Problems lists, less those ignored there). */
const crashers = () => S.plugins.filter(p => p.enabled && p.missing_masters.length && !ignored.has(`missing_masters|${p.name}`));

async function play() {
  const bad = crashers();
  if (bad.length) {
    const fixable = bad.filter(p => p.missing_masters.every(loadsLater));
    const why = p => {
      const absent = p.missing_masters.filter(m => !loadsLater(m));
      return absent.length ? `needs ${absent.join(', ')}, which ${absent.length === 1 ? 'isn’t' : 'aren’t'} on` : `loads before ${p.missing_masters.join(', ')}`;
    };
    const one = bad.length === 1;
    const v = await dialog({
      title: 'The game would crash at start',
      body: `<p>${one ? 'This plugin needs' : 'These plugins need'} other plugins (${one ? 'its' : 'their'} masters) on and loaded before ${one ? 'it' : 'them'}:</p>
        <ul class="small">${bad.map(p => `<li><b>${esc(p.name)}</b> <span class="muted">${esc(why(p))}</span></li>`).join('')}</ul>
        <p class="muted small">${fixable.length ? 'Sort with LOOT puts masters first. ' : ''}Turning ${one ? 'it' : 'them'} off is one undo step.</p>`,
      actions: [['cancel', 'Cancel'], ['anyway', 'Play anyway'], ['off', one ? 'Turn it off' : 'Turn them off', fixable.length ? '' : 'primary'], ...(fixable.length ? [['sort', 'Sort with LOOT…', 'primary']] : [])],
    });
    if (v === 'sort') return await lootSort() && play();
    if (v === 'off') return await setPluginsEnabled(bad.map(p => p.name), false) && play();
    if (v !== 'anyway') return;
  }
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
}

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

/** Share another profile's mod and plugin order, or stop sharing it. */
async function shareOrder(cur) {
  const sharers = S.options?.shares_order_with ?? [];
  if (sharers.length) {
    if (await confirmBox(`Stop sharing ${cur}’s order?`, `${cur} shares its mod and plugin order with ${sharers.join(', ')}: moving a mod in one moves it in the others. Stopping keeps the order ${cur} has now as its own.`, 'Stop sharing', 'primary')) {
      if (await act('Changing the profile', () => call('share_order', { with: null }))) toast(`${cur} has its own order again.`);
    }
    return;
  }
  const others = S.profiles.filter(p => p !== cur);
  if (!others.length) return toast('There’s no other profile to share an order with. Copy this profile first.', 'warn');
  const v = await dialog({
    title: 'Share another profile’s order',
    body: `<p>${esc(cur)} takes the other profile’s mod and plugin order now, and from then on moving a mod in either moves it in both. Each keeps its own mods and plugins turned on or off, so one list can serve, say, a light and a heavy setup.</p>
      <label class="field">Profile <select id="share-with">${others.map(p => `<option>${esc(p)}</option>`).join('')}</select></label>
      <p class="muted small">Undo puts ${esc(cur)}’s order back.</p>`,
    actions: [['cancel', 'Cancel'], ['ok', 'Share its order', 'primary']],
  });
  if (v !== 'ok') return;
  const with_ = $('#share-with').value;
  if (await act('Changing the profile', () => call('share_order', { with: with_ }))) toast(`${cur} now shares ${with_}’s mod order.`);
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
    + `<button class="item adv" data-share="1" title="Profiles that share one mod and plugin order, each with its own mods turned on"><span class="check">${S.options?.shares_order_with?.length ? '✓' : ''}</span>${S.options?.shares_order_with?.length ? `Order shared with ${esc(S.options.shares_order_with.join(', '))}…` : 'Share another profile’s order…'}</button>`
    + '<hr class="adv"><button class="item adv" data-io="mo2"><span class="check"></span>Import from MO2…</button>'
    + '<button class="item adv" data-io="vortex"><span class="check"></span>Import from Vortex…</button>'
    + '<button class="item adv" data-io="export" title="A lockfile, a pack with your own files, or a readable list"><span class="check"></span>Share this setup…</button>'
    + '<button class="item adv" data-io="diff"><span class="check"></span>Compare with a lockfile…</button>'
    + '<button class="item adv" data-io="apply"><span class="check"></span>Apply a lockfile…</button>';
  pop.hidden = false;
};
document.addEventListener('click', e => { if (!e.target.closest('.menu')) $('#profile-pop').hidden = true; if (!e.target.closest('#ctx')) closeCtx(); });

$('#profile-pop').onclick = async e => {
  const t = e.target.closest('[data-delete],[data-switch],[data-new],[data-rename],[data-io],[data-opt],[data-shortcut],[data-share]');
  if (!t) return;
  $('#profile-pop').hidden = true;
  const cur = S.info.profile;
  if (t.dataset.share) return shareOrder(cur);
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
  card.innerHTML = `<div class="start-head"><b>Getting started</b><span class="muted small">${done} of ${steps.length} done${W.simple ? ' · The Simple view hides the Data, Tools and MCP tabs until you turn it off in Settings.' : ''}</span><div class="spacer"></div><button class="link" data-start="hide">Hide</button></div>
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
  const ways = sso
    ? 'Sign in on nexusmods.com and approve rmm there. Or paste your personal API key from your account’s API Keys page.'
    : 'Open your account’s API Keys page on nexusmods.com, copy the Personal API Key at the bottom, and paste it here. (A sign-in button that skips this comes once Nexus approves rmm.)';
  const body = `<p>${esc(why)}</p><p class="muted small">${ways} rmm keeps the key in your user folder, never in the instance or a lockfile.</p>
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
  const app = { 'nxmhandler.exe': 'Mod Organizer 2', 'vortex.exe': 'Vortex', 'rmm.exe': 'rmm, for another instance', 'rmm-app.exe': 'rmm, for another instance' }[exe.toLowerCase()] ?? exe;
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
        const wanted = coll?.pending.find(p => p.mod_id === f.nexus?.mod_id && p.file_id === f.nexus?.file_id);
        if (wanted) await collGot(wanted);
        else if (job.update && byId.has(job.update)) await updateMod(job.update, f.path, f);
        else await install([f.path], job.at, f);
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
  if (coll) {
    const got = coll.total - coll.pending.length, open = coll.pending.filter(p => coll.opened.has(p.file_id));
    rows.push(`<div><b>${esc(coll.name)}</b>: ${got} of ${plural(coll.total, 'file')} downloaded, ${coll.pending.length} to go.
      ${coll.paused ? 'Paused.' : `On each Nexus page that opens, click “Mod manager download” (free accounts wait a few seconds there).${open.length ? ` <span class="muted small">Open now: ${esc(open.map(p => p.name).join(', '))}.</span>` : ''}`}
      The install starts when they’re all in.${nexus.handler_here ? '' : ' <span class="pill bad">Nexus links don’t open with rmm</span> <button data-coll="handler">Set it up</button>'}
      <div class="row" style="margin-top:6px"><button data-coll="more">Open the next ones</button><button data-coll="pause">${coll.paused ? 'Resume' : 'Pause'}</button><button class="link" data-coll="stop">Stop</button></div></div>`);
  }
  if (nxmNow) rows.push(`<div>Downloading ${esc(linkName(nxmNow.link))}…${nxmQueue.length ? ` <span class="muted small">${nxmQueue.length} more waiting</span>` : ''}</div>`);
  else if (nxmQueue.length) rows.push(`<div class="muted small">${plural(nxmQueue.length, 'download')} waiting.</div>`);
  nxmFailed.forEach((f, i) => rows.push(`<div><span class="pill bad">Failed</span> ${esc(linkName(f.link))} <span class="muted small">${esc(f.error)}</span> <button data-retry="${i}">Retry</button> <button class="link" data-drop="${i}">Remove</button></div>`));
  $('#dl-queue').innerHTML = rows.join('');
  $('#dl-queue').hidden = !rows.length;
  $('#n-downloads').classList.toggle('bad', nxmFailed.length > 0);
}
$('#dl-queue').onclick = e => {
  const c = e.target.closest('[data-coll]')?.dataset.coll;
  if (c && coll) {
    if (c === 'more') collOpen(coll.pending.filter(p => !coll.opened.has(p.file_id)).length && COLL_AT_ONCE);
    else if (c === 'pause') { coll.paused = !coll.paused; if (!coll.paused) collOpen(); }
    else if (c === 'stop') { coll = null; toast('Stopped. The files downloaded so far stay; installing the collection again carries on from them.'); }
    else if (c === 'handler') setHandler().then(renderQueue);
    return renderQueue();
  }
  const retry = e.target.closest('[data-retry]')?.dataset.retry, drop = e.target.closest('[data-drop]')?.dataset.drop;
  if (retry !== undefined) nxmQueue.push(...nxmFailed.splice(+retry, 1));
  else if (drop !== undefined) nxmFailed.splice(+drop, 1);
  runQueue();
};
T.event.listen('nxm', () => takeLinks());

$('#nxm').onclick = async () => {
  // Signed in first: every download goes through the user's Nexus account.
  if (!nexus.user && !await nexusLogin('Sign in to Nexus Mods first: rmm downloads with your Nexus account, free or Premium.', nexus.sso)) return;
  const link = await ask('Install from Nexus Mods', nexus.user?.is_premium
    ? 'Paste a mod page link or a mod id (downloads its newest main file), a collection’s link, or the link of a file’s “Mod manager download” button.'
    : 'Paste a collection’s link: rmm opens each file’s page for you to download. For one mod, click “Mod manager download” on its Files tab on nexusmods.com, or paste that button’s link here.', '');
  if (!link || !await nexusReady('Downloads from Nexus Mods need you signed in.')) return;
  if (link.includes('/collections/')) return installCollection(link);
  nxmQueue.push({ link, at: slotAfter(selIds().length === 1 ? selIds()[0] : null) });
  runQueue();
};

// A collection for a free account: Nexus lets free accounts download only through the website, so
// rmm opens the download page of each file it still needs, a few at a time, and the links its
// "Mod manager download" buttons send back are matched to those files by mod and file id. When
// they are all in, `go` installs from the downloads folder.
const COLL_AT_ONCE = 3;
let coll = null;
async function collectionGo(args, name, go) {
  if (nexus.user?.is_premium) return go();
  const r = await act('Checking which files you have', () => call('install_collection', { ...args, dry_run: true }), { reload: false, problem: true });
  if (!r || r === true) return;
  if (!r.to_download.length) return go();
  await offerHandler();
  coll = { name, go, pending: r.to_download, total: r.to_download.length, opened: new Set(), paused: false };
  showTab('downloads');
  collOpen();
}
/** Open download pages: `extra` more, or up to COLL_AT_ONCE waiting. */
async function collOpen(extra = 0) {
  const waiting = coll.pending.filter(p => coll.opened.has(p.file_id)).length;
  const next = coll.pending.filter(p => !coll.opened.has(p.file_id)).slice(0, extra || Math.max(0, COLL_AT_ONCE - waiting));
  for (const p of next) {
    coll.opened.add(p.file_id);
    await call('open_nexus_mod', { mod_id: p.mod_id, file_id: p.file_id }).catch(e => toast(String(e), 'error'));
  }
  renderQueue();
}
async function collGot(p) {
  coll.pending = coll.pending.filter(x => x !== p);
  if (coll.pending.length) {
    if (!coll.paused) await collOpen();
    return renderQueue();
  }
  const { go } = coll;
  coll = null;
  renderQueue();
  await go();
}

/** A Nexus collection: what it would install, then install it into a new profile (as Vortex
 *  does, so the user's setup stays as it was) or into this one. */
async function installCollection(link) {
  if (coll) return toast(`${coll.name} is still waiting for its downloads (Downloads tab). Stop it there first.`, 'error');
  const r = await act('Reading the collection', () => call('install_collection', { link, dry_run: true }), { reload: false, problem: true });
  if (!r || r === true) return;
  const list = (xs, n = 8) => xs.length ? `<ul class="small">${xs.slice(0, n).map(x => `<li>${esc(x)}</li>`).join('')}${xs.length > n ? `<li class="muted">and ${xs.length - n} more</li>` : ''}</ul>` : '';
  if (r.update_from != null) return updateCollection(link, r, list);
  const name = r.name.replace(/[<>:"/\\|?*\x00-\x1f]/g, '').trim().replace(/^\.+|\.+$/g, '').slice(0, 64) || 'Collection';
  const body = `<p>${plural(r.install.length, 'mod')} to download and install (${bytes(r.download_size)})${r.installed.length ? `; ${plural(r.installed.length, 'mod')} you have already are turned on` : ''}. Its mod rules, plugin list and tools come too. Then LOOT’s order for the plugins, which you see before it applies.</p>
    ${freeNote(r)}
    ${r.warnings.map(w => `<p class="small">${esc(w)}</p>`).join('')}
    ${r.game_folder.length ? `<p class="muted small">The collection puts these in the game folder: ${esc(r.game_folder.join(', '))}.</p>` : ''}
    ${r.optional.length ? `<label class="row"><input id="co-opt" type="checkbox"> Also its ${plural(r.optional.length, 'optional mod')}</label>${list(r.optional)}` : ''}
    <label class="row" style="margin-top:12px"><input id="co-new" type="checkbox" checked> Into a new profile, so your current setup stays as it is</label>
    <label class="field">New profile’s name <input id="co-name" type="text" value="${esc(name)}" spellcheck="false"></label>`;
  if (await dialog({ title: `Install ${r.name} (revision ${r.revision})`, body, actions: [['cancel', 'Cancel'], ['go', 'Install', 'primary']] }) !== 'go') return;
  const args = { link, revision: r.revision, optional: $('#co-opt')?.checked ?? false, profile: $('#co-new').checked ? $('#co-name').value.trim() : null };
  return collectionGo(args, r.name, () => collectionInstalled(args, list));
}

/** Free accounts: where the files come from. */
function freeNote(r) {
  return r.to_download?.length ? `<p class="small">Your Nexus account is free, so you download ${r.to_download.length === 1 ? 'the file' : `the ${r.to_download.length} files`} from Nexus’s pages: rmm opens them ${COLL_AT_ONCE} at a time and you click “Mod manager download” on each. The install starts when they’re all in.</p>` : '';
}

async function collectionInstalled(args, list) {
  const done = await act(`Installing the collection`, () => call('install_collection', { ...args, sort: false }), { problem: true });
  if (!done || done === true) return;
  const sorted = await lootSort(`Sort ${done.name}’s plugins with LOOT`, '<p>The mods are in. This is the order LOOT gives the plugins.</p>');
  const part = (title, xs) => xs.length ? `<p><b>${esc(title)}</b></p>${list(xs, 20)}` : '';
  await dialog({
    title: `${done.name} is installed`,
    body: `<p>${plural(done.install.length - done.failed.length, 'mod')} installed${done.profile ? `, in the profile “${esc(done.profile)}”, now the active one` : ''}. Deploy or Play applies it to the game folder.</p>
      ${sorted ? '' : unsorted}${part('Couldn’t install', done.failed)}${part('The curator’s notes (each is also on its mod)', done.instructions)}${part('Tools added', done.tools)}${part('Warnings', done.warnings)}`,
    actions: [['ok', 'OK', 'primary']],
  });
}

const unsorted = '<p class="muted small">The plugins aren’t in LOOT’s order: new ones load last. Sort with LOOT…, above the plugins, sorts them, showing what moves first.</p>';

/** The profile already has an earlier revision: update it, keeping the user's own changes. */
async function updateCollection(link, r, list) {
  if (r.revision === r.update_from && !r.install.length && !r.updated.length && !r.turned_off.length) return toast(`${r.name} is up to date (revision ${r.revision}).`);
  const body = `<p>${plural(r.updated.length, 'mod')} to update and ${plural(r.install.length, 'new mod')} to install (${bytes(r.download_size)}). Its mod rules and plugin list come too. Then LOOT’s order for the plugins, which you see before it applies.</p>
    ${r.turned_off.length ? `<p>To turn off, still installed: the revision dropped them, or has a newer file of them that goes in beside (another profile uses these).</p>${list(r.turned_off.map(nameOf))}` : ''}
    ${r.kept_off.length ? `<p class="muted small">You turned these off; they stay off: ${esc(r.kept_off.join(', '))}.</p>` : ''}
    ${r.yours.length ? `<p class="muted small">Your own ${plural(r.yours.length, 'mod')} stay as they are, and so do plugins you turned on or off.</p>` : ''}
    ${freeNote(r)}
    ${r.warnings.map(w => `<p class="small">${esc(w)}</p>`).join('')}`;
  if (await dialog({ title: `Update ${r.name} from revision ${r.update_from} to ${r.revision}`, body, actions: [['cancel', 'Cancel'], ['go', 'Update', 'primary']] }) !== 'go') return;
  return collectionGo({ link, revision: r.revision }, r.name, () => collectionUpdated(link, r, list));
}

async function collectionUpdated(link, r, list) {
  const done = await act(`Updating ${r.name}`, () => call('install_collection', { link, revision: r.revision, sort: false }), { problem: true });
  if (!done || done === true) return;
  const sorted = await lootSort(`Sort ${done.name}’s plugins with LOOT`, '<p>The revision is in. This is what LOOT would move; Cancel keeps your order.</p>');
  const part = (title, xs) => xs.length ? `<p><b>${esc(title)}</b></p>${list(xs, 20)}` : '';
  await dialog({
    title: `${done.name} is at revision ${done.revision}`,
    body: `<p>Deploy or Play applies it to the game folder.</p>${sorted ? '' : unsorted}${part('Updated', done.updated)}${part('Installed', done.install)}${part('Turned off', done.turned_off.map(nameOf))}${part('Couldn’t install', done.failed)}${part('Warnings', done.warnings)}`,
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
  // GitHub releases need no account. Without Nexus sign-in, GitHub's updates still show.
  const fromGh = S.mods.some(m => m.web?.startsWith('https://github.com/'));
  const g = fromGh ? await (quiet ? call('link_updates').catch(() => null) : act('Checking GitHub for updates', () => call('link_updates'), { reload: false })) : [];
  const gh = Array.isArray(g) ? g : [];
  let r = [], gone = [];
  if (quiet || nexus.user || !gh.length) {
    if (!quiet && !await nexusReady('Update checks ask Nexus Mods about your mods, which needs your API key.')) {
      if (!gh.length) return;
    } else {
      checkCollection();
      // The mod list's Update tags come with the reload.
      const c = quiet ? await call('check_updates').then(async u => (await load(), u)).catch(() => null) : await act('Checking for updates', () => call('check_updates'));
      if (!Array.isArray(c?.updates)) {
        if (!gh.length) return;
      } else {
        r = c.updates;
        gone = c.unavailable;
      }
    }
  }
  const all = r.length + gh.length;
  if (quiet) return all && toast(`${plural(all, 'update')} found; Check for updates lists them.`);
  const held = S.mods.filter(m => m.pinned).length;
  // Mods whose Nexus page is gone: the check skipped them; there's nothing to update.
  const goneNote = gone.length ? `Nexus no longer has a page for ${plural(gone.length, 'mod')}, or hides it (removed, hidden, or a wrong id), so it wasn’t checked: ${gone.map(g => nameOf(g.id)).join(', ')}.` : '';
  if (!all) return toast(`Every mod with Nexus ids${fromGh ? ' or a GitHub page' : ''} is up to date${held ? ` (${plural(held, 'mod')} held at their versions)` : ''}.${goneNote ? ' ' + goneNote : ''}`, goneNote ? 'info' : undefined);
  const premium = !!nexus.user?.is_premium;
  const rows = r.map(u => `<tr data-id="${esc(u.id)}"><td>${esc(nameOf(u.id))}<details class="changelog" data-mod="${u.mod_id}" data-since="${esc(u.installed ?? '')}"><summary class="small">What’s new</summary><div class="muted small">Loading…</div></details></td><td>${esc(u.installed ?? '?')} → <b>${esc(u.latest ?? u.file_name)}</b></td>
    <td style="white-space:nowrap"><button type="button" data-hold="skip" title="Don’t offer this file; a later one is offered">Skip</button> <button type="button" data-hold="pin" title="Keep the mod at its version until you release it in its panel">Hold</button> <button type="button" class="primary" data-get title="${premium ? 'Download it and update the mod in place' : 'Open its Files tab: Mod manager download brings it back here as an update'}">${premium ? 'Update' : 'Files tab'}</button></td></tr>`).join('')
    + gh.map((u, i) => `<tr data-id="${esc(u.id)}" data-gh="${i}"><td>${esc(nameOf(u.id))} <span class="muted small">GitHub</span>${u.file ? '' : `<br><select data-gh-file aria-label="The release’s file to update from">${u.files.map(f => `<option>${esc(f)}</option>`).join('')}</select>`}</td><td>${esc(u.installed ?? '?')} → <b>${esc(u.latest)}</b></td>
    <td style="white-space:nowrap"><button type="button" data-hold="skip" title="Don’t offer this release; a later one is offered">Skip</button> <button type="button" data-hold="pin" title="Keep the mod at its version until you release it in its panel">Hold</button> <button type="button" class="primary" data-gh-get title="Download the release and update the mod in place">Update</button></td></tr>`).join('');
  const how = [
    r.length ? (premium ? 'Update downloads the new file and updates the mod in place, with the FOMOD choices it was installed with; Undo puts the old files back.' : 'Free accounts download from the website: Files tab opens the mod’s, where “Mod manager download” brings the file back here as an update (once Nexus links open rmm).') : '',
    gh.length ? 'A GitHub update downloads the latest release and updates the mod in place; Undo puts the old files back.' : '',
    gh.length && !nexus.user && S.mods.some(m => m.nexus_id) ? 'Sign in to Nexus Mods to check your Nexus mods too.' : '',
    r.length && !premium && gh.length ? 'Update all updates the GitHub ones.' : '',
  ].filter(Boolean).join(' ');
  // Update all: what downloads here (GitHub, and Nexus with Premium), one mod after another.
  const auto = (premium ? r.length : 0) + gh.length;
  const done = dialog({ title: plural(all, 'update'), body: `<table class="fc">${rows}</table><p class="muted small">${how} Skip passes over this version; Hold keeps the mod at its version.</p>${goneNote ? `<p class="muted small">${esc(goneNote)}</p>` : ''}`, actions: [['cancel', 'Close'], ...(auto > 1 ? [['all', 'Update all', 'primary']] : [])] });
  $('#dlg').classList.add('wide');
  let changed = false;
  $('#dlg').onclick = async e => {
    const tr = e.target.closest('tr[data-id]');
    if (!tr) return;
    const u = tr.dataset.gh !== undefined ? gh[+tr.dataset.gh] : null;
    if (e.target.closest('button[data-gh-get]')) {
      const file = u.file ?? $('select[data-gh-file]', tr).value;
      $('#dlg').close('cancel');
      return updateFromLink(u, file);
    }
    const get = e.target.closest('button[data-get]');
    if (get) {
      updateFromNexus(tr.dataset.id);
      if (premium) get.replaceWith('Queued');
      return;
    }
    const b = e.target.closest('button[data-hold]');
    if (!b) return;
    try {
      if (u && b.dataset.hold === 'skip') await call('link_skip', { id: u.id, version: u.latest });
      else await call('hold_updates', { ids: [tr.dataset.id], hold: b.dataset.hold });
      changed = true;
      b.parentElement.textContent = b.dataset.hold === 'pin' ? 'Held' : 'Skipped';
    } catch (err) { toast(`Holding the update failed.\n${err}`, 'error'); }
  };
  const v = await done;
  $('#dlg').onclick = null;
  if (v === 'all') {
    // The rows still offering an update (not skipped, held or queued), read before another dialog replaces them.
    const left = $$('#dlg tr[data-id]').filter(tr => $('button[data-get], button[data-gh-get]', tr));
    const fromGh = left.filter(tr => tr.dataset.gh !== undefined).map(tr => [gh[+tr.dataset.gh], gh[+tr.dataset.gh].file ?? $('select[data-gh-file]', tr).value]);
    const fromNexus = premium ? left.filter(tr => tr.dataset.gh === undefined).map(tr => tr.dataset.id) : [];
    for (const [u, file] of fromGh) await updateFromLink(u, file);
    fromNexus.forEach(updateFromNexus); // the download queue takes them one at a time
  } else if (changed) load();
}

/** Download a mod's newer GitHub release and update it in place. */
async function updateFromLink(u, file) {
  const f = await act(`Downloading ${nameOf(u.id)} ${u.latest}`, () => call('link_download', { url: u.page, file }), { reload: false, problem: true });
  if (f && f !== true) await updateMod(u.id, f.path);
}

// ---- From a link: GitHub, ModDB and direct links download here; sites that need the user's
// sign-in, and pages rmm can't read, open in the browser, and rmm takes the finished file from the
// Downloads folder. -----------------------------------------------------------------------------

$('#link').onclick = async () => {
  const url = (await ask('Install from a link', 'Paste a GitHub repository or release, a ModDB page, a LoversLab, AFK Mods or Patreon page, or a link to a .zip, .7z or .rar.', ''))?.trim();
  if (!url) return;
  const at = slotAfter(selIds().length === 1 ? selIds()[0] : null);
  if (/^https?:\/\/([^/]*\.)?nexusmods\.com\//i.test(url)) {
    if (!await nexusReady('Downloads from Nexus Mods need you signed in.')) return;
    if (url.includes('/collections/')) return installCollection(url);
    nxmQueue.push({ link: url, at });
    return runQueue();
  }
  const p = await act('Reading the link', () => call('link_plan', { url }), { reload: false, problem: true });
  if (!p || p === true) return;
  if (p.browser) return fromBrowser(p, at);
  let file = p.files[0].name;
  if (p.files.length > 1) {
    const rows = p.files.map((f, i) => `<label class="row"><input type="radio" name="lf" value="${i}" ${i ? '' : 'checked'}> ${esc(f.name)}${f.size ? ` <span class="muted small">${bytes(f.size)}</span>` : ''}</label>`).join('');
    const what = p.version ? `Its release ${esc(p.version)} has` : 'It has';
    if (await dialog({ title: `${p.name ?? 'Files'} on ${p.site}`, body: `<p>${what} ${p.files.length} files. Which one?</p>${rows}`, actions: [['cancel', 'Cancel'], ['ok', 'Install', 'primary']] }) !== 'ok') return;
    file = p.files[+$('#dlg input[name=lf]:checked').value].name;
  }
  const f = await act(`Downloading ${file}`, () => call('link_download', { url, file }), { reload: false, problem: true });
  if (f && f !== true) install([{ path: f.path, nx: { name: f.name ?? undefined } }], at);
};

/** A page to download in the browser: open it, then list archives that land in the Downloads
 *  folder until the user picks one, which moves into rmm's downloads folder and installs. */
async function fromBrowser(p, at) {
  const since = Math.floor(Date.now() / 1000) - 5;
  await call('open_page', { url: p.page }).catch(e => toast(String(e), 'error'));
  const why = p.site === 'link' ? 'rmm can’t download from that page itself' : `${esc(p.site)} needs you signed in`;
  const body = `<p>${why}, so the page is open in your browser. Download the mod there, and pick the file here once it has finished.</p>
    <div id="bd-list"><p class="muted">Waiting for a download…</p></div>
    <label class="row small"><input type="checkbox" id="bd-all"> Show files downloaded earlier</label>
    <p class="muted small">rmm looks in your Downloads folder and moves the file you pick into its own downloads folder. Your sign-in stays in your browser.</p>`;
  const done = dialog({ title: `Download from ${p.site === 'link' ? 'the page' : p.site}`, body, actions: [['cancel', 'Cancel']] });
  $('#dlg').classList.add('wide');
  let picked = null;
  const draw = async () => {
    const list = await call('browser_downloads', { since: $('#bd-all')?.checked ? 0 : since }).catch(() => []);
    const el = $('#bd-list');
    if (el) el.innerHTML = list.length ? `<table class="fc">${list.slice(0, 20).map(d => `<tr><td>${esc(d.name)}</td><td class="muted small">${bytes(d.size)}</td><td><button type="button" class="primary" data-path="${esc(d.path)}">Install</button></td></tr>`).join('')}</table>` : '<p class="muted">Waiting for a download…</p>';
  };
  draw();
  const timer = setInterval(draw, 2000);
  $('#dlg').onclick = e => {
    const b = e.target.closest('button[data-path]');
    if (b) {
      picked = b.dataset.path;
      $('#dlg').close('ok');
    }
    if (e.target.id === 'bd-all') draw();
  };
  const v = await done;
  clearInterval(timer);
  $('#dlg').onclick = null;
  if (v !== 'ok' || !picked) return;
  const path = await act('Taking the download', () => call('take_browser_download', { path: picked, page: p.page }), { reload: false, problem: true });
  if (typeof path === 'string') install([path], at);
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

// ---- MCP tab: AI apps use rmm's tools ---------------------------------------------------------

let mcpOn = null;   // { url, token } while the window serves MCP over HTTP
let mcpRoot = null; // the instance it serves

/** Start or stop the HTTP server; quiet unless it fails. */
async function setMcp(on) {
  try {
    const r = await T.core.invoke('mcp', { on });
    mcpOn = r.on ? r : null;
    mcpRoot = mcpOn ? S.info.root : null;
  } catch (e) {
    mcpOn = null;
    toast(`MCP: ${e}`, 'error');
  }
  prefs.mcp = !!mcpOn;
  savePrefs();
  if (shown('mcp')) drawMcp();
}

/** A snippet to paste into an AI app's settings, with a Copy button. */
const snippet = (label, text) => `<p class="small">${label} <button class="small" data-copy="${esc(text)}">Copy</button></p><pre class="sel">${esc(text)}</pre>`;

function drawMcp() {
  const json = server => JSON.stringify({ mcpServers: { rmm: server } }, null, 2);
  // A helper agent the tester prompt hands live tests to (its tools are named after the server: rmm).
  const tester = snippet('Claude Code: a tester on Haiku, the cheapest Claude model, for live tests:', 'rmm tester-agent --apply');
  $('#mcp-http').innerHTML = mcpOn
    ? `<div class="row"><span class="pill win">On</span> <code>${esc(mcpOn.url)}</code><div class="spacer"></div><button data-mcp="off">Turn off</button></div>
       ${snippet('Claude Code:', `claude mcp add --transport http rmm ${mcpOn.url} --header "Authorization: Bearer ${mcpOn.token}"`)}
       ${snippet('Other apps (JSON settings):', json({ type: 'http', url: mcpOn.url, headers: { Authorization: `Bearer ${mcpOn.token}` } }))}
       ${tester}
       <p class="muted small">It serves this instance on 127.0.0.1 with a private token, kept in your user folder. Anyone with the token can change this instance, so keep it to yourself. It stays on for the next window.</p>`
    : `<div class="row"><span class="muted">Off.</span><div class="spacer"></div><button class="primary" data-mcp="on">Turn on</button></div>
       <p class="muted small">For apps that connect to a running server. It serves the open instance on 127.0.0.1 with a private token, only while this window is open.</p>`;
  const root = S?.info?.root ?? '';
  $('#mcp-stdio').innerHTML = `<p class="muted small">For apps that start rmm themselves; nothing needs to be on here. They run <code>rmm mcp</code>, which the installer puts on PATH.</p>
    ${snippet('Claude Code:', `claude mcp add rmm -- rmm mcp -i "${root}"`)}
    ${snippet('Claude Desktop, Cursor and other apps (JSON settings):', json({ command: 'rmm', args: ['mcp', '-i', root] }))}
    ${tester}`;
}

$('#pane-mcp').onclick = async e => {
  const b = e.target.closest('button[data-mcp], button[data-copy]');
  if (!b) return;
  if (b.dataset.mcp) return setMcp(b.dataset.mcp === 'on');
  b.textContent = await copyText(b.dataset.copy) ? 'Copied' : 'Couldn’t copy';
  setTimeout(() => { b.textContent = 'Copy'; }, 1500);
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
    // What it costs: disk space, hidden files, MO2's overwrite, clashes, and the mods it puts after.
    const cost = [
      first.files ? `${plural(first.files, 'file')}, ${first.copy_bytes ? `<b>${bytes(first.copy_bytes)}</b> of them copied, as they’re on another drive than the instance` : 'hard-linked: no extra disk space'}` : '',
      first.hidden ? `${plural(first.hidden, 'file')} MO2 had hidden, which stay hidden (Hide files… in a mod’s panel shows them)` : '',
      first.overwrite ? `${plural(first.overwrite, 'file')} from MO2’s overwrite folder, into rmm’s Overwrite` : '',
    ].filter(Boolean);
    const clashes = (first.clashes ?? []).map(c => `${c.id}: yours ${c.have ?? '(no version)'}, ${plural(c.have_files, 'file')}; ${what === 'mo2' ? 'MO2' : 'Vortex'}’s ${c.incoming ?? '(no version)'}, ${plural(c.incoming_files, 'file')}`);
    const after = args.all ? [] : (r.after ?? []);
    const body = into + (own.length ? `<p>MO2 kept INIs and saves per profile, and so will rmm: ${own.join(', and ')} are copied in.</p>` : '')
      + (cost.length ? `<ul>${cost.map(c => `<li>${c}</li>`).join('')}</ul>` : '')
      + `<p class="muted small">The list becomes ${args.all ? 'each' : 'this'} profile’s order in one step that Undo reverts.${after.length ? ` The ${plural(after.length, 'mod')} already in this profile that the import doesn’t name stay, after the imported ones, so they win their file conflicts with them.` : ''}${what === 'vortex' ? ' Purge Vortex’s deployment before deploying with rmm.' : ''}</p>`
      + part('to bring in', first.mods)
      + (clashes.length ? `<details><summary><b>${clashes.length}</b> already installed under the same name: yours stays, the import’s doesn’t come in</summary><pre>${esc(clashes.join('\n'))}</pre></details>` : '')
      + part('already in this profile, after the import', after.map(nameOf))
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
  $('#tools tbody').innerHTML = t.tools.map(({ id, tool, last, when: ran, status: st }) => {
    const out = tool.output_mod ?? `${tool.name} Output`;
    const how = !last ? '<span class="muted">Never run</span>'
      : (last.edit_in_place ? (last.files ? `${plural(last.files, 'file')} saved into mods` : 'Changed nothing')
        : last.mod_id ? `${plural(last.files, 'file')} captured` : 'Captured nothing')
        + (last.timed_out ? ' · timed out' : last.exit_code ? ` · exit code ${last.exit_code}` : '')
        + (last.warnings?.length ? ` · <span title="${esc(last.warnings.join('\n'))}">⚠ ${last.warnings.length}</span>` : '');
    const when_ = ran ? `<div class="muted small">${esc(when(ran))}</div>` : '';
    const status = !st ? '' : st.state === 'current' ? '<span class="pill win" title="Made from the setup as it is now, in what this tool reads">Up to date</span>'
      : st.state === 'stale' ? `<span class="pill lose" title="${esc(st.why)}">Out of date</span>`
      : `<span class="pill plain" title="${esc(st.why)}">Unknown</span>`;
    return `<tr data-tool="${esc(id)}">
      <td class="name"><b>${esc(tool.name)}</b> <span class="muted small">${esc(id)}</span><div class="muted small" title="${esc(tool.exe)}">${esc(tool.exe)}</div></td>
      <td>${tool.edit_in_place ? `<span title="Files it changes go back to the mods they came from; new files go to ${esc(out)}">Edits in place · ${esc(out)}</span>` : esc(out)} ${status}</td><td>${how}${when_}</td>
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
    <details id="t-more"><summary>More: files it keeps, rewrites, waits for, its settings file</summary>
      <p class="muted small">Left empty, a preset’s own values stay. In paths, * and ** match any name.</p>
      <label class="field">Keep from the last run, one per line<textarea id="t-keep" rows="2" spellcheck="false" placeholder="Nemesis_Engine/mod/**"></textarea><span class="muted small">Files of its previous output put back in {output} before it runs: state it reads back, such as a mod list or a cache.</span></label>
      <label class="field">Rewrites in Data, one per line<textarea id="t-rewrites" rows="2" spellcheck="false" placeholder="meshes/actors/character/**/*.hkx"></textarea><span class="muted small">Files it changes where they are: the run gets copies, so mods’ own files stay as they were.</span></label>
      <label class="field">Also wait for<input id="t-wait" spellcheck="false" placeholder="SkyrimSE.exe"><span class="muted small">A program in the game folder that it starts and leaves running, so the run waits for that too (a grass cache run through the game).</span></label>
      <label class="field">Settings file<input id="t-sfile" spellcheck="false" placeholder="Settings.json, beside the program unless a full path"></label>
      <label class="field">Settings to set before each run, one key = value per line<textarea id="t-sset" rows="3" spellcheck="false" placeholder="games.SkyrimSE.gameDataPath = {data}&#10;output = {output}"></textarea><span class="muted small">Dotted keys into the JSON file; values are JSON (true, 3, ["a"]) or text, with placeholders filled in.</span></label>
    </details>
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
  const lines = sel => $(sel, d).value.split('\n').map(s => s.trim()).filter(Boolean);
  const args = lines('#t-args');
  if (!preset.value) spec.args = args;
  // Empty means the preset's own (null), so only what's filled in is sent.
  if (lines('#t-keep').length) spec.keep = lines('#t-keep');
  if (lines('#t-rewrites').length) spec.rewrites = lines('#t-rewrites');
  if ($('#t-wait', d).value.trim()) spec.wait_for = $('#t-wait', d).value.trim();
  const sfile = $('#t-sfile', d).value.trim(), sset = lines('#t-sset');
  if (sfile || sset.length) {
    if (!sfile) return toast('Settings to set need the settings file they go in.', 'warn');
    const set = {};
    for (const l of sset) {
      const i = l.indexOf('=');
      if (i < 1) return toast(`“${l}” isn’t key = value.`, 'warn');
      const v = l.slice(i + 1).trim();
      try { set[l.slice(0, i).trim()] = JSON.parse(v); } catch { set[l.slice(0, i).trim()] = v; }
    }
    spec.settings = { file: sfile, set };
  }
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
    ? `<span class="muted">${!rows.length ? 'Search results' : `${rows.length >= 1000 ? 'The first 1,000 matches' : rows.length === 1 ? '1 match' : `${rows.length.toLocaleString()} matches`}. Click one to open its folder.`}</span>`
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
  const orphans = savesData.orphans ?? [];
  $('#saves-cosaves').hidden = !orphans.length;
  $('#saves-cosaves').textContent = `Clean ${plural(orphans.length, 'co-save')}`;
}

$('#saves-cosaves').onclick = async () => {
  const o = savesData.orphans;
  if (!await confirmBox(`Clean ${plural(o.length, 'co-save')}?`, `The script extender keeps a co-save beside each save, and ${o.length === 1 ? 'this one’s save is' : 'these ones’ saves are'} gone: ${o.slice(0, 6).join(', ')}${o.length > 6 ? '…' : ''}. ${o.length === 1 ? 'It moves' : 'They move'} to the instance’s trash, which Empty the trash deletes.`, 'Move to the trash', 'primary')) return;
  const r = await act('Cleaning the co-saves', () => call('clean_cosaves'), { reload: false });
  if (Array.isArray(r)) toast(`Moved ${plural(r.length, 'co-save')} to the trash.`);
  loadSaves();
};

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
$('#saves tbody').addEventListener('contextmenu', e => {
  const file = e.target.closest('tr[data-file]')?.dataset.file;
  if (!file) return;
  e.preventDefault();
  ctxMenu(e, [['Delete…', () => deleteSaves(file), 'danger']]);
});
$('#saves-reload').onclick = loadSaves;

/** The profile's saves as ticks, with an All box (#st-all, #st-saves); `ticked` start ticked. */
const saveChecklist = (saves, ticked = []) => `<label class="row" style="margin:12px 0 4px"><input type="checkbox" id="st-all"> All ${plural(saves.length, 'save')}</label>
  <div class="stack" id="st-saves" style="max-height:40vh;overflow:auto">${saves.map(s => `<label class="row" style="margin:0" title="${esc(s.file)}"><input type="checkbox" value="${esc(s.file)}"${ticked.includes(s.file) ? ' checked' : ''}> ${esc(s.player || s.file)}${s.level ? `, level ${s.level}` : ''}${s.location ? `, ${esc(s.location)}` : ''} <span class="muted small">${esc(when(s.modified))}</span></label>`).join('')}</div>`;

/** Delete saves: they and their co-saves go to the instance's trash. `file` starts ticked. */
async function deleteSaves(file) {
  const saves = savesData?.saves ?? [];
  if (!saves.length) return toast('This profile has no saves.');
  const done = dialog({ title: 'Delete saves', body: `${saveChecklist(saves, file ? [file] : [])}
    <p class="muted small" style="margin-top:10px">Each save goes with its script extender co-save to the instance’s trash, which Empty the trash deletes for good.</p>`,
    actions: [['cancel', 'Cancel'], ['delete', 'Move to the trash', 'danger']] });
  $('#st-all').onchange = e => $$('#st-saves input').forEach(c => { c.checked = e.target.checked; });
  const r = await done;
  const files = $$('#st-saves input:checked').map(c => c.value);
  if (r !== 'delete') return;
  if (!files.length) return toast('No saves were ticked.');
  const moved = await act('Deleting the saves', () => call('delete_saves', { files }), { reload: false });
  if (Array.isArray(moved)) toast(`Moved ${plural(files.length, 'save')} to the trash${moved.length > files.length ? (files.length === 1 ? ', with its co-save' : ', with their co-saves') : ''}.`);
  loadSaves();
}
$('#saves-delete').onclick = () => deleteSaves();

$('#saves-transfer').onclick = async () => {
  const cur = S.info.profile, others = S.profiles.filter(p => p !== cur), saves = savesData?.saves ?? [];
  if (!others.length) return toast('There’s no other profile yet. New playthrough… in the profile menu makes one with saves of its own.');
  if (!saves.length) return toast('This profile has no saves to copy or move.');
  const body = `<label class="field">To the profile<select id="st-to">${others.map(p => `<option value="${esc(p)}">${esc(p)}</option>`).join('')}</select></label>
    ${saveChecklist(saves)}
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

/** The downloads the Find box matches: by file name, Nexus name or the mods made from it. */
function dlShown() {
  const q = $('#dl-find').value.trim().toLowerCase(), all = downloadsData ?? [];
  return !q ? all : all.filter(d => [d.name, d.nexus?.name ?? '', ...d.mods.map(nameOf)].some(s => s.toLowerCase().includes(q)));
}

function renderDownloads() {
  const all = downloadsData, l = dlShown();
  for (const n of dlSel) if (!all.some(d => d.name === n)) dlSel.delete(n);
  $('#n-downloads').textContent = all.length || '';
  $('#downloads-note').textContent = l.length === all.length ? 'Files you get with From Nexus… or From a link… land here. Installing one leaves the file, so you can install it again later.' : `${l.length.toLocaleString()} of ${plural(all.length, 'download')}`;
  $('#downloads tbody').innerHTML = l.map(d => `<tr data-name="${esc(d.name)}"${dlSel.has(d.name) ? ' class="sel"' : ''}>
    <td class="check-col"><input type="checkbox" data-pick ${dlSel.has(d.name) ? 'checked' : ''}></td>
    <td class="name" title="${esc(d.name)}">${esc(d.name)}</td>
    <td class="name">${d.nexus ? `<button class="link" data-page="${d.nexus.nexus.mod_id}" title="Open its Files tab on Nexus">${esc(d.nexus.name ?? `mod ${d.nexus.nexus.mod_id}`)}</button>${d.nexus.nexus.version ? ` <span class="muted small">${esc(d.nexus.nexus.version)}</span>` : ''}` : '<span class="muted">—</span>'}</td>
    <td class="name">${d.mods.length ? d.mods.map(srcLink).join(', ') : '<span class="muted">Not installed</span>'}</td>
    <td class="num">${bytes(d.size)}</td><td class="muted">${esc(when(d.modified))}</td>
    <td class="run-col">${d.mods.length ? '<button data-reinstall title="Install it again over the mod made from it">Reinstall…</button>' : '<button data-install class="primary">Install</button>'} <button data-delete class="link danger" title="Delete the file for good">Delete…</button></td></tr>`).join('');
  $('#downloads-empty').hidden = l.length > 0;
  $('#downloads-empty').textContent = all.length ? 'No downloads match.' : 'No downloads yet.';
  $('#dl-all').checked = l.length > 0 && l.every(d => dlSel.has(d.name));
  const sel = selDownloads();
  $('#dl-install').hidden = $('#dl-delete').hidden = !sel.length;
  $('#dl-install').textContent = `Install ${plural(sel.length, 'file')}`;
  $('#dl-delete').textContent = `Delete ${plural(sel.length, 'file')}…`;
  const unknown = (sel.length ? sel : l).filter(d => !d.nexus?.nexus.file_id).length; // a name's mod id isn't the file
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
  if (e.target.checked) for (const d of dlShown()) dlSel.add(d.name);
  renderDownloads();
};
$('#dl-find').oninput = () => { if (downloadsData) renderDownloads(); };
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
  const names = (sel.length ? sel : downloadsData ?? []).filter(d => !d.nexus?.nexus.file_id).map(d => d.name);
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
      const v = await dialog({ title: `Update ${name}?`, body: `<p><b>${esc(basename(path))}</b> looks like another version of <b>${esc(name)}</b>: the same mod page.</p><p class="muted small">Update replaces its files and keeps its place, notes, categories, colour and rules; Undo puts the old files back. Install as a new mod keeps both.</p>`,
        actions: [['cancel', 'Cancel'], ['new', 'Install as a new mod'], ['ok', 'Update', 'primary']] });
      if (v === 'ok') await updateMod(id, path, nx);
      if (v !== 'new') continue;
    }
    // Unpacks the archive once; the install below reuses it.
    const walk = await act(`Reading ${basename(path)}`, () => call('fomod', { path }), { reload: false, problem: true });
    if (!walk) continue;
    let choices, name = nx?.name ?? undefined, run_script = false;
    if (walk.script) {
      if (!await scriptOk(path)) continue;
      run_script = true;
    } else if (walk !== true) { // it has a FOMOD installer, which names the mod better than most file names
      choices = walk.steps.length ? await fomodWizard(path, walk) : walk.choices;
      if (!choices) continue;
      name = walk.module;
    }
    const r = await act(`Installing ${basename(path)}`, () => call('install', { path, choices, name, priority: at, nexus: nx?.nexus, run_script }), { problem: true });
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
  if (type === 'enter' || type === 'over') {
    $('#dropzone').hidden = !S || busy;
    // Over a mod row: install right after it (a separator: at the end of its group).
    const pos = e.payload.position, r = window.devicePixelRatio || 1;
    const tr = pos && shown('mods') ? document.elementFromPoint(pos.x / r, pos.y / r)?.closest('#mods tr[data-id]') : null;
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

/** Hints on a FOMOD installer's options: the one for this game version (Skyrim SE's AE and
 *  pre-AE files), and patches for mods that are enabled. Returns option -> [[label, class, title]]. */
function fomodHints(module, gv) {
  const norm = s => s.toLowerCase().replace(/[^a-z0-9]/g, '');
  const own = norm(module ?? '');
  const skyrim = S.info.game === 'skyrim-se' && /^1\.[56]\./.test(gv ?? '');
  const ae = skyrim && gv.startsWith('1.6.');
  const names = new Map(); // what an option would name -> the enabled mod or plugin
  for (const m of S.mods) if (m.enabled && !m.separator) names.set(norm(m.name), m.name);
  // Not the base game's own plugins (its DLC and Creations), which every setup has.
  for (const p of S.plugins ?? []) if (p.enabled && !p.implicit && !names.has(norm(p.name.replace(/\.es[plm]$/i, '')))) names.set(norm(p.name.replace(/\.es[plm]$/i, '')), p.name);
  // Long enough to mean something, and not the mod being installed (every option names it).
  const have = [...names].filter(([k]) => k.length >= 5 && !own.includes(k) && !['skyrim', 'update', 'fallout4'].includes(k)).sort((a, b) => b[0].length - a[0].length);
  // And their initials as a word: USSEP for the Unofficial Skyrim Special Edition Patch.
  const short = [...names.values()].map(n => [n.replace(/\.es[plm]$/i, '').split(/[^A-Za-z0-9]+/).filter(Boolean), n]).filter(([w]) => w.length >= 3).map(([w, n]) => [w.map(x => x[0]).join('').toLowerCase(), n]).filter(([k]) => k.length >= 3 && !own.includes(k));
  return o => {
    const out = [], t = o.name;
    if (skyrim) {
      const pre = /non[- ]?ae\b|pre[- ]?ae\b|1\.5\.\d|\bse only\b/i.test(t);
      const post = !pre && /\bae\b|anniversary|1\.6\.\d/i.test(t);
      if (ae ? post : pre) out.push(['Your game version', 'win', `Your game is version ${gv}, the ${ae ? 'Anniversary Edition (1.6)' : 'Special Edition before AE (1.5)'} build`]);
      else if (ae ? pre : post) out.push(['Other game version', 'lose', `For ${ae ? 'the game before AE (1.5.97)' : 'the Anniversary Edition build (1.6)'}; yours is ${gv}`]);
    }
    const words = t.toLowerCase().split(/[^a-z0-9]+/);
    const hit = have.find(([k]) => norm(t).includes(k)) ?? short.find(([k]) => words.includes(k));
    if (hit) out.push([`You have ${hit[1]}`, 'win', `${hit[1]} is enabled, so a patch for it is likely what you want`]);
    return out;
  };
}

/** Walk a FOMOD installer one step at a time. After every change the core plays the whole
 *  installer again, so later steps, defaults and option types follow the choices. Resolves to
 *  the choices to install with, or null when cancelled. */
function fomodWizard(path, first) {
  const d = $('#dlg');
  const images = new Map(); // image path -> data: URL, or null when it can't be read
  let w = first, at = 0, working = false, shown = null;
  const hints = fomodHints(first.module, first.game_version);

  const radio = g => g.type === 'SelectExactlyOne' || g.type === 'SelectAtMostOne';
  const option = (gi, oi) => {
    const g = w.steps[at].groups[gi];
    const tag = TAG[g.options[oi].type];
    const o = g.options[oi];
    const locked = g.type === 'SelectAll' || o.type === 'Required' || o.type === 'NotUsable';
    return `<label class="opt${o.type === 'NotUsable' ? ' off' : ''}" data-g="${gi}" data-o="${oi}">
      <input type="${radio(g) ? 'radio' : 'checkbox'}" name="g${gi}" ${o.selected ? 'checked' : ''} ${locked ? 'disabled' : ''}>
      <span>${esc(o.name)}</span>${tag ? `<span class="pill ${tag[1]}">${tag[0]}</span>` : ''}${hints(o).map(([l, c, h]) => `<span class="pill ${c}" title="${esc(h)}">${esc(l)}</span>`).join('')}</label>`;
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

// rmm's own version, and updates from its signed releases.
T.app.getVersion().then(v => { $('#set-version').textContent = `rmm ${v}`; }).catch(() => {});
$('#set-update').onclick = async () => {
  rmmChecked = true;
  const r = await act('Checking for a newer rmm', () => T.core.invoke('app_update', { install: false }), { reload: false });
  if (!r || r === true) return;
  if (!r.newer) return toast(`rmm ${r.current} is the newest.`);
  const v = await dialog({
    title: `rmm ${r.newer} is out`,
    body: `<p>You have ${esc(r.current)}.</p>${r.notes ? `<pre class="small" style="white-space:pre-wrap">${esc(r.notes)}</pre>` : ''}<p class="muted small">Installing closes this window, runs the installer and opens the new rmm, with rmm on the command line updated too. Your instances and settings stay as they are.</p>`,
    actions: [['cancel', 'Later'], ['ok', 'Install', 'primary']],
  });
  if (v === 'ok') act('Installing the update', () => T.core.invoke('app_update', { install: true }), { reload: false });
};

// ---- Settings: rmm's (settings.json, shared with the CLI and AI apps) and this window's -------

let conf = null, healthKinds = [], dlDir = null;
async function loadConf() {
  try { ({ settings: conf, health_kinds: healthKinds } = await call('settings')); } catch {}
  return conf;
}
const LANGS = [['en', 'English'], ['de', 'Deutsch'], ['es', 'Español'], ['fr', 'Français'], ['it', 'Italiano'], ['ja', '日本語'], ['ko', '한국어'], ['pl', 'Polski'], ['pt_BR', 'Português (Brasil)'], ['ru', 'Русский'], ['sv', 'Svenska'], ['uk', 'Українська'], ['zh_CN', '中文']];
const TABS = [['mods', 'Mods and plugins'], ['data', 'Data'], ['saves', 'Saves'], ['downloads', 'Downloads'], ['problems', 'Problems'], ['tools', 'Tools'], ['history', 'History'], ['mcp', 'MCP']];
const MCP_OPTIONS = 'Options', MCP_ROLES = 'Your instructions for each role';
// [key, label, kind, options or [min, max, step], help]. Keys with "win." are this window's.
const SETTINGS = [
  ['This window', 'Kept by this window on this computer.', [
    ['win.lang', 'Language', 'select', Object.entries(I18N.LANGS), 'The window’s own words; mod names, files, and messages from the game and from rmm’s engine stay as they are. Changing it reloads the window.'],
    ['win.theme', 'Theme', 'select', [['system', 'Follow Windows'], ['light', 'Light'], ['dark', 'Dark']]],
    ['win.density', 'Rows', 'select', [['comfortable', 'Comfortable'], ['compact', 'Compact']]],
    ['win.zoom', 'Size', 'select', [[0.8, '80%'], [0.9, '90%'], [1, '100%'], [1.1, '110%'], [1.25, '125%'], [1.5, '150%']]],
    ['win.simple', 'Simple view', 'bool', null, 'Hides the Data, Tools and MCP tabs, the advanced filters, separators, lockfiles, imports, and the tool and test-run settings.'],
    ['win.gettingStarted', 'Show the getting-started list on the Mods tab until it’s all done', 'bool'],
    ['win.startTab', 'Open on', 'select', [['last', 'The last tab'], ...TABS]],
    ['win.poll', 'Look for changes made by the CLI or AI apps', 'select', [[1.5, 'Every 1.5 seconds'], [5, 'Every 5 seconds'], [15, 'Every 15 seconds'], [0, 'Never']]],
    ['win.toast', 'Seconds a message stays', 'num', [2, 60], 'Errors stay until you click them.'],
    ['win.recentDays', 'The Recent filter shows mods installed in the last', 'num', [1, 365], 'days'],
    ['win.confirmDrop', 'Ask before installing files dropped on the window', 'bool'],
    ['win.deployPreview', 'Show what a deploy changes before it runs', 'bool'],
  ]],
  ['Deploy', 'How mod files reach the game folder.', [
    ['deploy.method', 'Files in the game folder', 'select', [['link', 'Hard links (copies across drives)'], ['copy', 'Copies']], 'Copies take space, and nothing the game does to them reaches the mod. The next deploy redoes links as copies.'],
    ['deploy.skip_extras', 'Leave out files that aren’t for the game', 'bool', null, 'MO2’s meta.ini, Vortex’s override notes, FOMOD installers, read-me’s and licences at the top of Data or the game folder, and the script extender’s sources.'],
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
  ['Problems', 'What the Problems tab and AI apps’ health check report.', [
    ['health.hidden', 'Report', 'kinds'],
    ['health.low_space_gb', 'Warn when a drive has less free space than', 'num', [0, 10000], 'GB (0: never)'],
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
  [MCP_OPTIONS, '', [
    ['mcp.port', 'HTTP port on 127.0.0.1', 'num', [1024, 65535], 'AI apps’ settings name this port; change them too.'],
    ['mcp.record_sessions', 'Keep each AI app’s session in the instance', 'bool', null, 'As a chat any AI app can read later (list_chats, read_chat).'],
  ]],
  [MCP_ROLES, 'rmm gives AI apps four prompts: find_mods (Researcher), install_safely (Installer), test_mods (Tester) and mod_team (Lead). What you write here is added to each.', [
    ['ai.role_instructions.lead', 'For the Lead', 'area'],
    ['ai.role_instructions.researcher', 'For the Researcher', 'area'],
    ['ai.role_instructions.installer', 'For the Installer', 'area'],
    ['ai.role_instructions.tester', 'For the Tester', 'area'],
  ]],
];
const pathGet = (o, k) => k.split('.').reduce((v, p) => v?.[p], o);
const pathSet = (o, k, v) => { const ps = k.split('.'), last = ps.pop(); const t = ps.reduce((x, p) => (x[p] ??= {}), o); if (v === '' && ps.at(-1) === 'role_instructions') delete t[last]; else t[last] = v; };

async function drawMore() {
  if (!conf) await loadConf();
  if (!conf) return;
  dlDir = await call('downloads_dir').catch(() => null);
  const cur = k => k === 'win.lang' ? I18N.lang : k.startsWith('win.') ? W[k.slice(4)] : pathGet(conf, k);
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
  const mcp = s => s[0] === MCP_OPTIONS || s[0] === MCP_ROLES;
  // Simple view leaves out tools and test runs, which it hides elsewhere too (and the MCP tab).
  const shown = SETTINGS.filter(s => !W.simple || !['Test runs', 'Tools'].includes(s[0]));
  $('#set-more').innerHTML = shown.filter(s => !mcp(s)).map(section).join('');
  $('#set-mcp').innerHTML = shown.filter(mcp).map(section).join('');
}

$('#set-more').onchange = $('#set-mcp').onchange = async e => {
  const el = e.target.closest('[data-key]');
  if (!el) return;
  const { key, kind } = el.dataset;
  let v = kind === 'bool' ? el.checked
    : kind === 'num' ? Number(el.value)
    : kind === 'lines' ? el.value.split('\n').map(l => l.trim()).filter(Boolean)
    : kind === 'kind' ? $$('input[data-kind="kind"]', $('#set-more')).filter(c => !c.checked).map(c => c.value)
    : kind === 'area' ? el.value.trim() : el.value;
  if (kind === 'select' && el.value !== '' && !isNaN(el.value)) v = Number(el.value);
  if (key === 'win.lang') return I18N.set(v);
  if (key.startsWith('win.')) {
    W[key.slice(4)] = v;
    savePrefs();
    if (key === 'win.simple') drawMore();
    return applyWin();
  }
  const next = structuredClone(conf);
  pathSet(next, key, v);
  try { conf = await call('set_settings', { settings: next }); } catch (err) { toast(String(err), 'error'); return drawMore(); }
  if (key === 'mcp.port' && mcpOn) setMcp(true);
};
$('#set-more').onclick = $('#set-reset').onclick = async e => {
  const b = e.target.closest('button[data-act]');
  if (!b) return;
  if (b.dataset.act === 'reset') {
    if (!await confirmBox('Reset all settings', 'Put every setting on this page and the MCP tab back to its default?', 'Reset')) return;
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
// ---- Dialogs and toasts ---------------------------------------------------------------------

/** Modal dialog. actions: [[value, label, class]]. Resolves to the chosen value (the input's
 *  text when `input` is given), or null when cancelled. */
/** A mod's read-me, wrapped, to read in a dialog. */
async function showDoc(id, doc) {
  let d;
  try { d = await call('read_file', { path: `mods/${id}/${doc}` }); } catch (e) { return toast(String(e), 'error'); }
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


// ---- Contents, light plugins, archives, LOOT groups, game settings ------------------------------

const CONTENT = {
  plugins: ['🧩', 'Plugins'], skse: ['⚙️', 'Script extender plugin (DLL)'], scripts: ['📜', 'Scripts'],
  meshes: ['🧊', 'Meshes'], textures: ['🖼️', 'Textures'], animations: ['🏃', 'Animations'],
  interface: ['🖥️', 'Interface (menus)'], sound: ['🔊', 'Sound or music'], archives: ['📦', 'Archives (BSA or BA2)'],
  root: ['📁', 'Files for the game folder'], ini: ['📝', 'INI settings'], fomod: ['🧙', 'Installed with a FOMOD wizard'],
  'tool output': ['🛠️', 'A tool’s output'],
};

let lightCands = null; // plugins that could be flagged light, read when a plugin panel needs them
function lightRow(p) {
  if (!p.mod_id || p.implicit || p.error) return '';
  if (p.light) return p.name.toLowerCase().endsWith('.esl') ? '' : '<p class="small muted">Flagged light (ESL): it takes no full load-order slot. <button class="link" data-act="light-off">Remove the flag</button></p>';
  if (!lightCands) {
    lightCands = [];
    call('light_candidates').then(c => { lightCands = c; if (pluginSide()) renderDetails(); }).catch(() => {});
    return '';
  }
  return lightCands.includes(p.name) ? '<p class="small">It could be a light plugin (ESL), which takes no full load-order slot. <button data-act="light-on">Flag light…</button></p>' : '';
}

/** Flag plugins light: `names`, or the ones the user picks from every candidate. */
async function flagLight(names) {
  const all = names ?? await act('Reading the plugins', () => call('light_candidates'), { reload: false });
  if (!Array.isArray(all) || !all.length) return toast('No plugin can be flagged light.');
  const pick = all.length > 1 ? `<div class="checks">${all.map(n => `<label class="row"><input type="checkbox" value="${esc(n)}" checked> ${esc(n)}</label>`).join('')}</div>` : '';
  const body = `<p>${all.length > 1 ? `These ${all.length} plugins` : esc(all[0])} could be light (ESL): ${all.length > 1 ? 'they then take' : 'it then takes'} no full load-order slot, of which the game has 254. Their new records fit a light plugin’s form ids, and they add no cells or worldspaces.</p>${pick}
    <p class="small"><b>For a new game.</b> A save that already uses a plugin loses what it added (items, NPCs, quests) once it’s light. Undo takes the flag off again.</p>`;
  if (await dialog({ title: 'Flag light', body, actions: [['cancel', 'Cancel'], ['ok', 'Flag light', 'primary']] }) !== 'ok') return;
  const chosen = all.length > 1 ? $$('#dlg .checks input:checked').map(i => i.value) : all;
  for (const n of chosen) if (!await act(`Flagging ${n} light`, () => call('set_light', { plugin: n, light: true }), { reload: false, problem: true })) break;
  lightCands = null;
  load();
}

/** Pack a mod's loose files into archives, after a preview of what goes in and what changes hands. */
async function packDialog(id, plugin) {
  const r = await act('Reading the mod', () => call('pack_mod', { id, plugin, dry_run: true }), { reload: false });
  if (!r || r === true) return;
  const files = r.archives.reduce((n, a) => n + a.files, 0), size = r.archives.reduce((n, a) => n + a.bytes, 0);
  const plugins = byId.get(id)?.plugins ?? [];
  const pick = plugins.length > 1 ? `<label class="row small">Named after <select id="pack-plugin">${plugins.map(p => `<option ${p === r.plugin ? 'selected' : ''}>${esc(p)}</option>`).join('')}</select></label>` : '';
  const game = S.info.game === 'fallout4' ? 'Fallout 4' : 'Skyrim';
  const body = `<p>Packs ${plural(files, 'file')} (${bytes(size)}) into ${r.archives.map(a => `<b>${esc(a.name)}</b>`).join(' and ')}, which ${esc(r.plugin)} loads. The game reads fewer loose files, and the mod’s folder stays tidy.</p>${pick}
    ${r.kept.length ? `<details><summary class="small">${plural(r.kept.length, 'file')} stay loose</summary><div class="files">${r.kept.map(esc).join('<br>')}</div><p class="muted small">Plugins, script extender plugins and their settings, and files the game reads only loose.</p></details>` : ''}
    ${r.loses.length ? `<p class="small"><span class="pill bad">${r.loses.length} change hands</span> These win against other mods now; packed, they lose to them, because every loose file beats every archive:</p><div class="files">${r.loses.slice(0, 30).map(esc).join('<br>')}${r.loses.length > 30 ? '<br>…' : ''}</div>` : ''}
    <p class="muted small">${game === 'Skyrim' ? 'Skyrim’s archives are stored uncompressed. ' : 'Textures go in a texture archive, the rest compressed. '}Undo unpacks it again; Deploy puts it in the game.</p>`;
  const done = dialog({ title: `Pack ${nameOf(id)}`, body, actions: [['cancel', 'Cancel'], ['ok', 'Pack', 'primary']] });
  $('#pack-plugin')?.addEventListener('change', e => $('#dlg').close('p:' + e.target.value));
  const v = await done;
  if (v?.startsWith('p:')) return packDialog(id, v.slice(2));
  if (v !== 'ok') return;
  const out = await act(`Packing ${nameOf(id)}`, () => call('pack_mod', { id, plugin: r.plugin, dry_run: false }), { problem: true });
  if (out && out !== true) toast(`Packed ${plural(files, 'file')} of ${nameOf(id)} into ${out.archives.map(a => a.name).join(' and ')}. Undo unpacks it.`);
}

/** LOOT's groups: which load after which, with the user's additions and groups of their own. */
$('#loot-groups').onclick = async () => {
  let groups = await act('Reading LOOT’s groups', () => call('loot_groups'), { reload: false });
  if (!Array.isArray(groups)) return;
  if (!groups.length) return toast('LOOT’s groups come with its masterlist; Sort with LOOT downloads it.');
  const draw = () => {
    const names = groups.map(g => g.name);
    const rows = groups.map(g => {
      const others = names.filter(n => n !== g.name && !g.after.includes(n) && !g.yours.includes(n));
      return `<tr data-g="${esc(g.name)}"><td><b>${esc(g.name)}</b>${g.user_only ? ' <span class="pill plain">yours</span>' : ''}${g.description ? `<div class="muted small">${esc(g.description)}</div>` : ''}<div class="muted small" title="${esc(g.plugins.join(', '))}">${plural(g.plugins.length, 'plugin')}</div></td>
        <td class="small">${g.after.map(a => `<span class="pill plain" title="LOOT’s masterlist">${esc(a)}</span>`).join(' ')} ${g.yours.map(a => `<span class="pill win">${esc(a)} <button type="button" class="link" data-unafter="${esc(a)}" title="Take your rule away">×</button></span>`).join(' ')}${!g.after.length && !g.yours.length ? '<span class="muted">first</span>' : ''}</td>
        <td><select data-add aria-label="Load ${esc(g.name)} after another group"><option value="">Load after…</option>${others.map(n => `<option>${esc(n)}</option>`).join('')}</select>${g.user_only ? ' <button type="button" class="link danger" data-drop>Remove</button>' : ''}</td></tr>`;
    }).join('');
    $("#lg-body").innerHTML = `<table class="form">${rows}</table>`;
  };
  const body = `<p class="small">Each group loads after the groups listed: grey ones are LOOT’s, green ones yours. A plugin’s panel puts it in a group (its LOOT rule).</p><div id="lg-body"></div>
    <div class="row"><input id="lg-name" placeholder="New group’s name" spellcheck="false"><select id="lg-after"><option value="">after…</option>${groups.map(g => `<option>${esc(g.name)}</option>`).join('')}</select><button type="button" id="lg-new">Make it</button></div>`;
  const done = dialog({ title: 'LOOT groups', body, actions: [['cancel', 'Close']] });
  $('#dlg').classList.add('wide');
  draw();
  const set = async (name, after, remove = false) => {
    const r = await act(remove ? `Removing ${name}` : `Saving ${name}`, async () => {
      await call('set_loot_group', { name, after, remove });
      return call('loot_groups');
    }, { reload: false });
    if (!Array.isArray(r)) return false;
    groups = r;
    lootRules = null;
    draw();
    $('#lg-after').innerHTML = `<option value="">after…</option>${groups.map(g => `<option>${esc(g.name)}</option>`).join('')}`;
    return true;
  };
  $('#dlg').onchange = e => {
    const g = groups.find(x => x.name === e.target.closest('tr[data-g]')?.dataset.g);
    if (g && e.target.matches('[data-add]') && e.target.value) set(g.name, [...g.yours, e.target.value]);
  };
  $('#dlg').onclick = e => {
    if (e.target.id === 'lg-new') {
      const name = $('#lg-name').value.trim(), after = $('#lg-after').value;
      if (!name) return toast('Give the group a name.');
      return set(name, after ? [after] : []).then(ok => { if (ok) $('#lg-name').value = ''; });
    }
    const g = groups.find(x => x.name === e.target.closest('tr[data-g]')?.dataset.g);
    if (!g) return;
    if (e.target.dataset.unafter) set(g.name, g.yours.filter(a => a !== e.target.dataset.unafter), !g.user_only && g.yours.length === 1);
    if (e.target.matches('[data-drop]')) set(g.name, [], true);
  };
  await done;
  $('#dlg').onclick = $('#dlg').onchange = null;
};

/** The game's everyday INI settings as a form. */
$('#game-settings').onclick = async () => {
  const list = await act('Reading the game’s INI files', () => call('ini_form'), { reload: false });
  if (!Array.isArray(list)) return;
  const groups = [...new Set(list.map(s => s.group))];
  const control = (s, i) => {
    const v = s.value ?? s.default, id = `gs-${i}`;
    if (s.kind === 'bool') return `<input type="checkbox" id="${id}" ${v.trim() === '1' ? 'checked' : ''}>`;
    if (s.kind === 'choice') return `<select id="${id}">${s.options.map(o => `<option ${o === v.trim() ? 'selected' : ''}>${o}</option>`).join('')}</select>`;
    if (s.kind === 'int') return `<input type="number" id="${id}" min="${s.min}" max="${s.max}" value="${esc(v)}" style="width:7em">`;
    return `<input type="range" id="${id}" min="${s.min}" max="${s.max}" step="${s.step}" value="${esc(v)}"> <output class="small">${(+v).toFixed(String(s.step).split('.')[1]?.length ?? 0)}</output>`;
  };
  const body = groups.map(g => `<h3>${esc(g)}</h3><table class="form gs"><colgroup><col><col style="width:260px"><col style="width:150px"></colgroup>${list.map((s, i) => s.group !== g ? '' : `<tr><td><label for="gs-${i}">${esc(s.label)}</label>${s.help ? `<div class="muted small">${esc(s.help)}</div>` : ''}</td><td style="white-space:nowrap">${control(s, i)}</td><td class="muted small" title="[${esc(s.section)}] ${esc(s.key)}">${s.file ? esc(s.file) : 'game default'}</td></tr>`).join('')}</table>`).join('')
    + '<p class="muted small">Saved into the INI that holds each setting now, else the Prefs INI. They take effect when the game starts; the toast after saving can put them back.</p>';
  const done = dialog({ title: 'Game settings', body, actions: [['cancel', 'Cancel'], ['ok', 'Save', 'primary']] });
  $('#dlg').classList.add('wide');
  const now = (s, i) => {
    const el = $(`#gs-${i}`);
    return s.kind === 'bool' ? (el.checked ? '1' : '0') : el.value;
  };
  const start = list.map(now); // as the controls show them: a slider snaps to its step, which isn't a change
  $('#dlg').oninput = e => { if (e.target.type === 'range') e.target.nextElementSibling.value = (+e.target.value).toFixed(String(e.target.step).split('.')[1]?.length ?? 0); };
  const v = await done;
  $('#dlg').oninput = null;
  if (v !== 'ok') return;
  const changed = list.map((s, i) => [s, now(s, i), start[i]]).filter(([, x, was]) => x !== was);
  if (!changed.length) return;
  const edits = [];
  for (const [s, value] of changed) {
    const e = await act(`Saving ${s.label}`, () => call('ini_form_set', { section: s.section, key: s.key, value }), { reload: false, problem: true });
    if (!e || e === true) break;
    edits.push(e.n);
  }
  if (edits.length) toast(`Saved ${plural(edits.length, 'setting')}; the game reads them when it starts.`, 'info', ['Put back', async () => {
    for (const n of edits.reverse()) await call('revert_edit', { n }).catch(err => toast(String(err), 'error'));
    toast('Put back.');
  }]);
};
