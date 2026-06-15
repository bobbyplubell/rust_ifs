// world-picker.js — a compact <select> for choosing the active world.
//
// A "world" is one coordinator (one flock). Picking one persists it to
// localStorage (via config.setWorld) and reloads so every module re-resolves
// COORDINATOR. The current world (which may be a custom ?world= URL not in the
// list) is always shown as selected, plus a "Custom URL…" option that prompts.
//
// The identity (key) is shared across worlds — only credits/reputation differ.

import { WORLDS, COORDINATOR, setWorld } from '../config.js';

const CUSTOM = '__custom__';

/** Build the <select> element. Append it wherever you want in the header. */
export function buildWorldPicker() {
  const sel = document.createElement('select');
  sel.className = 'world-picker';
  sel.title = 'choose which world (coordinator) to connect to — your key is ' +
    'shared across worlds; only credits & reputation differ per world';

  const known = WORLDS.map((w) => w.url.replace(/\/+$/, ''));
  let matched = false;
  for (const w of WORLDS) {
    const opt = document.createElement('option');
    opt.value = w.url.replace(/\/+$/, '');
    opt.textContent = w.name;
    if (opt.value === COORDINATOR) { opt.selected = true; matched = true; }
    sel.append(opt);
  }
  // The active world isn't one of the known ones (a custom ?world=): show it.
  if (!matched && COORDINATOR) {
    const opt = document.createElement('option');
    opt.value = COORDINATOR;
    opt.textContent = `Custom (${COORDINATOR})`;
    opt.selected = true;
    sel.append(opt);
  }
  const customOpt = document.createElement('option');
  customOpt.value = CUSTOM;
  customOpt.textContent = 'Custom URL…';
  sel.append(customOpt);

  sel.addEventListener('change', () => {
    let url = sel.value;
    if (url === CUSTOM) {
      url = prompt('Coordinator API base URL (e.g. https://api.example.com):', COORDINATOR);
      if (!url) { sel.value = COORDINATOR; return; } // cancelled — revert
    }
    if (url.replace(/\/+$/, '') === COORDINATOR) return; // no change
    setWorld(url);
    location.reload();
  });

  return sel;
}

/** Convenience: build the picker and append it to a container by selector/el. */
export function mountWorldPicker(target) {
  const host = typeof target === 'string' ? document.querySelector(target) : target;
  if (!host) return null;
  const sel = buildWorldPicker();
  host.append(sel);
  return sel;
}
