// DOM focus → KeyContext resolution (docs/gui/05 §8.2). The dispatcher itself lives in the
// keymap store (`match`, lookup order) + actions.ts (runtime effect); this file only maps the
// current focus to one of the src/keymap.rs context ids. A container may declare
// `data-kctx="<context_id>"` to override the view-based default (used for the Search box,
// DJ Gem input, and Queue dock); otherwise the active view decides.

import type { View } from '../stores/ui.svelte';
import type { KeyContext } from '../stores/keymap.svelte';

const VIEW_CONTEXT: Record<View, KeyContext> = {
  now: 'player',
  search: 'search_results',
  library: 'library',
  ai: 'ai_suggestions',
  settings: 'settings',
};

const KCTX_VALUES = new Set<KeyContext>([
  'player',
  'now_playing',
  'mpv_overlay',
  'queue',
  'search_input',
  'search_results',
  'library',
  'local_deck',
  'playlists',
  'settings',
  'ai_input',
  'ai_suggestions',
  'common',
  'global',
]);

/** Resolve the active KeyContext: an explicit `data-kctx` container wins, else the view. */
export function resolveContext(view: View, active: Element | null): KeyContext {
  const tagged = active?.closest?.('[data-kctx]')?.getAttribute('data-kctx');
  if (tagged && KCTX_VALUES.has(tagged as KeyContext)) return tagged as KeyContext;
  return VIEW_CONTEXT[view];
}
