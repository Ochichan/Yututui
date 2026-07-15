// The keymap store (docs/gui/05 §8, 07 §8). Mirrors the core's remappable keymap read model
// — the full binding table plus `ActionInfo` per (context, action) — and drives both the
// in-webview dispatcher (lib/keyboard/) and the Settings→Hotkeys / Help surfaces from one
// source of truth. Rides the `settings` push, exactly like `anim`/`theme`.
//
// The wire is the real one (C6): `KeymapSettingsModel` carries the FULL effective map from
// `KeyMap::wire_bindings()` (`"" = unbound`) plus the `wire_actions()` catalog — snake_case
// context/action ids and canonical config-format chords, the exact persisted form of
// src/keymap.rs (05 §8.1). **Conflict detection stays core-side** — `keymap_bind`'s reply
// carries the shadowed binding as display text; the GUI never reimplements the shadowing
// rules. The demo core speaks the identical vocabulary (its seed mirrors default_bindings()).

import type { Client } from '../ipc/client';
import type { SettingsSnapshot } from './settings.svelte';
import type { ActionInfoModel } from '../../generated/protocol/ActionInfoModel';

/** The KeyContext ids of src/keymap.rs (CONTEXT_META), in GUI display order. */
export type KeyContext =
  | 'player'
  | 'now_playing'
  | 'mpv_overlay'
  | 'queue'
  | 'search_input'
  | 'search_results'
  | 'library'
  | 'local_deck'
  | 'playlists'
  | 'settings'
  | 'ai_input'
  | 'ai_suggestions'
  | 'common'
  | 'global';

export const KEY_CONTEXTS: KeyContext[] = [
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
];

/** Human titles (the GUI stand-in for CONTEXT_META's localized names, src/keymap.rs). */
export const CONTEXT_LABELS: Record<KeyContext, string> = {
  player: 'Now Playing',
  now_playing: "What's playing card",
  mpv_overlay: 'mpv video overlay',
  queue: 'Queue',
  search_input: 'Search box',
  search_results: 'Search results',
  library: 'Library',
  local_deck: 'Local Deck',
  playlists: 'Playlists',
  settings: 'Settings',
  ai_input: 'DJ Gem — input',
  ai_suggestions: 'DJ Gem — suggestions',
  common: 'Common navigation & text editing',
  global: 'Global',
};

/** One catalog row — the generated wire model, with `context` narrowed to the known ids. */
export interface ActionInfo extends Omit<ActionInfoModel, 'context'> {
  context: KeyContext;
}

export interface KeymapModel {
  /** The persisted binding table: `"<context>.<action>" → "<chord>"` (src/keymap.rs form). */
  bindings: Record<string, string>;
  /** Every action's stable id, context, label, and default chord. */
  actions: ActionInfo[];
}

/** Inline conflict info surfaced from the core's bind reply (shadowing stays core-side). */
export interface KeymapConflict {
  /** `"<context>.<action>"` key of the binding we just tried to set. */
  key: string;
  chord: string;
  /** Display text for the binding the chord already resolves to (`"<ctx> — <label>"`). */
  shadows: string;
}

// ── the canonical GUI keymap seed (the demo core emits it; the real core replaces it) ──────
// Mirrors `default_bindings()` + ACTION_META / `human_label_for` in src/keymap.rs exactly
// (context ids, action ids, English labels, config-format chords) — update from that table,
// never invent rows. Runtime effect lives in lib/keyboard/actions for the ids with a GUI
// handler; the rest still render + rebind (they drive the shared TUI keymap).
type SeedRow = [KeyContext, string, string, string]; // context, id, label, default chord
const SEED_ROWS: SeedRow[] = [
  ['player', 'toggle_pause', 'Play / pause', 'space'],
  ['player', 'toggle_radio_mode', 'Radio/Normal mode', 'alt+shift+r'],
  ['player', 'toggle_recordings', 'Radio recordings', 'alt+shift+e'],
  ['player', 'seek_back', 'Seek backward', 'left'],
  ['player', 'seek_forward', 'Seek forward', 'right'],
  ['player', 'vol_up', 'Volume up', 'up'],
  ['player', 'vol_down', 'Volume down', 'down'],
  ['player', 'toggle_mute', 'Mute / unmute', 'm'],
  ['player', 'prev_track', 'Previous track', ','],
  ['player', 'next_track', 'Next track', '.'],
  ['player', 'cycle_rating', 'Rate: like / dislike', 'f'],
  ['player', 'open_library', 'Open library', 'l'],
  ['player', 'open_queue', 'Open queue', 'c'],
  ['player', 'queue_remove', 'Remove current from queue', 'delete'],
  ['player', 'toggle_lyrics', 'Toggle lyrics', 'L'],
  ['player', 'lyrics_delay_earlier', 'Lyrics earlier', 'z'],
  ['player', 'lyrics_delay_later', 'Lyrics later', 'Z'],
  ['player', 'download', 'Download track', 'd'],
  ['player', 'toggle_shuffle', 'Toggle shuffle', 'x'],
  ['player', 'cycle_repeat', 'Cycle repeat', 'r'],
  ['player', 'identify_now_playing', "What's playing (radio)", 'i'],
  ['player', 'cycle_eq', 'Cycle EQ preset', 'e'],
  ['player', 'toggle_normalize', 'Toggle normalization', 'N'],
  ['player', 'speed_up', 'Speed up', ']'],
  ['player', 'speed_down', 'Speed down', '['],
  ['player', 'open_settings', 'Open settings', 'o'],
  ['player', 'open_ai', 'Open DJ Gem assistant', 'g'],
  ['player', 'open_search', 'Open search', 's'],
  ['player', 'add_to_playlist', 'Add to playlist', 'P'],
  ['player', 'copy_link', 'Copy track link', 'y'],
  ['player', 'play_video', 'Video overlay (mpv)', 'v'],
  ['player', 'toggle_video_layout', 'Video size / position', 'V'],
  ['player', 'back', 'Back / close', 'q'],
  ['now_playing', 'now_playing_favorite', 'Save to music favorites', 'f'],
  ['now_playing', 'now_playing_ask_ai', 'Tell me more (DJ Gem)', 'g'],
  ['mpv_overlay', 'video_toggle_pause', 'Video play / pause', 'space'],
  ['mpv_overlay', 'video_next', 'Next video', '.'],
  ['mpv_overlay', 'video_prev', 'Previous video', ','],
  ['mpv_overlay', 'video_close', 'Close video', 'q'],
  ['mpv_overlay', 'video_toggle_fullscreen', 'Fullscreen', 'f'],
  ['mpv_overlay', 'video_toggle_mute', 'Mute / unmute', 'm'],
  ['common', 'move_up', 'Move up', 'up'],
  ['common', 'move_down', 'Move down', 'down'],
  ['common', 'page_up', 'Page up', 'pageup'],
  ['common', 'page_down', 'Page down', 'pagedown'],
  ['common', 'jump_top', 'Jump to top', 'home'],
  ['common', 'jump_bottom', 'Jump to bottom', 'end'],
  ['common', 'select_up', 'Extend selection up', 'shift+up'],
  ['common', 'select_down', 'Extend selection down', 'shift+down'],
  ['common', 'select_page_up', 'Extend selection a page up', 'shift+pageup'],
  ['common', 'select_page_down', 'Extend selection a page down', 'shift+pagedown'],
  ['common', 'select_to_top', 'Extend selection to top', 'shift+home'],
  ['common', 'select_to_bottom', 'Extend selection to bottom', 'shift+end'],
  ['common', 'confirm', 'Confirm / select', 'enter'],
  ['common', 'focus_prev', 'Previous tab / focus', 'backtab'],
  ['common', 'focus_next', 'Next tab / focus', 'tab'],
  ['common', 'delete_char', 'Delete character', 'backspace'],
  ['common', 'delete_word', 'Delete previous word (text inputs)', 'ctrl+backspace'],
  ['common', 'back', 'Back / close', 'q'],
  ['global', 'home', 'Go home', 'ctrl+h'],
  ['global', 'toggle_streaming', 'Toggle autoplay', 'ctrl+r'],
  ['global', 'toggle_help', 'Toggle help', '?'],
  ['global', 'open_context_menu', 'Open context menu', 'shift+f10'],
  ['global', 'toggle_about', 'About YuTuTui!', 'f1'],
  ['global', 'toggle_animations', 'Toggle animations', 'A'],
  ['global', 'toggle_control_box', 'Collapse / expand player bar', 'B'],
  ['global', 'why_ai', 'Why these DJ Gem picks', 'w'],
  ['global', 'text_zoom_in', 'Text size up', 'ctrl+='],
  ['global', 'text_zoom_out', 'Text size down', 'ctrl+-'],
  ['global', 'toggle_zoom_wheel_lock', 'Ctrl+wheel zoom lock', 'ctrl+l'],
  ['global', 'quit', 'Quit', 'ctrl+q'],
  ['library', 'confirm', 'Play selected', 'enter'],
  ['library', 'toggle_local_mode', 'Enter / exit Local Deck', 'alt+shift+l'],
  ['library', 'enqueue', 'Add to queue', '\\'],
  ['library', 'play_all', 'Play whole tab', 'a'],
  ['library', 'favorite', 'Favorite / unfavorite', 'f'],
  ['library', 'download', 'Download track', 'd'],
  ['library', 'download_all', 'Download whole list', 'D'],
  ['library', 'open_ai', 'Open DJ Gem assistant', 'g'],
  ['library', 'add_to_playlist', 'Add to playlist', 'p'],
  ['library', 'library_remove', 'Remove / delete', 'delete'],
  ['library', 'library_filter', 'Filter library', '/'],
  ['library', 'back', 'Close Library', 'q'],
  ['local_deck', 'accept_all_import_review', 'Accept all import candidates', 'A'],
  ['playlists', 'confirm', 'Open / play selected', 'enter'],
  ['playlists', 'play_all', 'Play playlist', 'a'],
  ['playlists', 'enqueue', 'Enqueue playlist / song', '\\'],
  ['playlists', 'playlist_create', 'New playlist', 'n'],
  ['playlists', 'favorite', 'Favorite / unfavorite', 'f'],
  ['playlists', 'download', 'Download track', 'd'],
  ['playlists', 'download_all', 'Download playlist', 'D'],
  ['playlists', 'open_ai', 'Open DJ Gem assistant', 'g'],
  ['playlists', 'add_to_playlist', 'Add to playlist', 'p'],
  ['playlists', 'library_remove', 'Delete playlist / remove song', 'delete'],
  ['playlists', 'library_filter', 'Filter library', '/'],
  ['playlists', 'back', 'Back / close', 'q'],
  ['queue', 'confirm', 'Play / jump to track', 'enter'],
  ['queue', 'queue_remove', 'Remove selected from queue', 'delete'],
  ['queue', 'back', 'Close queue', 'q'],
  ['search_input', 'select_all', 'Select all', 'ctrl+a'],
  ['search_input', 'toggle_search_source_menu', 'Open source menu', 'tab'],
  ['search_input', 'toggle_search_kind', 'Search songs / playlists', 'ctrl+p'],
  ['search_input', 'focus_prev', 'Focus search results', 'backtab'],
  ['search_results', 'focus_prev', 'Focus search box', 'backtab'],
  ['search_results', 'toggle_search_source_menu', 'Open source menu', 'tab'],
  ['search_results', 'toggle_search_kind', 'Search songs / playlists', 'ctrl+p'],
  ['search_results', 'enqueue', 'Add to queue', '\\'],
  ['search_results', 'favorite', 'Favorite / unfavorite', 'f'],
  ['search_results', 'download', 'Download track', 'd'],
  ['search_results', 'add_to_playlist', 'Add to playlist', 'p'],
  ['search_results', 'search_filter', 'Filter results (popup)', '/'],
  ['search_results', 'back', 'Close Search Results', 'q'],
  ['ai_input', 'select_all', 'Select all', 'ctrl+a'],
  ['settings', 'change_decrease', 'Decrease value', 'left'],
  ['settings', 'change_increase', 'Increase value', 'right'],
  ['settings', 'settings_cancel', 'Save + quit', 'q'],
];
const ACTIONS: ActionInfo[] = SEED_ROWS.map(([context, id, label, default_chord]) => ({
  context,
  id,
  label,
  default_chord,
}));

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
  #nextMutation = 0;
  readonly #pendingMutation = new Map<string, number>();
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('settings', (payload) => this.#onPush(payload as SettingsSnapshot));
    this.#client.onConn((info) => {
      if (info.state === 'offline') {
        this.#pending = {};
        this.#pendingMutation.clear();
        this.conflict = null;
      }
    });
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
   * specific-context → common → global (first binding wins), matching keymap.rs.
   */
  match(context: KeyContext, chord: string): string | null {
    const bindings = this.#merged();
    const order: KeyContext[] =
      context === 'common'
        ? ['common', 'global']
        : context === 'global'
          ? ['global']
          : [context, 'common', 'global'];
    for (const ctx of order) {
      for (const a of this.actions) {
        if (a.context === ctx && bindings[`${ctx}.${a.id}`] === chord) return a.id;
      }
    }
    return null;
  }

  /** Resolve the text editor action directly, ahead of a view-specific binding. */
  textEditMatch(chord: string): string | null {
    const bindings = this.#merged();
    return bindings['common.delete_word'] === chord ? 'delete_word' : null;
  }

  // ── mutations (Apply(Keymap(...))) ───────────────────────────────────────────────────

  /** Bind a chord (optimistic); the reply carries any core-side conflict for inline display. */
  async rebind(context: KeyContext, action: string, chord: string): Promise<void> {
    const key = `${context}.${action}`;
    const mutation = ++this.#nextMutation;
    this.#pendingMutation.set(key, mutation);
    this.#pending = { ...this.#pending, [key]: chord };
    this.conflict = null;
    const result = await this.#client.cmd<{ conflict?: { shadows?: string } }>('keymap_bind', {
      context,
      action,
      chord,
    });
    if (!result.ok) {
      this.#rejectPending(key, mutation);
      return;
    }
    if (this.#pendingMutation.get(key) !== mutation) return;
    const shadows = result.payload?.conflict?.shadows;
    if (shadows) {
      // The core does NOT apply a conflicted bind (and pushes nothing, since nothing
      // changed) — roll the optimistic chord back ourselves and explain why inline.
      this.conflict = { key, chord, shadows: String(shadows) };
      this.#rejectPending(key, mutation);
      return;
    }
    this.conflict = null;
    this.#pendingMutation.delete(key);
  }

  unbind(context: KeyContext, action: string): void {
    const key = `${context}.${action}`;
    const mutation = ++this.#nextMutation;
    this.#pendingMutation.set(key, mutation);
    this.#pending = { ...this.#pending, [key]: null };
    this.conflict = null;
    void this.#client.cmd('keymap_unbind', { context, action }).then((result) => {
      if (!result.ok) this.#rejectPending(key, mutation);
      else if (this.#pendingMutation.get(key) === mutation) this.#pendingMutation.delete(key);
    });
  }

  /** Per-row reset: rebind the action to its factory chord. */
  resetBinding(a: ActionInfo): void {
    void this.rebind(a.context, a.id, a.default_chord);
  }

  /** Restore every chord to its default (danger zone). The confirming push refills bindings. */
  resetAll(): void {
    this.#pending = {};
    this.#pendingMutation.clear();
    this.conflict = null;
    void this.#client.cmd('keymap_reset_all');
  }

  #rejectPending(key: string, mutation: number): void {
    if (this.#pendingMutation.get(key) !== mutation) return;
    const next = { ...this.#pending };
    delete next[key];
    this.#pending = next;
    this.#pendingMutation.delete(key);
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
    // Drop the overlay entries the authoritative model now agrees with. The wire's
    // unbound form is the EMPTY string (wire_bindings), so '' and a missing key both
    // agree with a pending unbind (null).
    const next: Record<string, string | null> = {};
    for (const [key, val] of Object.entries(this.#pending)) {
      const authoritative = km.bindings[key] || null;
      if (authoritative !== (val || null)) next[key] = val;
    }
    this.#pending = next;
  }
}
