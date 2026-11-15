'use strict';
// The window talks to rmm-core through one Tauri command, `call(name, args)` (src/main.rs).
const T = window.__TAURI__;
const call = (name, args = {}) => T.core.invoke('call', { name, args });
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const esc = v => String(v ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const basename = p => String(p).split(/[\\/]/).pop();
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
const GAMES = { 'skyrim-se': 'Skyrim Special Edition', fallout4: 'Fallout 4' };
const LAST = 'rmm.lastInstance';
const remember = path => { try { localStorage.setItem(LAST, path); } catch {} };
const recall = () => { try { return localStorage.getItem(LAST); } catch { return null; } };

let S = null;          // instance state from the core
let names = new Map(); // mod id -> name
let plan = null;       // deploy dry run, or { error }
let issues = null;     // health check findings
let issuesStale = true;
let details = null;    // mod_details of the selected mod
let stamp = '';
let sel = null;        // selected mod id
let tab = 'mods';
let busy = false;
let healthTimer = 0, healthRun = 0;
const nameOf = id => names.get(id) ?? id;

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
  } finally {
    setBusy(false);
  }
}

function setBusy(on) {
  busy = on;
  $('#busy').hidden = !on;
  document.body.classList.toggle('busy', on);
}

async function load() {
  S = await call('state');
  stamp = await call('stamp');
  names = new Map(S.mods.map(m => [m.id, m.name]));
  if (sel && !names.has(sel)) sel = null;
  issuesStale = true;
  render();
  refreshPlan();
  // The health check reads every plugin; re-run it once a burst of changes settles.
  clearTimeout(healthTimer);
  healthTimer = setTimeout(runHealth, tab === 'problems' ? 0 : 800);
  if (sel) loadDetails();
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

const conflictMap = () => Object.fromEntries(S.conflicts.map(c => [c.id, c]));
const filterText = () => $('#filter').value.trim().toLowerCase();

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

function renderMods() {
  const C = conflictMap();
  const selC = sel && C[sel];
  const broken = new Map();
  for (const [id, after] of S.broken) broken.set(id, [...(broken.get(id) ?? []), after]);
  const f = filterText();
  $('#mods').classList.toggle('filtering', !!f);
  $('#mods tbody').innerHTML = S.mods.map(m => {
    if (f && !m.name.toLowerCase().includes(f)) return '';
    const cls = [
      m.enabled ? '' : 'off',
      m.id === sel ? 'sel' : '',
      selC?.overwrites.includes(m.id) ? 'beaten' : '',
      selC?.overwritten_by.includes(m.id) ? 'beats' : '',
    ].join(' ');
    const b = broken.get(m.id);
    const warn = b ? `<span class="warn-icon" title="Should load after ${esc(b.map(nameOf).join(', '))} (a mod rule)">⚠</span>` : '';
    return `<tr data-id="${esc(m.id)}" class="${cls}">
      <td><span class="grip" title="Drag to change priority">⋮⋮</span></td>
      <td><input type="checkbox" ${m.enabled ? 'checked' : ''} aria-label="Enable ${esc(m.name)}"></td>
      <td class="name">${esc(m.name)}${warn}</td>
      <td>${conflictPills(C[m.id])}</td>
      <td class="num">${m.files}</td>
      <td class="num">${m.priority}</td></tr>`;
  }).join('');
  $('#mods-empty').hidden = S.mods.length > 0;
  const n = S.broken.length;
  $('#rules-notice').hidden = !n;
  $('#rules-notice').innerHTML = n ? `<span>⚠ ${n === 1 ? 'A mod rule is' : `${n} mod rules are`} broken by the current order.</span><button id="sort-mods">Sort by rules…</button>` : '';
}

function renderPlugins() {
  const f = filterText();
  $('#plugins').classList.toggle('filtering', !!f);
  $('#plugins tbody').innerHTML = S.plugins.map((p, i) => {
    if (f && !p.name.toLowerCase().includes(f)) return '';
    const type = p.master ? (p.light ? 'ESL' : 'ESM') : p.light ? 'ESP · light' : 'ESP';
    const from = p.implicit ? 'Base game' : p.mod_id ? nameOf(p.mod_id) : 'Data folder';
    const issue = p.error ? `<span class="pill bad" title="${esc(p.error)}">Unreadable</span>`
      : p.enabled && p.missing_masters.length ? `<span class="pill bad" title="Needs ${esc(p.missing_masters.join(', '))} enabled and loaded earlier">Missing masters</span>` : '';
    return `<tr data-name="${esc(p.name)}" class="${p.implicit ? 'locked' : ''} ${p.enabled ? '' : 'off'}">
      <td>${p.implicit ? '' : '<span class="grip" title="Drag to change load order">⋮⋮</span>'}</td>
      <td><input type="checkbox" ${p.enabled ? 'checked' : ''} ${p.implicit ? 'disabled title="Always loaded by the game"' : `aria-label="Enable ${esc(p.name)}"`}></td>
      <td class="name">${esc(p.name)}</td>
      <td class="muted">${type}</td>
      <td class="muted">${esc(from)}</td>
      <td>${issue}</td>
      <td class="num">${i}</td></tr>`;
  }).join('');
}

function renderHistory() {
  $('#history').innerHTML = S.history.length
    ? S.history.map(h => `<li>${esc(h)}</li>`).join('')
    : '<li class="muted">No changes to this profile yet.</li>';
}

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

function renderDetails() {
  const box = $('#details');
  const m = sel && S.mods.find(x => x.id === sel);
  box.hidden = tab !== 'mods' || !m;
  if (!m) return;
  const c = conflictMap()[m.id];
  const rule = S.rules.find(r => r.id === m.id)?.after ?? [];
  const others = S.mods.filter(o => o.id !== m.id && !rule.includes(o.id));
  const people = (title, ids) => ids.length
    ? `<div class="small muted">${title}</div><ul>${ids.map(id => `<li><button class="link" data-goto="${esc(id)}">${esc(nameOf(id))}</button></li>`).join('')}</ul>` : '';
  const d = details?.id === m.id ? details : null;
  box.innerHTML = `
    <h2>${esc(m.name)}</h2>
    ${m.id !== m.name ? `<div class="muted small">id: ${esc(m.id)}</div>` : ''}
    <div class="row">
      <label class="row" style="margin:0"><input type="checkbox" data-act="enable" ${m.enabled ? 'checked' : ''}> Enabled</label>
      <div class="spacer"></div>
      <button class="danger" data-act="uninstall">Uninstall…</button>
    </div>
    <h3>Conflicts</h3>
    ${!c || (!c.overwrites.length && !c.overwritten_by.length)
      ? `<p class="muted">No file conflicts${m.enabled ? '' : ' (only enabled mods are compared)'}.</p>`
      : `${c.fully_overridden ? '<p><span class="pill bad">Overridden</span> Every file is replaced by other mods, so this mod has no effect.</p>' : ''}
         ${people('Wins against', c.overwrites)}
         ${people('Loses to', c.overwritten_by)}
         <div class="legend"><span><i style="background:var(--win)"></i>this mod wins</span><span><i style="background:var(--lose)"></i>wins over this mod</span></div>`}
    <h3>Loads after</h3>
    <div class="chips">${rule.map(id => `<span class="chip">${esc(nameOf(id))}<button data-unrule="${esc(id)}" title="Remove from the rule" aria-label="Remove ${esc(nameOf(id))}">×</button></span>`).join('') || '<span class="muted">No rule.</span>'}</div>
    <div class="row"><select id="rule-add" aria-label="Add a mod this one loads after"><option value="">Load after another mod…</option>${others.map(o => `<option value="${esc(o.id)}">${esc(o.name)}</option>`).join('')}</select></div>
    <p class="muted small">Rules are suggestions: dragging is never blocked, and Sort by rules fixes the order.</p>
    <h3>Files (${m.files})</h3>
    ${m.plugins.length ? `<p>Plugins: ${esc(m.plugins.join(', '))}</p>` : ''}
    <div class="files">${d ? d.files.map(esc).join('<br>') + (d.files_truncated ? '<br>…' : '') : '…'}</div>
    ${d?.meta?.source ? `<h3>Installed from</h3><div class="small muted">${esc(d.meta.source)}</div>` : ''}`;
}

async function loadDetails() {
  const id = sel;
  try { details = await call('details', { id }); } catch { details = null; }
  if (id === sel) renderDetails();
}

function select(id) {
  sel = sel === id ? null : id;
  renderMods();
  renderDetails();
  if (sel) loadDetails();
}

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

function renderProblems() {
  const rank = { error: 0, warning: 1, info: 2 };
  const list = [...issues].sort((a, b) => rank[a.severity] - rank[b.severity]);
  $('#issues').innerHTML = list.length
    ? list.map(i => `<li class="${esc(i.severity)}"><span class="sev">${esc(i.severity)}</span><span class="subject">${esc(i.subject)}</span><span class="message">${esc(i.message)}</span></li>`).join('')
    : '<li class="info"><span class="sev">ok</span><span class="subject">No problems found.</span></li>';
  const serious = issues.filter(i => i.severity !== 'info').length;
  $('#n-problems').textContent = serious || '';
  $('#n-problems').classList.toggle('bad', issues.some(i => i.severity === 'error'));
  $('#problems-note').textContent = `${plural(issues.length, 'finding')}. LOOT's messages are included once its masterlist is downloaded.`;
}

// ---- Tabs, filter, keys ---------------------------------------------------------------------

function showTab(t) {
  tab = t;
  $$('.tabs [data-tab]').forEach(b => b.classList.toggle('on', b.dataset.tab === t));
  for (const p of ['mods', 'plugins', 'problems', 'history']) $('#pane-' + p).hidden = p !== t;
  $('#filter').hidden = t !== 'mods' && t !== 'plugins';
  renderDetails();
  if (t === 'problems' && issuesStale) runHealth();
}
$$('.tabs [data-tab]').forEach(b => { b.onclick = () => showTab(b.dataset.tab); });
$('#filter').oninput = () => { renderMods(); renderPlugins(); };
$('#recheck').onclick = runHealth;

const undo = () => act('Undo', async () => { const l = await call('undo'); toast(l ? `Undid: ${l}` : 'Nothing to undo.'); });
const redo = () => act('Redo', async () => { const l = await call('redo'); toast(l ? `Redid: ${l}` : 'Nothing to redo.'); });
$('#undo').onclick = undo;
$('#redo').onclick = redo;
document.addEventListener('keydown', e => {
  if (!S || $('#dlg').open || e.target.matches('input, select, textarea')) return;
  const k = e.key.toLowerCase();
  if (e.ctrlKey && k === 'z' && !e.shiftKey) { e.preventDefault(); undo(); }
  else if (e.ctrlKey && (k === 'y' || (k === 'z' && e.shiftKey))) { e.preventDefault(); redo(); }
  else if (k === 'escape' && sel) select(sel);
});

// ---- Mods -----------------------------------------------------------------------------------

$('#mods tbody').addEventListener('click', e => {
  const tr = e.target.closest('tr');
  if (!tr) return;
  if (e.target.matches('input[type=checkbox]')) {
    if (busy) return e.preventDefault();
    act('Changing the mod', () => call('enable_mod', { id: tr.dataset.id, enabled: e.target.checked }));
  } else if (!e.target.closest('.grip')) select(tr.dataset.id);
});
draggable($('#mods tbody'), (id, from, to) => act('Moving the mod', () => call('move_mod', { id, priority: to })));

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
  const t = e.target;
  const m = S.mods.find(x => x.id === sel);
  if (!m) return;
  if (t.dataset.goto) {
    select(t.dataset.goto);
    $(`#mods tr[data-id="${CSS.escape(t.dataset.goto)}"]`)?.scrollIntoView({ block: 'nearest' });
  } else if (t.dataset.unrule) {
    const after = (S.rules.find(r => r.id === m.id)?.after ?? []).filter(a => a !== t.dataset.unrule);
    act('Changing the rule', () => call('set_rule', { id: m.id, after }));
  } else if (t.dataset.act === 'enable') {
    if (busy) return e.preventDefault();
    act('Changing the mod', () => call('enable_mod', { id: m.id, enabled: t.checked }));
  } else if (t.dataset.act === 'uninstall') {
    if (await confirmBox(`Uninstall ${m.name}?`, 'Its files move to the instance’s trash folder. Undo brings it back.', 'Uninstall')) {
      act('Uninstalling', () => call('uninstall', { id: m.id }));
    }
  }
});
$('#details').addEventListener('change', e => {
  if (e.target.id !== 'rule-add' || !e.target.value) return;
  const after = [...(S.rules.find(r => r.id === sel)?.after ?? []), e.target.value];
  act('Adding the rule', () => call('set_rule', { id: sel, after }));
});

// ---- Plugins --------------------------------------------------------------------------------

$('#plugins tbody').addEventListener('click', e => {
  const tr = e.target.closest('tr');
  if (!tr || !e.target.matches('input[type=checkbox]')) return;
  if (busy) return e.preventDefault();
  act('Changing the plugin', () => call('enable_plugin', { name: tr.dataset.name, enabled: e.target.checked }));
});
draggable($('#plugins tbody'), (name, from, to) => {
  const order = S.plugins.filter(p => !p.implicit).map(p => p.name);
  order.splice(to, 0, ...order.splice(from, 1));
  act('Moving the plugin', () => call('plugin_order', { names: order }));
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

function draggable(tbody, onMove) {
  tbody.addEventListener('pointerdown', e => {
    const grip = e.target.closest('.grip');
    if (!grip || busy || e.button !== 0) return;
    const row = grip.closest('tr');
    const rows = [...tbody.rows].filter(r => !r.classList.contains('locked'));
    const from = rows.indexOf(row);
    if (from < 0) return;
    e.preventDefault();
    const scroller = tbody.closest('.scroll');
    let to = from, mark = null, lastY = e.clientY;
    const clear = () => { mark?.classList.remove('drop-before', 'drop-after'); mark = null; };
    const timer = setInterval(() => {
      const r = scroller.getBoundingClientRect();
      if (lastY < r.top + 50) scroller.scrollTop -= 14;
      else if (lastY > r.bottom - 30) scroller.scrollTop += 14;
    }, 16);
    row.classList.add('dragging');
    document.body.classList.add('dragging');
    const move = ev => {
      lastY = ev.clientY;
      clear();
      const over = document.elementFromPoint(ev.clientX, ev.clientY)?.closest('tr');
      const i = rows.indexOf(over);
      if (i < 0) return;
      const r = over.getBoundingClientRect();
      const after = ev.clientY > r.top + r.height / 2;
      const slot = i + (after ? 1 : 0);
      to = slot > from ? slot - 1 : slot;
      mark = over;
      over.classList.add(after ? 'drop-after' : 'drop-before');
    };
    const up = () => {
      clearInterval(timer);
      removeEventListener('pointermove', move);
      clear();
      row.classList.remove('dragging');
      document.body.classList.remove('dragging');
      if (to !== from) onMove(row.dataset.id ?? row.dataset.name, from, to);
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
  const r = await dialog({ title: 'Deploy to the game folder', body, actions: n ? [['cancel', 'Cancel'], ['apply', 'Deploy', 'primary']] : [['cancel', 'Close']] });
  if (r !== 'apply') return;
  const res = await act('Deploying', () => call('deploy'));
  if (res) toast(`Deployed: ${res.link.length} linked, ${res.remove.length} removed.`);
};

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
    + '<button class="item" data-rename="1"><span class="check"></span>Rename this profile…</button>';
  pop.hidden = false;
};
document.addEventListener('click', e => { if (!e.target.closest('.menu')) $('#profile-pop').hidden = true; });

$('#profile-pop').onclick = async e => {
  const t = e.target.closest('[data-delete],[data-switch],[data-new],[data-rename]');
  if (!t) return;
  $('#profile-pop').hidden = true;
  const cur = S.info.profile;
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

// ---- Install --------------------------------------------------------------------------------

$('#install').onclick = async () => {
  const picked = await T.dialog.open({ multiple: true, title: 'Install mods', filters: [{ name: 'Mod archives', extensions: ['zip', '7z'] }] });
  if (picked) install([].concat(picked));
};

async function install(paths) {
  for (const path of paths) {
    const r = await act(`Installing ${basename(path)}`, () => call('install', { path }));
    if (!r) continue;
    const notes = [
      r.plugins.length ? `Plugins: ${r.plugins.join(', ')}` : '',
      r.fomod ? 'Its FOMOD installer ran with the default options.' : '',
      ...r.warnings,
    ].filter(Boolean);
    toast(`Installed ${r.name} (${plural(r.files, 'file')}).${notes.length ? '\n' + notes.join('\n') : ''}`, r.warnings.length ? 'warn' : 'info');
  }
}

T.webview.getCurrentWebview().onDragDropEvent(e => {
  const type = e.payload.type;
  if (type === 'enter' || type === 'over') {
    $('#dropzone').hidden = !S || busy;
  } else {
    $('#dropzone').hidden = true;
    if (type === 'drop' && S && e.payload.paths?.length) install(e.payload.paths);
  }
});

// ---- Dialogs and toasts ---------------------------------------------------------------------

/** Modal dialog. actions: [[value, label, class]]. Resolves to the chosen value (the input's
 *  text when `input` is given), or null when cancelled. */
function dialog({ title, body = '', actions, input }) {
  const d = $('#dlg');
  const field = input === undefined ? '' : `<input type="text" value="${esc(input)}" spellcheck="false">`;
  d.innerHTML = `<form method="dialog"><h2>${esc(title)}</h2><div class="body">${body}${field}</div>
    <div class="actions">${actions.map(([v, label, cls]) => `<button value="${v}" class="${cls ?? ''}">${esc(label)}</button>`).join('')}</div></form>`;
  const text = $('input', d);
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
