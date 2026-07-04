// DOM focus → KeyContext resolution (docs/gui/05 §8.2). The dispatcher itself lives in the
// keymap store (`match`, lookup order) + actions.ts (runtime effect); this file only maps the
// current focus to one of the 11 contexts. A container may declare `data-kctx="<Context>"` to
// override the view-based default (used for the Search box, DJ Gem input, and Queue dock);
// otherwise the active view decides.

import type { View } from '../stores/ui.svelte';
import type { KeyContext } from '../stores/keymap.svelte';

const VIEW_CONTEXT: Record<View, KeyContext> = {
  now: 'Player',
  search: 'SearchResults',
  library: 'Library',
  ai: 'AiSuggestions',
  settings: 'Settings',
};

const KCTX_VALUES = new Set<KeyContext>([
  'Player',
  'Queue',
  'SearchInput',
  'SearchResults',
  'Library',
  'Playlists',
  'Settings',
  'AiInput',
  'AiSuggestions',
  'Common',
  'Global',
]);

/** Resolve the active KeyContext: an explicit `data-kctx` container wins, else the view. */
export function resolveContext(view: View, active: Element | null): KeyContext {
  const tagged = active?.closest?.('[data-kctx]')?.getAttribute('data-kctx');
  if (tagged && KCTX_VALUES.has(tagged as KeyContext)) return tagged as KeyContext;
  return VIEW_CONTEXT[view];
}
