// Translations. The window is written in English; with a language chosen (Settings), its text is
// swapped for that language's as it appears: whole text pieces and tooltips found in
// lang/<code>.json, where "{0}" stands for a part that changes (a number, a name). Text with no
// translation stays English, and names (mods, plugins, files, saves) are left alone.
'use strict';
const I18N = (() => {
  const LANGS = { en: 'English', de: 'Deutsch', es: 'Español', fr: 'Français', ru: 'Русский', 'zh-CN': '简体中文' };
  let lang = 'en';
  try { lang = localStorage.getItem('rmm.lang') || 'en'; } catch { /* no storage: English */ }
  if (!LANGS[lang]) lang = 'en';
  const exact = new Map();
  const patterns = [];
  const ATTRS = ['title', 'placeholder', 'aria-label'];
  // Names in the lists, and anything marked translate="no".
  const keep = el => !el || el.closest('[translate="no"], script, style, textarea') || el.matches('td.name, td.name > b');

  function translate(text) {
    const t = text.trim().replace(/\s+/g, ' ');
    if (!t || !/[A-Za-z]/.test(t)) return null;
    const hit = exact.get(t);
    let out = hit;
    if (out === undefined) {
      for (const p of patterns) {
        const m = p.re.exec(t);
        // The parts that change are often text of their own ("5 files", a nested sentence).
        if (m) { out = p.to.replace(/\{(\d+)\}/g, (_, i) => m[+i + 1] === undefined ? '' : translate(m[+i + 1]) ?? m[+i + 1]); break; }
      }
    }
    if (out === undefined) return null;
    const lead = text.match(/^\s*/)[0], trail = text.match(/\s*$/)[0];
    return lead + out + trail;
  }

  function textNode(n) {
    if (keep(n.parentElement)) return;
    const t = translate(n.nodeValue);
    if (t !== null && t !== n.nodeValue) n.nodeValue = t;
  }
  function element(el) {
    if (el.closest('[translate="no"]')) return;
    for (const a of ATTRS) {
      const v = el.getAttribute(a);
      if (v && !(a === 'title' && el.matches('td.name'))) {
        const t = translate(v);
        if (t !== null && t !== v) el.setAttribute(a, t);
      }
    }
  }
  function walk(root) {
    if (root.nodeType === Node.TEXT_NODE) return textNode(root);
    if (root.nodeType !== Node.ELEMENT_NODE) return;
    element(root);
    const it = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
    for (let n = it.nextNode(); n; n = it.nextNode()) n.nodeType === Node.TEXT_NODE ? textNode(n) : element(n);
  }

  async function start() {
    if (lang === 'en') return;
    let dict;
    try { dict = await (await fetch(`lang/${lang}.json`)).json(); } catch { return; }
    for (let [en, to] of Object.entries(dict)) {
      if (!to || to === en) continue;
      // "#{0} files": a count, where {0} is only ever a number.
      const count = en.startsWith('#');
      if (count) en = en.slice(1);
      if (!/\{\d+\}/.test(en)) { exact.set(en, to); continue; }
      const parts = en.split(/\{\d+\}/);
      let re = '^';
      parts.forEach((s, i) => {
        re += s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
        if (i === parts.length - 1) return;
        // A part glued to a word ("Update{0}") is an optional tail like " 1.2", never more letters,
        // and may be empty.
        const glued = /[A-Za-z]$/.test(s);
        const before = /^[A-Za-z]/.test(parts[i + 1]) ? '(?<![A-Za-z])' : '';
        re += count ? '(\\d[\\d.,\\s\\u00a0]*?)' : glued ? `(?![A-Za-z])(.*?)${before}` : `(.+?)${before}`;
      });
      patterns.push({ re: new RegExp(re + '$', 's'), to });
    }
    // Longer patterns first: "Deployed {0} files to {1}" before "Deployed {0}".
    patterns.sort((a, b) => b.re.source.length - a.re.source.length);
    document.documentElement.lang = lang;
    walk(document.body);
    new MutationObserver(list => {
      for (const m of list) {
        if (m.type === 'childList') m.addedNodes.forEach(walk);
        else if (m.type === 'characterData') textNode(m.target);
        else if (m.type === 'attributes') element(m.target);
      }
    }).observe(document.body, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ATTRS });
  }

  return {
    LANGS,
    get lang() { return lang; },
    // Choosing reloads the window, which then starts in that language.
    set(code) {
      try { localStorage.setItem('rmm.lang', code); } catch { /* stays for this run only */ }
      location.reload();
    },
    // For text the window builds itself (confirm() dialogs, alerts).
    t(text) { return translate(text) ?? text; },
    start,
  };
})();
I18N.start();
