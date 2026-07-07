// The keymap store (docs/gui/05 §8, 07 §8). Mirrors the core's remappable keymap read model
// — the full binding table plus `ActionInfo` per (context, action) — and drives both the
// in-webview dispatcher (lib/keyboard/) and the Settings→Hotkeys / Help surfaces from one
// source of truth. Rides the `settings` push, exactly like `anim`/`theme`.
//
// ── PROVISIONAL wire shape (settings.hotkeys) ────────────────────────────────────────────
// `KeymapModel` rides the `settings` snapshot (docs/gui/02 §11.6). Only the demo core speaks
// it today; reconcile with the ts-rs export + the real `Apply(Keymap(Bind|Unbind|ResetAll))`
// wire when settings-v8 lands. **Conflict detection stays core-side** — the demo does a
// simple shadow check and returns it in the `keymap_bind` reply; the GUI never reimplements
// the `src/keymap.rs` shadowing rules.

import type { Client } from '../ipc/client';
import type { SettingsSnapshot } from './settings.svelte';

/** The 13 KeyContexts in src/keymap.rs, in GUI display order (specific → fallbacks). */
export type KeyContext =
  | 'Player'
  | 'NowPlaying'
  | 'MpvOverlay'
  | 'Queue'
  | 'SearchInput'
  | 'SearchResults'
  | 'Library'
  | 'Playlists'
  | 'Settings'
  | 'AiInput'
  | 'AiSuggestions'
  | 'Common'
  | 'Global';

export const KEY_CONTEXTS: KeyContext[] = [
  'Player',
  'NowPlaying',
  'MpvOverlay',
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
];

/** Human titles (the GUI stand-in for CONTEXT_META, src/keymap.rs:453). */
export const CONTEXT_LABELS: Record<KeyContext, string> = {
  Player: 'Now Playing',
  NowPlaying: "What's playing card",
  MpvOverlay: 'mpv video overlay',
  Queue: 'Queue',
  SearchInput: 'Search box',
  SearchResults: 'Search results',
  Library: 'Library',
  Playlists: 'Playlists',
  Settings: 'Settings',
  AiInput: 'DJ Gem — input',
  AiSuggestions: 'DJ Gem — suggestions',
  Common: 'Common',
  Global: 'Global',
};

export interface ActionInfo {
  /** Stable id, unique within its context. */
  id: string;
  context: KeyContext;
  /** Localized action label (per-(context, action), so Help reads like the TUI). */
  label: string;
  /** The factory chord, for per-row reset. */
  default_chord: string;
}

export interface KeymapModel {
  /** The persisted binding table: `"<context>.<action>" → "<chord>"` (src/keymap.rs form). */
  bindings: Record<string, string>;
  /** Every action's stable id, context, label, and default chord. */
  actions: ActionInfo[];
}

/** Inline conflict info surfaced from the core's bind reply (shadowing stays core-side). */
export interface KeymapConflict {
  /** `"<context>.<action>"` key of the binding we just set. */
  key: string;
  chord: string;
  /** The action id the chord already resolves to (what it shadows / collides with). */
  shadows: string;
}

// ── the canonical GUI keymap seed (the demo core emits it; the real core replaces it) ──────
// Representative actions across all 13 contexts. Runtime effect lives in lib/keyboard/actions
// for the ones with a GUI handler; the rest still render + rebind (their effect lands later).
const ACTIONS: ActionInfo[] = [
  { id: 'play_pause', context: 'Player', label: 'Play / pause', default_chord: 'Space' },
  { id: 'seek_back', context: 'Player', label: 'Seek −5 s', default_chord: 'Left' },
  { id: 'seek_forward', context: 'Player', label: 'Seek +5 s', default_chord: 'Right' },
  { id: 'volume_up', context: 'Player', label: 'Volume +5', default_chord: 'Up' },
  { id: 'volume_down', context: 'Player', label: 'Volume −5', default_chord: 'Down' },
  { id: 'toggle_mute', context: 'Player', label: 'Mute / unmute', default_chord: 'm' },
  { id: 'next', context: 'Player', label: 'Next track', default_chord: '.' },
  { id: 'prev', context: 'Player', label: 'Previous track', default_chord: ',' },
  { id: 'toggle_shuffle', context: 'Player', label: 'Shuffle', default_chord: 'S' },
  { id: 'cycle_repeat', context: 'Player', label: 'Repeat mode', default_chord: 'r' },
  { id: 'cycle_rating', context: 'Player', label: 'Cycle rating', default_chord: 'f' },
  { id: 'identify_now_playing', context: 'Player', label: "What's playing", default_chord: 'i' },
  { id: 'play_video', context: 'Player', label: 'Video overlay (mpv)', default_chord: 'v' },
  {
    id: 'toggle_video_layout',
    context: 'Player',
    label: 'Video size / position',
    default_chord: 'V',
  },
  {
    id: 'now_playing_favorite',
    context: 'NowPlaying',
    label: 'Favorite / unfavorite',
    default_chord: 'f',
  },
  { id: 'now_playing_ask_ai', context: 'NowPlaying', label: 'Ask DJ Gem', default_chord: 'g' },
  {
    id: 'video_toggle_pause',
    context: 'MpvOverlay',
    label: 'Video play / pause',
    default_chord: 'Space',
  },
  { id: 'video_next', context: 'MpvOverlay', label: 'Next video', default_chord: '.' },
  { id: 'video_prev', context: 'MpvOverlay', label: 'Previous video', default_chord: ',' },
  { id: 'video_close', context: 'MpvOverlay', label: 'Close video', default_chord: 'q' },
  { id: 'video_toggle_fullscreen', context: 'MpvOverlay', label: 'Fullscreen', default_chord: 'f' },
  { id: 'video_toggle_mute', context: 'MpvOverlay', label: 'Mute / unmute', default_chord: 'm' },
  { id: 'clear_upcoming', context: 'Queue', label: 'Clear upcoming', default_chord: 'c' },
  { id: 'play_pause', context: 'Queue', label: 'Play / pause', default_chord: 'Space' },
  { id: 'focus_query', context: 'SearchInput', label: 'Edit query', default_chord: '/' },
  { id: 'clear_query', context: 'SearchInput', label: 'Clear query', default_chord: 'Ctrl+u' },
  { id: 'play_result', context: 'SearchResults', label: 'Play selection', default_chord: 'Enter' },
  { id: 'enqueue_result', context: 'SearchResults', label: 'Enqueue', default_chord: 'e' },
  { id: 'play_item', context: 'Library', label: 'Play selection', default_chord: 'Enter' },
  { id: 'enqueue_item', context: 'Library', label: 'Enqueue', default_chord: 'e' },
  { id: 'open_playlist', context: 'Playlists', label: 'Open playlist', default_chord: 'Enter' },
  { id: 'next_tab', context: 'Settings', label: 'Next tab', default_chord: 'Tab' },
  { id: 'prev_tab', context: 'Settings', label: 'Previous tab', default_chord: 'BackTab' },
  { id: 'send_message', context: 'AiInput', label: 'Send message', default_chord: 'Enter' },
  {
    id: 'play_suggestion',
    context: 'AiSuggestions',
    label: 'Play suggestion',
    default_chord: 'Enter',
  },
  { id: 'toggle_queue', context: 'Common', label: 'Toggle queue panel', default_chord: 'q' },
  { id: 'back', context: 'Common', label: 'Close overlay / dialog', default_chord: 'Esc' },
  { id: 'help', context: 'Global', label: 'Help', default_chord: '?' },
  { id: 'view_now', context: 'Global', label: 'Now Playing', default_chord: '1' },
  { id: 'view_search', context: 'Global', label: 'Search', default_chord: '2' },
  { id: 'view_library', context: 'Global', label: 'Library', default_chord: '3' },
  { id: 'view_ai', context: 'Global', label: 'DJ Gem', default_chord: '4' },
  { id: 'view_settings', context: 'Global', label: 'Settings', default_chord: '5' },
];

/** The default keymap read model — the demo core's seed and the per-row reset target. */
export function defaultKeymap(): KeymapModel {
  const actions = ACTIONS.map((a) => ({ ...a }));
  const bindings: Record<string, string> = {};
  for (const a of actions) bindings[`${a.context}.${a.id}`] = a.default_chord;
  return { bindings, actions };
}

export class KeymapStore {
  /** Last authoritative keymap; null until the first `settings` snapshot. */
  model = $state<KeymapModel | null>(null);
  /** The active rebind target, or null. ChordCapture reads/writes this. */
  capture = $state<{ context: KeyContext; action: string } | null>(null);
  /** The most recent bind's conflict (from the core reply), cleared on the next edit. */
  conflict = $state<KeymapConflict | null>(null);
  /** Sparse optimistic overlay `"<ctx>.<action>" → chord | null(unbound)`. */
  #pending = $state<Record<string, string | null>>({});
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('settings', (payload) => this.#onPush(payload as SettingsSnapshot));
  }

  get actions(): ActionInfo[] {
    return this.model?.actions ?? [];
  }

  /** Actions grouped by context (only contexts that have at least one action). */
  get groups(): Array<{ context: KeyContext; label: string; actions: ActionInfo[] }> {
    const out: Array<{ context: KeyContext; label: string; actions: ActionInfo[] }> = [];
    for (const ctx of KEY_CONTEXTS) {
      const actions = this.actions.filter((a) => a.context === ctx);
      if (actions.length) out.push({ context: ctx, label: CONTEXT_LABELS[ctx], actions });
    }
    return out;
  }

  /** The chord currently bound to an action (pending overlay ?? model), '' if unbound. */
  chordFor(a: ActionInfo): string {
    const key = `${a.context}.${a.id}`;
    const p = this.#pending[key];
    if (p !== undefined) return p ?? '';
    return this.model?.bindings[key] ?? '';
  }

  /**
   * Resolve a normalized chord in `context` to an action id, honoring the lookup order
   * specific-context → Common → Global (first binding wins), matching keymap.rs.
   */
  match(context: KeyContext, chord: string): string | null {
    const bindings = this.#merged();
    const order: KeyContext[] =
      context === 'Common'
        ? ['Common', 'Global']
        : context === 'Global'
          ? ['Global']
          : [context, 'Common', 'Global'];
    for (const ctx of order) {
      for (const a of this.actions) {
        if (a.context === ctx && bindings[`${ctx}.${a.id}`] === chord) return a.id;
      }
    }
    return null;
  }

  // ── mutations (Apply(Keymap(...))) ───────────────────────────────────────────────────

  /** Bind a chord (optimistic); the reply carries any core-side conflict for inline display. */
  async rebind(context: KeyContext, action: string, chord: string): Promise<void> {
    const key = `${context}.${action}`;
    this.#pending = { ...this.#pending, [key]: chord };
    this.conflict = null;
    try {
      const res = await this.#client.req<{ conflict?: { shadows?: string } }>('keymap_bind', {
        context,
        action,
        chord,
      });
      const shadows = res?.conflict?.shadows;
      this.conflict = shadows ? { key, chord, shadows: String(shadows) } : null;
    } catch {
      // The real core may not speak keymap yet — keep the optimistic overlay, no conflict info.
    }
  }

  unbind(context: KeyContext, action: string): void {
    const key = `${context}.${action}`;
    this.#pending = { ...this.#pending, [key]: null };
    this.conflict = null;
    this.#client.cmd('keymap_unbind', { context, action });
  }

  /** Per-row reset: rebind the action to its factory chord. */
  resetBinding(a: ActionInfo): void {
    void this.rebind(a.context, a.id, a.default_chord);
  }

  /** Restore every chord to its default (danger zone). The confirming push refills bindings. */
  resetAll(): void {
    this.#pending = {};
    this.conflict = null;
    this.#client.cmd('keymap_reset_all');
  }

  // ── capture ──────────────────────────────────────────────────────────────────────────

  startCapture(context: KeyContext, action: string): void {
    this.capture = { context, action };
    this.conflict = null;
  }

  cancelCapture(): void {
    this.capture = null;
  }

  applyCapture(chord: string): void {
    const c = this.capture;
    this.capture = null;
    if (c) void this.rebind(c.context, c.action, chord);
  }

  // ── internals ────────────────────────────────────────────────────────────────────────

  #merged(): Record<string, string> {
    const out: Record<string, string> = { ...(this.model?.bindings ?? {}) };
    for (const [key, val] of Object.entries(this.#pending)) {
      if (val == null) delete out[key];
      else out[key] = val;
    }
    return out;
  }

  #onPush(snap: SettingsSnapshot): void {
    const km = snap?.model?.keymap;
    if (!km) return;
    this.model = km;
    // Drop the overlay entries the authoritative model now agrees with.
    const next: Record<string, string | null> = {};
    for (const [key, val] of Object.entries(this.#pending)) {
      const authoritative = km.bindings[key] ?? null;
      if (authoritative !== (val ?? null)) next[key] = val;
    }
    this.#pending = next;
  }
}
