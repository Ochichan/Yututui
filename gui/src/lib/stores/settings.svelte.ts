// Settings (docs/gui/05 §5.2, 07 §6–§10): the read model + a sparse pending-edit overlay +
// dirty tracking. The `settings` topic pushes the whole SettingsModelV8 on change; forms
// read `pending ?? model` per field so an optimistic edit survives until the authoritative
// push confirms it. On a push, pending entries the model now agrees with are cleared; the
// rest persist (a slow round-trip must not revert an in-progress edit).
//
// This is the M3 keystone: the store holds the full model so its wires read their own block
// without a second source of truth — `settings.animations` (stores/anim.svelte.ts reads
// `model.animations`), `settings.theme-editor` (stores/theme.svelte.ts reads `model.theme`),
// and `settings.hotkeys` (stores/keymap.svelte.ts reads `model.keymap`) are all live. The
// desktop bridge forwards `apply` once the core advertises `settings-v8`; until then the
// in-page demo core answers (gui/WIRING.md).

import type { EqModel } from '../../generated/protocol/EqModel';
import type { Repeat } from '../../generated/protocol/Repeat';
import type { SearchSource } from '../../generated/protocol/SearchSource';
import type { Client } from '../ipc/client';
import type { AnimationsModel } from './anim.svelte';
import type { ThemeModel } from './theme.svelte';
import type { KeymapModel } from './keymap.svelte';

// ── PROVISIONAL wire shapes — only the demo core speaks them today. Faithful to the model
// in docs/gui/02 §11.6; reconcile with the ts-rs `SettingsModelV8` + `SettingChangeV8`
// (grouped typed enums, §13.3) when the settings-v8 core lands. A few fields are flattened
// vs the spec so the two-level `group.field` overlay merges cleanly (noted inline). ────────

export interface PlaybackSettings {
  speed_tenths: number;
  seek_seconds: number;
  gapless: boolean;
  enqueue_next: boolean;
  autoplay_on_start: boolean;
  mouse_wheel_volume: boolean;
  media_controls: boolean;
  // Carried for completeness (§11.6) but transport-bound, not edited on the Settings screen.
  volume: number;
  shuffle: boolean;
  repeat: Repeat;
}

export interface StreamingSettings {
  ai_enabled: boolean;
  gemini_model: string;
  autoplay: boolean;
  /** 'focused' | 'balanced' | 'discovery'. */
  mode: string;
  /** Presence only — the key itself never crosses the wire (§16). */
  has_gemini_key: boolean;
}

export interface SearchSettings {
  default_source: SearchSource;
  // Per-source enables, flattened from §11.6's map so `search.<x>_enabled` overlays merge.
  soundcloud_enabled: boolean;
  audius_enabled: boolean;
  jamendo_enabled: boolean;
  internet_archive_enabled: boolean;
  radio_browser_enabled: boolean;
  audius_app_name: string | null;
  jamendo_client_id: string | null;
}

export interface UiSettings {
  /** 'en' | 'ko'. */
  language: string;
  mouse: boolean;
  album_art: boolean;
  romanized_titles: boolean;
}

export interface StorageSettings {
  download_dir: string | null;
  /** Path only, never contents (§16). */
  cookies_file: string | null;
  download_concurrency: number;
}

export interface AudioSettings {
  /** v1 supports only mpv. */
  backend: string;
  mpv_output: string | null;
  mpv_device: string | null;
  mpv_cache_forward: string;
  mpv_cache_back: string;
}

export interface SettingsModelV8 {
  rev: number;
  playback: PlaybackSettings;
  /** Normalize lives here (matches EqModel + the tab's `eq.normalize` read). */
  eq: EqModel;
  streaming: StreamingSettings;
  search: SearchSettings;
  ui: UiSettings;
  storage: StorageSettings;
  audio: AudioSettings;
  /** Live as of the settings.animations wire (mirrors the core's AnimationsConfig). */
  animations: AnimationsModel;
  /** Live as of the settings.theme-editor wire (the resolved 34 roles + preset/overrides). */
  theme: ThemeModel;
  /** Live as of the settings.hotkeys wire (the remappable binding table + ActionInfo). */
  keymap: KeymapModel;
  // The accounts block is added by its own wire — the demo core need not emit it yet.
}

export interface SettingsSnapshot {
  kind: 'settings_snapshot';
  model: SettingsModelV8;
}

export type SettingGroup =
  'playback' | 'eq' | 'streaming' | 'search' | 'ui' | 'storage' | 'audio' | 'animations' | 'theme';

/** The provisional uniform mutation. The real wire is the grouped `SettingChangeV8` (§13.3). */
export interface SettingChange {
  group: SettingGroup;
  field: string;
  value: unknown;
}

export class SettingsStore {
  /** Last authoritative push; null until the first `settings` snapshot. */
  model = $state<SettingsModelV8 | null>(null);
  /** Sparse optimistic overlay keyed `group.field`; reassigned immutably for reactivity. */
  #pending = $state<Record<string, unknown>>({});
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('settings', (payload) => this.#onPush(payload as SettingsSnapshot));
  }

  // ── merged per-group views (pending ?? model) ────────────────────────────────────────
  get playback(): PlaybackSettings | null {
    return this.model ? this.#merge('playback', this.model.playback) : null;
  }
  get eq(): EqModel | null {
    return this.model ? this.#merge('eq', this.model.eq) : null;
  }
  get streaming(): StreamingSettings | null {
    return this.model ? this.#merge('streaming', this.model.streaming) : null;
  }
  get search(): SearchSettings | null {
    return this.model ? this.#merge('search', this.model.search) : null;
  }
  get ui(): UiSettings | null {
    return this.model ? this.#merge('ui', this.model.ui) : null;
  }
  get storage(): StorageSettings | null {
    return this.model ? this.#merge('storage', this.model.storage) : null;
  }
  get audio(): AudioSettings | null {
    return this.model ? this.#merge('audio', this.model.audio) : null;
  }
  get animations(): AnimationsModel | null {
    return this.model ? this.#merge('animations', this.model.animations) : null;
  }

  /** True while any optimistic edit is still awaiting the confirming push. */
  get dirty(): boolean {
    return Object.keys(this.#pending).length > 0;
  }

  // ── mutations ────────────────────────────────────────────────────────────────────────

  /** Live-apply a field: overlay it optimistically, then send. Cleared by the next push. */
  apply(group: SettingGroup, field: string, value: unknown): void {
    this.#pending = { ...this.#pending, [`${group}.${field}`]: value };
    this.#client.cmd('apply', { change: { group, field, value } });
  }

  /** Write-only: the core stores the key; only `streaming.has_gemini_key` comes back. */
  setGeminiKey(key: string): void {
    this.#client.cmd('set_gemini_key', { key });
  }

  /** Clears cached romanizations; resolves to the count for a feedback toast. */
  async clearRomanizationCache(): Promise<number> {
    const res = await this.#client.req<{ cleared: number }>('clear_romanization_cache');
    return res?.cleared ?? 0;
  }

  /** Factory-reset every setting (danger zone). The confirming push refills the model. */
  resetAll(): void {
    this.#pending = {};
    this.#client.cmd('reset_all_settings');
  }

  // ── internals ────────────────────────────────────────────────────────────────────────

  #merge<T extends object>(group: SettingGroup, base: T): T {
    const prefix = `${group}.`;
    let out = base;
    for (const [key, val] of Object.entries(this.#pending)) {
      if (!key.startsWith(prefix)) continue;
      if (out === base) out = { ...base };
      (out as Record<string, unknown>)[key.slice(prefix.length)] = val;
    }
    return out;
  }

  #onPush(snap: SettingsSnapshot): void {
    if (snap?.kind !== 'settings_snapshot') return;
    this.model = snap.model;
    // Keep only the overlay entries the authoritative model still disagrees with.
    const next: Record<string, unknown> = {};
    for (const [key, val] of Object.entries(this.#pending)) {
      if (!sameValue(readPath(snap.model, key), val)) next[key] = val;
    }
    this.#pending = next;
  }
}

/** Read a `group.field` path off the model; undefined if any hop is missing. */
function readPath(model: SettingsModelV8, path: string): unknown {
  let cur: unknown = model;
  for (const part of path.split('.')) {
    if (cur == null || typeof cur !== 'object') return undefined;
    cur = (cur as Record<string, unknown>)[part];
  }
  return cur;
}

/** Value (not reference) equality — covers primitives and the 10-band EQ array. */
function sameValue(a: unknown, b: unknown): boolean {
  return a === b || JSON.stringify(a) === JSON.stringify(b);
}
