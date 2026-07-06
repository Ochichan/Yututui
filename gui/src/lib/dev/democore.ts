// The demo core — a stateful in-page stand-in for a real ytm-tui core, used whenever the
// app runs outside the Rust shell (`npm run dev`, plain browser, Playwright). It speaks
// the same envelopes as bridge.rs and actually *behaves*: transport commands mutate real
// state, snapshots push back, tracks end and auto-advance. This is the fixture vehicle
// docs/gui/05 §4.3 / 10-testing.md call for, grown a personality.
//
// Wiring agents: when you connect a new feature, teach it here too (answer the command,
// push the topic) so the browser demo keeps exercising the whole surface. Keep shapes
// identical to the generated protocol types — this file must never invent wire forms
// except the PROVISIONAL lyrics shape flagged in lyrics.svelte.ts.

import type { InEnvelope, OutEnvelope } from '../ipc/envelope';
import type { Transport } from '../ipc/transport';
import type { PlayerModel } from '../../generated/protocol/PlayerModel';
import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Repeat } from '../../generated/protocol/Repeat';
import type { LyricLine } from '../stores/lyrics.svelte';
import type { DownloadStatus } from '../stores/downloads.svelte';
import type { SettingGroup, SettingsModelV8 } from '../stores/settings.svelte';
import type { ThemeModel } from '../stores/theme.svelte';
import type { PlaylistDetail, PlaylistSummary } from '../stores/playlists.svelte';
import type { SpotifyPlaylist, TransferSpec, TransferState } from '../stores/transfer.svelte';
import type { AccountsSnapshot } from '../stores/accounts.svelte';
import type { WhyGem } from '../stores/whygem.svelte';
import { defaultAnimations } from '../stores/anim.svelte';
import { defaultKeymap, type KeyContext } from '../stores/keymap.svelte';

// ── the demo catalog (original fictional tracks — the cat is the brand, =^..^=) ──────

function track(
  id: string,
  title: string,
  artist: string,
  album: string | null,
  durationMs: number | null,
  extra: Partial<TrackModel> = {},
): TrackModel {
  return {
    video_id: id,
    title,
    artist,
    album,
    duration_ms: durationMs,
    source: 'youtube',
    is_local: false,
    downloaded: false,
    favorite: false,
    disliked: false,
    display_title: null,
    display_artist: null,
    artwork: null,
    watch_url: `https://music.youtube.com/watch?v=${id}`,
    is_live: false,
    ...extra,
  };
}

const CATALOG: TrackModel[] = [
  track('demo-001', 'Purrple Rain', 'The Whisker Quartet', 'Feline Grooves', 252_000, {
    favorite: true,
  }),
  track('demo-002', '네온 골목 고양이', '서울야행', '심야 산책', 227_000),
  track('demo-003', "Schrödinger's Setlist", 'Quantum Paws', 'Superposition EP', 198_000),
  track('demo-004', '새벽 4시, 코드가 안 잡혀요', '못갖춘마디', null, 263_000, {
    source: 'sound_cloud',
  }),
  track('demo-005', 'Meownlight Sonata (lo-fi rework)', 'DJ Churu', 'Naptime Tapes', 184_000, {
    downloaded: true,
  }),
  track('demo-006', 'Tailwind', 'Nimbus Nine', 'Cirrus', 216_000),
  track('demo-007', '수요일의 츄르', '골골송협동조합', '간식의 계보', 174_000, { favorite: true }),
  track('demo-008', 'Nine Lives, One Take', 'Alley Cat Analog', 'Rooftop Sessions', 241_000),
  track('demo-009', 'ON AIR · 밤샘 코딩 라디오', 'ytm-tui.fm', null, null, {
    source: 'radio_browser',
    watch_url: null,
  }),
  track('demo-010', 'Whiskers on Vinyl', 'The Catnip Files', 'B-Sides for Strays', 205_000),
];

const LYRICS: Record<string, LyricLine[]> = {
  'demo-001': [
    { ms: 1_000, text: 'Rain on the rooftop, paws on the sill' },
    { ms: 14_000, text: 'Purple umbrellas down on the hill' },
    { ms: 27_000, text: 'Everything echoes when the city goes still' },
    { ms: 41_000, text: '(purr section)' },
    { ms: 55_000, text: 'Purrple rain, purrple rain — nap through the thunder again' },
    { ms: 72_000, text: 'Purrple rain, purrple rain — dry paws by half past ten' },
    { ms: 96_000, text: 'Chasing the drops down the windowpane' },
    { ms: 110_000, text: 'Nine little heartbeats, one refrain' },
    { ms: 130_000, text: 'Purrple rain, purrple rain…' },
  ],
  'demo-002': [
    { ms: 2_000, text: '가로등 불빛 아래 젖은 골목' },
    { ms: 16_000, text: '네온 간판이 물웅덩이에 번져' },
    { ms: 30_000, text: '발소리 죽인 채 담장을 넘는 밤' },
    { ms: 45_000, text: '아무도 모르는 지름길로 가자' },
    { ms: 60_000, text: '야옹 — 이 도시는 우리 거야' },
    { ms: 78_000, text: '새벽까지, 골목 끝까지' },
  ],
};

// ── settings fixture (docs/gui/07 §6–§10) ───────────────────────────────────────────

type Bands = SettingsModelV8['eq']['bands'];

// Illustrative preset curves so the settings EQ bars actually move when the user switches
// presets (the real core recomputes these; the demo just needs to look alive).
const EQ_PRESETS: Record<string, Bands> = {
  flat: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  bass: [6, 5, 4, 2, 0, 0, 0, 0, 0, 0],
  vocal: [0, 0, 0, 2, 4, 4, 2, 0, 0, 0],
  rock: [4, 3, 0, -2, -1, 1, 3, 4, 3, 2],
};

// ── theme fixture (docs/gui/06 §1–3) ────────────────────────────────────────────────
//
// The real core resolves a preset + overrides to 34 hexes via `ThemeConfig::effective_hex`;
// the demo derives all 34 from a compact per-preset seed (this is fixture fidelity, not the
// real resolver). Seeds are recognizable palettes so switching presets visibly repaints.

interface ThemeSeed {
  bg: string;
  surface: string; // border-muted / raised-surface base
  text: string;
  textMuted: string;
  textSubtle: string;
  textInverse: string;
  borderPrimary: string;
  borderFocused: string;
  accent: string;
  accentAlt: string;
  success: string;
  warning: string;
  error: string;
  selFg: string;
  selBg: string;
}

// The 13 TUI presets (ThemePreset, src/theme.rs) — compact seeds, faithful hues.
const THEME_SEEDS: Record<string, ThemeSeed> = {
  // prettier-ignore
  Default: { bg: '#12141a', surface: '#2a2e3a', text: '#e6e8ee', textMuted: '#a0a6b4', textSubtle: '#6b7180', textInverse: '#0b0d12', borderPrimary: '#3a3f4d', borderFocused: '#5b8cff', accent: '#5b8cff', accentAlt: '#8f6bff', success: '#4ec98a', warning: '#e0b341', error: '#e5556b', selFg: '#ffffff', selBg: '#2f3646' },
  // prettier-ignore
  Retro: { bg: '#001100', surface: '#003300', text: '#33ff66', textMuted: '#22aa44', textSubtle: '#157a30', textInverse: '#001100', borderPrimary: '#0a5a1a', borderFocused: '#66ff99', accent: '#33ff66', accentAlt: '#99ff33', success: '#66ff66', warning: '#ffff33', error: '#ff5555', selFg: '#001100', selBg: '#33ff66' },
  // prettier-ignore
  Radio: { bg: '#1a1410', surface: '#3a2e22', text: '#f4e8d8', textMuted: '#b8a488', textSubtle: '#7a6a52', textInverse: '#14100a', borderPrimary: '#4a3c2c', borderFocused: '#ff9d3c', accent: '#ff9d3c', accentAlt: '#ffca6b', success: '#9ccf4e', warning: '#e0b341', error: '#e5556b', selFg: '#14100a', selBg: '#ff9d3c' },
  // prettier-ignore
  Midnight: { bg: '#0a0e1a', surface: '#1c2438', text: '#dce3f0', textMuted: '#8b96b0', textSubtle: '#566178', textInverse: '#0a0e1a', borderPrimary: '#2a3450', borderFocused: '#4d7cff', accent: '#4d7cff', accentAlt: '#7a5cff', success: '#4ec98a', warning: '#e0b341', error: '#e5556b', selFg: '#ffffff', selBg: '#22304e' },
  // prettier-ignore
  Light: { bg: '#f7f8fa', surface: '#e2e5ea', text: '#1a1d24', textMuted: '#565b68', textSubtle: '#8a909e', textInverse: '#ffffff', borderPrimary: '#cdd2db', borderFocused: '#2f6bff', accent: '#2f6bff', accentAlt: '#7a4dff', success: '#1f9d57', warning: '#b7791f', error: '#d13c50', selFg: '#ffffff', selBg: '#2f6bff' },
  // prettier-ignore
  'High Contrast': { bg: '#000000', surface: '#1a1a1a', text: '#ffffff', textMuted: '#d0d0d0', textSubtle: '#a0a0a0', textInverse: '#000000', borderPrimary: '#ffffff', borderFocused: '#ffff00', accent: '#ffff00', accentAlt: '#00ffff', success: '#00ff00', warning: '#ffaa00', error: '#ff0000', selFg: '#000000', selBg: '#ffff00' },
  // prettier-ignore
  'Terminal Green': { bg: '#0b1a0b', surface: '#163a16', text: '#b8ffb8', textMuted: '#6fd06f', textSubtle: '#3f8f3f', textInverse: '#0b1a0b', borderPrimary: '#245c24', borderFocused: '#4dff4d', accent: '#4dff4d', accentAlt: '#a6ff4d', success: '#4dff4d', warning: '#d0ff4d', error: '#ff6b6b', selFg: '#0b1a0b', selBg: '#4dff4d' },
  // prettier-ignore
  Gruvbox: { bg: '#282828', surface: '#3c3836', text: '#ebdbb2', textMuted: '#a89984', textSubtle: '#7c6f64', textInverse: '#282828', borderPrimary: '#504945', borderFocused: '#fabd2f', accent: '#fabd2f', accentAlt: '#fe8019', success: '#b8bb26', warning: '#fabd2f', error: '#fb4934', selFg: '#282828', selBg: '#d79921' },
  // prettier-ignore
  Nord: { bg: '#2e3440', surface: '#3b4252', text: '#eceff4', textMuted: '#d8dee9', textSubtle: '#7b88a1', textInverse: '#2e3440', borderPrimary: '#434c5e', borderFocused: '#88c0d0', accent: '#88c0d0', accentAlt: '#81a1c1', success: '#a3be8c', warning: '#ebcb8b', error: '#bf616a', selFg: '#2e3440', selBg: '#5e81ac' },
  // prettier-ignore
  Dracula: { bg: '#282a36', surface: '#383a4c', text: '#f8f8f2', textMuted: '#b8b9c4', textSubtle: '#6272a4', textInverse: '#282a36', borderPrimary: '#44475a', borderFocused: '#bd93f9', accent: '#bd93f9', accentAlt: '#ff79c6', success: '#50fa7b', warning: '#f1fa8c', error: '#ff5555', selFg: '#f8f8f2', selBg: '#44475a' },
  // prettier-ignore
  'Tokyo Night': { bg: '#1a1b26', surface: '#24283b', text: '#c0caf5', textMuted: '#9aa5ce', textSubtle: '#565f89', textInverse: '#1a1b26', borderPrimary: '#2f334d', borderFocused: '#7aa2f7', accent: '#7aa2f7', accentAlt: '#bb9af7', success: '#9ece6a', warning: '#e0af68', error: '#f7768e', selFg: '#c0caf5', selBg: '#33467c' },
  // prettier-ignore
  Solarized: { bg: '#002b36', surface: '#073642', text: '#eee8d5', textMuted: '#93a1a1', textSubtle: '#586e75', textInverse: '#002b36', borderPrimary: '#0a4a58', borderFocused: '#268bd2', accent: '#268bd2', accentAlt: '#2aa198', success: '#859900', warning: '#b58900', error: '#dc322f', selFg: '#eee8d5', selBg: '#094a58' },
  // prettier-ignore
  'Rosé Pine': { bg: '#191724', surface: '#26233a', text: '#e0def4', textMuted: '#908caa', textSubtle: '#6e6a86', textInverse: '#191724', borderPrimary: '#302d41', borderFocused: '#ebbcba', accent: '#ebbcba', accentAlt: '#c4a7e7', success: '#9ccfd8', warning: '#f6c177', error: '#eb6f92', selFg: '#191724', selBg: '#403d52' },
};

const PRESET_NAMES = Object.keys(THEME_SEEDS);

/** Derive the full 34-role palette from a preset seed (mirrors roles.ts ids exactly). */
function presetPalette(name: string): Record<string, string> {
  const s = THEME_SEEDS[name] ?? THEME_SEEDS.Default;
  return {
    background: s.bg,
    'text-primary': s.text,
    'text-muted': s.textMuted,
    'text-subtle': s.textSubtle,
    'text-inverse': s.textInverse,
    'border-primary': s.borderPrimary,
    'border-focused': s.borderFocused,
    'border-muted': s.surface,
    accent: s.accent,
    'accent-alt': s.accentAlt,
    success: s.success,
    warning: s.warning,
    error: s.error,
    'selection-fg': s.selFg,
    'selection-bg': s.selBg,
    'selection-inactive-fg': s.textMuted,
    'selection-inactive-bg': s.surface,
    'gauge-filled': s.accent,
    'gauge-empty': s.surface,
    'player-control': s.accent,
    'player-label': s.textMuted,
    'help-group': s.accentAlt,
    'help-key': s.accent,
    'help-action': s.text,
    'settings-group': s.accentAlt,
    'settings-label': s.textMuted,
    'settings-value': s.text,
    'settings-value-focused': s.accent,
    'ai-user': s.accent,
    'ai-assistant': s.text,
    'ai-error': s.error,
    'ai-thinking': s.accentAlt,
    'lyrics-current': s.accent,
    'lyrics-dim': s.textSubtle,
  };
}

/** The gallery preview — the strip colors + card bg/text, derived from the full palette. */
function themePreview(name: string): {
  name: string;
  label: string;
  swatch: Record<string, string>;
} {
  const p = presetPalette(name);
  return {
    name,
    label: name,
    swatch: {
      accent: p.accent,
      'accent-alt': p['accent-alt'],
      success: p.success,
      warning: p.warning,
      error: p.error,
      background: p.background,
      'text-primary': p['text-primary'],
    },
  };
}

function defaultTheme(): ThemeModel {
  return {
    preset: 'Default',
    roles: presetPalette('Default'),
    overrides: {},
    background_none: false,
    retro: false,
    presets: PRESET_NAMES.map(themePreview),
  };
}

function defaultSettings(): SettingsModelV8 {
  return {
    rev: 1,
    playback: {
      speed_tenths: 10,
      seek_seconds: 5,
      gapless: true,
      enqueue_next: false,
      autoplay_on_start: false,
      mouse_wheel_volume: true,
      media_controls: true,
      volume: 72,
      shuffle: false,
      repeat: 'off',
    },
    eq: { preset: 'flat', bands: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], normalize: false },
    streaming: {
      ai_enabled: false,
      gemini_model: 'gemini-2.5-flash',
      autoplay: false,
      mode: 'balanced',
      has_gemini_key: false,
    },
    search: {
      default_source: 'youtube',
      soundcloud_enabled: true,
      audius_enabled: true,
      jamendo_enabled: false,
      internet_archive_enabled: true,
      radio_browser_enabled: true,
      audius_app_name: 'ytm-tui',
      jamendo_client_id: null,
    },
    ui: { language: 'en', mouse: true, album_art: true, romanized_titles: false },
    storage: { download_dir: '~/Music/ytm-tui', cookies_file: null, download_concurrency: 3 },
    // Core defaults: every effect off, pause-unfocused on, 30 fps (the generic setter mutates
    // this block on `apply { group: 'animations' }`, like every other group).
    animations: defaultAnimations(),
    // Default preset, all 34 roles resolved, no overrides (settings.theme-editor).
    theme: defaultTheme(),
    // The remappable keymap read model — bindings + ActionInfo (settings.hotkeys).
    keymap: defaultKeymap(),
  };
}

// ── the core ─────────────────────────────────────────────────────────────────────────

export class DemoCoreTransport implements Transport {
  readonly live = false;
  #cb: ((env: InEnvelope) => void) | null = null;

  #queue: TrackModel[] = CATALOG.map((t) => ({ ...t }));
  #pos = 0;
  #paused = false;
  #volume = 72;
  #speedTenths = 10;
  #shuffle = false;
  #repeat: Repeat = 'off';
  #rev = 1;
  #epoch = 1;
  #elapsedMs = 38_000;
  #anchor = performance.now();
  #endTimer: ReturnType<typeof setTimeout> | null = null;
  // Every track the last search surfaced, keyed by its (source-prefixed) video_id, so
  // play_tracks / enqueue_tracks can resolve an id the user clicked back to a real track.
  #searchIndex = new Map<string, TrackModel>();
  // Fake radio stations for the radio_likes / radio_history library scopes.
  #stations: TrackModel[] = [
    track('radio-1', 'ON AIR · 밤샘 코딩 라디오', 'ytm-tui.fm', null, null, {
      source: 'radio_browser',
      watch_url: null,
    }),
    track('radio-2', 'Lofi Alley — 24/7', 'Stray Signal', null, null, {
      source: 'radio_browser',
      watch_url: null,
    }),
    track('radio-3', 'Jazz for Naps', 'Catnip FM', null, null, {
      source: 'radio_browser',
      watch_url: null,
    }),
  ];
  // `<scope>:<video_id>` entries the user removed via library_remove.
  #removed = new Set<string>();
  // DJ Gem transcript (role + text), grown by ask_ai.
  #aiMessages: Array<{ role: 'user' | 'assistant'; text: string }> = [];
  #aiTimer: ReturnType<typeof setTimeout> | null = null;
  // Why-DJ-Gem explanations by video_id (ai.whygem) — the "autoplay-added" queue rows.
  // Seeded with two picks; enqueuing a DJ-Gem suggestion adds more. PROVISIONAL, demo-only.
  #whyGem = new Map<string, WhyGem>([
    [
      'demo-006',
      {
        slot: 'More like this',
        reasons: [
          'Stays in the airy synth-pop lane you were on',
          'High skip-survival across similar late-night sessions',
        ],
        confidence: 0.86,
      },
    ],
    [
      'demo-008',
      {
        slot: 'Deep cut',
        reasons: ['Same label family as your last ♥', 'Tempo matches the current run'],
        confidence: 0.71,
      },
    ],
  ]);
  // The video_ids of the most recent DJ-Gem suggestion set, so enqueuing one marks it a pick.
  #lastAiSuggestions = new Set<string>();
  // Tracked downloads by video_id (the `downloads` topic snapshot).
  #downloads: DownloadStatus[] = [];
  // Music ⇄ Radio mode, flipped by set_setting { radio_mode }.
  #radioMode = false;
  // The `settings` topic read model (docs/gui/07 §6–§10), mutated by `apply`.
  #settings: SettingsModelV8 = defaultSettings();
  // Fake romanization cache size, drained by clear_romanization_cache.
  #romanizationCache = 12;
  // Local playlists (the `playlists` topic) — summaries + their track lists by id.
  #playlists: PlaylistSummary[] = [
    { id: 'pl-1', name: 'Late-night coding', count: 3, description: 'ship it =^..^=' },
    { id: 'pl-2', name: 'Favorites', count: 2, description: null },
  ];
  #playlistTracks = new Map<string, TrackModel[]>([
    ['pl-1', [CATALOG[3], CATALOG[5], CATALOG[7]].map((t) => ({ ...t }))],
    ['pl-2', CATALOG.filter((t) => t.favorite).map((t) => ({ ...t }))],
  ]);
  #nextPlaylist = 3;
  // Spotify import wizard state (the `transfer` topic).
  #transfer: TransferState = {
    kind: 'transfer_state',
    phase: 'idle',
    sources: [],
    job: null,
    report: null,
    error: null,
  };
  #transferTimers: Array<ReturnType<typeof setTimeout>> = [];
  // Account connection state (the `accounts` topic).
  #accounts: AccountsSnapshot = {
    kind: 'accounts_snapshot',
    lastfm: { connected: false, user: null, scrobbling: false, love_sync: false },
    listenbrainz: { submit: false, has_token: false, custom_url: null },
    spotify: { connected: false, user: null, client_id: null, redirect_port: null },
    scrobble_local: false,
  };

  onMessage(cb: (env: InEnvelope) => void): void {
    this.#cb = cb;
    setTimeout(() => this.#emit({ v: 1, kind: 'conn', payload: { state: 'connecting' } }), 0);
    setTimeout(() => {
      this.#emit({
        v: 1,
        kind: 'conn',
        payload: {
          state: 'online',
          coreVersion: 'demo-core',
          protocolVersion: 8,
          capabilities: ['events-v8'],
          ownerMode: 'daemon',
        },
      });
    }, 120);
  }

  send(env: OutEnvelope): void {
    // Async like the real bridge, so callers never observe a synchronous reply.
    setTimeout(() => this.#handle(env), 10);
  }

  #handle(env: OutEnvelope): void {
    switch (env.kind) {
      case 'req':
        if (env.name === 'ping') {
          this.#emit({ v: 1, id: env.id, kind: 'res', payload: 'pong (demo)' });
        } else if (env.name === 'clear_romanization_cache') {
          const cleared = this.#romanizationCache;
          this.#romanizationCache = 0;
          this.#emit({ v: 1, id: env.id, kind: 'res', payload: { cleared } });
        } else if (env.name === 'fetch_library_page') {
          const p = (env.payload ?? {}) as Record<string, unknown>;
          this.#emit({
            v: 1,
            id: env.id,
            kind: 'res',
            payload: this.#libraryPage(
              String(p.scope ?? 'all'),
              String(p.filter ?? ''),
              Number(p.offset ?? 0),
              Number(p.limit ?? 50),
            ),
          });
        } else if (env.name === 'fetch_playlist_detail') {
          const p = (env.payload ?? {}) as Record<string, unknown>;
          this.#emit({
            v: 1,
            id: env.id,
            kind: 'res',
            payload: this.#playlistDetail(String(p.playlist_id ?? '')),
          });
        } else if (env.name === 'keymap_bind') {
          const p = (env.payload ?? {}) as Record<string, unknown>;
          const conflict = this.#keymapBind(
            String(p.context ?? '') as KeyContext,
            String(p.action ?? ''),
            String(p.chord ?? ''),
          );
          // Conflict detection stays core-side; the reply carries it for inline display.
          this.#emit({ v: 1, id: env.id, kind: 'res', payload: { ok: true, conflict } });
        } else if (env.name === 'fetch_why_gem') {
          const p = (env.payload ?? {}) as Record<string, unknown>;
          this.#emit({
            v: 1,
            id: env.id,
            kind: 'res',
            payload: this.#whyGem.get(String(p.video_id ?? '')) ?? null,
          });
        } else {
          this.#emit({ v: 1, id: env.id, kind: 'err', payload: { reason: 'not_supported' } });
        }
        return;
      case 'sub': {
        // Initial-snapshot-on-subscribe, like the real session wire (docs/gui/02 §8).
        const topics = Array.isArray(env.payload) ? (env.payload as string[]) : [];
        if (topics.includes('player')) this.#pushPlayer();
        if (topics.includes('queue')) this.#pushQueue();
        if (topics.includes('lyrics')) this.#pushLyrics();
        if (topics.includes('ai')) this.#pushWhyGem();
        if (topics.includes('settings')) this.#pushSettings();
        if (topics.includes('playlists')) this.#pushPlaylists();
        if (topics.includes('accounts')) this.#pushAccounts();
        return;
      }
      case 'cmd':
        this.#command(env.name, (env.payload ?? {}) as Record<string, unknown>);
        return;
      case 'unsub':
      case 'win':
        return; // window ops are the shell's business; nothing to demo
    }
  }

  #command(name: string, p: Record<string, unknown>): void {
    switch (name) {
      case 'toggle_pause':
        this.#samplePosition();
        this.#paused = !this.#paused;
        break;
      case 'next':
        this.#advance(1, true);
        break;
      case 'prev':
        this.#advance(-1, true);
        break;
      case 'seek_to':
        this.#seekTo(Number(p.ms ?? 0));
        break;
      case 'seek_back':
        this.#seekTo((this.#position() ?? 0) - 5_000);
        break;
      case 'seek_forward':
        this.#seekTo((this.#position() ?? 0) + 5_000);
        break;
      case 'set_volume':
        this.#volume = Math.max(0, Math.min(100, Number(p.percent ?? this.#volume)));
        break;
      case 'toggle_shuffle':
        this.#shuffle = !this.#shuffle;
        break;
      case 'cycle_repeat':
        this.#repeat = this.#repeat === 'off' ? 'all' : this.#repeat === 'all' ? 'one' : 'off';
        break;
      case 'queue_play': {
        const pos = Number(p.position ?? 0);
        if (pos >= 0 && pos < this.#queue.length) this.#jumpTo(pos);
        break;
      }
      case 'queue_remove':
      case 'queue_remove_many': {
        const positions =
          name === 'queue_remove' ? [Number(p.position ?? -1)] : ((p.positions as number[]) ?? []);
        this.#removePositions(positions);
        break;
      }
      case 'queue_move':
        this.#moveQueue(Number(p.from ?? -1), Number(p.to ?? -1));
        break;
      case 'queue_clear_upcoming':
        this.#queue = this.#queue.slice(0, this.#pos + 1);
        this.#rev++;
        this.#pushQueue();
        break;
      case 'rate':
        this.#rate(String(p.video_id ?? ''));
        break;
      case 'run_search':
        // Its own topic push, no playback change — return before the trailing player push.
        this.#runSearch(
          Number(p.ticket ?? 0),
          String(p.query ?? ''),
          String(p.source ?? 'youtube'),
        );
        return;
      case 'play_tracks':
      case 'enqueue_tracks':
        this.#addTracks((p.video_ids as string[] | undefined) ?? [], name === 'play_tracks');
        break;
      case 'library_play':
      case 'library_enqueue': {
        const list = this.#libraryPage(
          String(p.scope ?? 'all'),
          String(p.filter ?? ''),
          0,
          9999,
        ).tracks;
        if (list.length === 0) return;
        if (name === 'library_play') {
          this.#queue = list.map((t) => ({ ...t }));
          this.#rev++;
          this.#pushQueue();
          this.#jumpTo(0);
        } else {
          this.#queue.push(...list.map((t) => ({ ...t })));
          this.#rev++;
          this.#pushQueue();
        }
        break;
      }
      case 'library_remove':
        this.#removed.add(`${String(p.scope ?? 'all')}:${String(p.video_id ?? '')}`);
        this.#pushLibrary(); // invalidate → the store re-fetches the shrunken scope
        return;
      case 'ask_ai':
        this.#askAi(String(p.prompt ?? ''));
        return;
      case 'set_setting': {
        const change = (p.change ?? {}) as Record<string, unknown>;
        if (change.setting === 'radio_mode') {
          const st = String(change.state ?? 'toggle');
          this.#radioMode = st === 'on' ? true : st === 'off' ? false : !this.#radioMode;
        }
        break; // trailing pushPlayer reflects radio_mode
      }
      case 'apply': {
        const change = (p.change ?? {}) as Record<string, unknown>;
        this.#applySetting(
          String(change.group ?? '') as SettingGroup,
          String(change.field ?? ''),
          change.value,
        );
        return;
      }
      case 'theme_set_override':
        this.#themeSetOverride(String(p.role ?? ''), String(p.hex ?? ''));
        return;
      case 'theme_clear_override':
        this.#themeClearOverride(String(p.role ?? ''));
        return;
      case 'keymap_unbind':
        this.#keymapUnbind(String(p.context ?? '') as KeyContext, String(p.action ?? ''));
        return;
      case 'keymap_reset_all':
        this.#settings.keymap = defaultKeymap();
        this.#settings.rev++;
        this.#pushSettings();
        return;
      case 'set_gemini_key':
        // Write-only: the key never round-trips; only presence flips.
        this.#settings.streaming.has_gemini_key = String(p.key ?? '').length > 0;
        this.#settings.rev++;
        this.#pushSettings();
        return;
      case 'reset_all_settings':
        this.#settings = defaultSettings();
        this.#settings.rev++;
        this.#pushSettings();
        return;
      case 'download':
        this.#download(String(p.video_id ?? ''), String(p.title ?? ''));
        return;
      case 'delete_download':
        this.#downloads = this.#downloads.filter((d) => d.video_id !== String(p.video_id ?? ''));
        this.#pushDownloads();
        return;
      case 'playlist_create':
        this.#playlistCreate(String(p.name ?? ''));
        return;
      case 'playlist_delete':
        this.#playlistDelete(String(p.playlist_id ?? ''));
        return;
      case 'playlist_add_tracks':
        this.#playlistAddTracks(String(p.playlist_id ?? ''), (p.video_ids as string[]) ?? []);
        return;
      case 'playlist_remove_track':
        this.#playlistRemoveTrack(String(p.playlist_id ?? ''), String(p.video_id ?? ''));
        return;
      case 'playlist_play':
        this.#playlistPlay(String(p.playlist_id ?? ''));
        return;
      case 'transfer_list_spotify':
        this.#transferList();
        return;
      case 'transfer_start':
        this.#transferStart((p.spec ?? {}) as TransferSpec);
        return;
      case 'transfer_cancel':
        this.#transferCancel();
        return;
      case 'lastfm_connect':
        this.#accountConnect('lastfm');
        return;
      case 'spotify_connect':
        this.#accountConnect('spotify');
        return;
      case 'listen_brainz_configure':
        this.#listenBrainzConfigure(p);
        return;
      case 'account_set':
        this.#accountSet(String(p.service ?? ''), String(p.field ?? ''), p.value);
        return;
      default:
        // Unknown command: a real core would reject reason-coded; the demo just ignores.
        return;
    }
    this.#pushPlayer();
  }

  // ── playback state machine ─────────────────────────────────────────────────────────

  #current(): TrackModel | null {
    return this.#queue[this.#pos] ?? null;
  }

  #position(): number | null {
    const t = this.#current();
    if (!t) return null;
    if (this.#paused) return this.#elapsedMs;
    const pos = this.#elapsedMs + (performance.now() - this.#anchor) * (this.#speedTenths / 10);
    return t.duration_ms == null ? pos : Math.min(pos, t.duration_ms);
  }

  #samplePosition(): void {
    this.#elapsedMs = this.#position() ?? 0;
    this.#anchor = performance.now();
  }

  #seekTo(ms: number): void {
    const t = this.#current();
    if (!t) return;
    const max = t.duration_ms ?? Number.MAX_SAFE_INTEGER;
    this.#elapsedMs = Math.max(0, Math.min(ms, max));
    this.#anchor = performance.now();
    this.#epoch++;
  }

  #jumpTo(pos: number): void {
    this.#pos = pos;
    this.#elapsedMs = 0;
    this.#anchor = performance.now();
    this.#paused = false;
    this.#epoch++;
    this.#pushLyrics();
  }

  #advance(dir: 1 | -1, manual: boolean): void {
    let next = this.#pos + dir;
    if (this.#repeat === 'all' || (manual && this.#repeat === 'one')) {
      next = (next + this.#queue.length) % this.#queue.length;
    }
    if (!manual && this.#repeat === 'one') next = this.#pos;
    if (next < 0) next = 0;
    if (next >= this.#queue.length) {
      // End of queue, repeat off: park paused at the last track, like the player would.
      this.#pos = this.#queue.length - 1;
      this.#samplePosition();
      this.#paused = true;
      return;
    }
    this.#jumpTo(next);
  }

  #removePositions(positions: number[]): void {
    const drop = new Set(positions.filter((n) => n >= 0 && n < this.#queue.length));
    if (drop.size === 0) return;
    const wasCurrentDropped = drop.has(this.#pos);
    this.#queue = this.#queue.filter((_, i) => !drop.has(i));
    this.#pos -= [...drop].filter((i) => i < this.#pos).length;
    if (this.#queue.length === 0) {
      this.#pos = 0;
      this.#paused = true;
    } else if (wasCurrentDropped) {
      this.#jumpTo(Math.min(this.#pos, this.#queue.length - 1));
    }
    this.#rev++;
    this.#pushQueue();
  }

  /** Drag-reorder (queue_move): splice `from`→`to`, keeping the cursor on the same track. */
  #moveQueue(from: number, to: number): void {
    const n = this.#queue.length;
    if (from < 0 || from >= n || to < 0 || to >= n || from === to) return;
    const currentTrack = this.#queue[this.#pos] ?? null; // reference-tracked (dupe-safe)
    const [moved] = this.#queue.splice(from, 1);
    this.#queue.splice(to, 0, moved);
    if (currentTrack) {
      const idx = this.#queue.indexOf(currentTrack);
      if (idx >= 0) this.#pos = idx;
    }
    this.#rev++;
    this.#pushQueue();
    this.#pushPlayer(); // queue_pos may have shifted
  }

  #rate(videoId: string): void {
    const t = this.#queue.find((x) => x.video_id === videoId);
    if (!t) return;
    if (!t.favorite && !t.disliked) t.favorite = true;
    else if (t.favorite) {
      t.favorite = false;
      t.disliked = true;
    } else t.disliked = false;
    this.#rev++;
    this.#pushQueue();
  }

  // ── search ───────────────────────────────────────────────────────────────────────────

  #runSearch(ticket: number, query: string, source: string): void {
    const q = query.trim().toLowerCase();
    const hits = CATALOG.filter(
      (t) => t.title.toLowerCase().includes(q) || t.artist.toLowerCase().includes(q),
    );
    // Always surface *something* so the results UI is exercised even for a gibberish query.
    const pool = hits.length ? hits : this.#generated(query);
    const catalogs = [
      'youtube',
      'sound_cloud',
      'audius',
      'jamendo',
      'internet_archive',
      'radio_browser',
    ];
    const wanted = source === 'all' ? catalogs : [source];

    this.#searchIndex.clear();
    const groups = wanted.map((src) => {
      // Jamendo stands in for the per-source error chip (registry note: "no client id").
      if (src === 'jamendo') {
        return { source: src, tracks: [], error: 'no client id — set one in Settings → General' };
      }
      const tracks = pool.map((t) => ({
        ...t,
        source: src as TrackModel['source'],
        video_id: `${src}:${t.video_id}`,
      }));
      for (const t of tracks) this.#searchIndex.set(t.video_id, t);
      return { source: src, tracks, error: null };
    });

    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'search',
      // PROVISIONAL shape — see SearchCompleted in stores/search.svelte.ts.
      payload: { kind: 'search_completed', ticket, query, source, groups },
    });
  }

  #generated(query: string): TrackModel[] {
    const stub = query.trim() || 'meow';
    return [1, 2, 3].map((n) =>
      track(
        `gen-${n}`,
        `${stub} — take ${n}`,
        'The Search Cats',
        'Live Results',
        120_000 + n * 24_000,
      ),
    );
  }

  /** play_tracks / enqueue_tracks: resolve ids to real tracks, then splice or append. */
  #addTracks(videoIds: string[], playNow: boolean): void {
    const resolve = (id: string) =>
      this.#searchIndex.get(id) ??
      this.#queue.find((t) => t.video_id === id) ??
      CATALOG.find((t) => t.video_id === id) ??
      this.#stations.find((t) => t.video_id === id);
    const picked = videoIds
      .map(resolve)
      .filter((t): t is TrackModel => t != null)
      .map((t) => ({ ...t }));
    if (picked.length === 0) return;
    // A track that came from the latest DJ-Gem suggestions becomes an autoplay pick with a
    // "why?" explanation (ai.whygem provenance).
    let gemAdded = false;
    for (const t of picked) {
      if (this.#lastAiSuggestions.has(t.video_id) && !this.#whyGem.has(t.video_id)) {
        this.#whyGem.set(t.video_id, this.#gemFor(t));
        gemAdded = true;
      }
    }
    if (gemAdded) this.#pushWhyGem();
    if (playNow) {
      const at = this.#queue.length ? this.#pos + 1 : 0;
      this.#queue.splice(at, 0, ...picked);
      this.#rev++;
      this.#pushQueue();
      this.#jumpTo(at); // start the first inserted track now
    } else {
      this.#queue.push(...picked);
      this.#rev++;
      this.#pushQueue();
    }
  }

  // ── library ──────────────────────────────────────────────────────────────────────────

  /** The full track list for a scope, minus anything library_remove dropped. */
  #libraryScope(scope: string): TrackModel[] {
    const base =
      scope === 'favorites'
        ? CATALOG.filter((t) => t.favorite)
        : scope === 'history'
          ? [...CATALOG].reverse()
          : scope === 'radio_likes'
            ? this.#stations
            : scope === 'radio_history'
              ? [...this.#stations].reverse()
              : CATALOG; // 'all'
    return base.filter((t) => !this.#removed.has(`${scope}:${t.video_id}`));
  }

  /** A filtered, windowed page — the `fetch_library_page` response body. */
  #libraryPage(scope: string, filter: string, offset: number, limit: number) {
    const q = filter.trim().toLowerCase();
    const all = this.#libraryScope(scope).filter(
      (t) => !q || t.title.toLowerCase().includes(q) || t.artist.toLowerCase().includes(q),
    );
    return {
      scope,
      filter,
      offset,
      total: all.length,
      tracks: all.slice(offset, offset + limit).map((t) => ({ ...t })),
    };
  }

  #pushLibrary(): void {
    // PROVISIONAL invalidation — the store just re-fetches; see LibraryStore.
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'library',
      payload: { kind: 'library_invalidated' },
    });
  }

  // ── DJ Gem ───────────────────────────────────────────────────────────────────────────

  #askAi(prompt: string): void {
    this.#aiMessages = [...this.#aiMessages, { role: 'user', text: prompt }];
    this.#pushAi(true, []); // thinking, no suggestions yet
    if (this.#aiTimer) clearTimeout(this.#aiTimer);
    this.#aiTimer = setTimeout(() => {
      const picks = this.#djPicks(prompt);
      const names = picks.map((t) => t.title).join(', ');
      this.#aiMessages = [
        ...this.#aiMessages,
        {
          role: 'assistant',
          text: `For "${prompt}" try ${names}. Tap one to play — say the word for more. =^..^=`,
        },
      ];
      this.#pushAi(false, picks);
    }, 400);
  }

  /** Deterministic picks: catalog rows matching a word in the prompt, else the first few. */
  #djPicks(prompt: string): TrackModel[] {
    const q = prompt.trim().toLowerCase();
    const hits = CATALOG.filter(
      (t) => t.title.toLowerCase().includes(q) || t.artist.toLowerCase().includes(q),
    );
    return (hits.length ? hits : CATALOG).slice(0, 3).map((t) => ({ ...t }));
  }

  #pushAi(thinking: boolean, suggestions: TrackModel[]): void {
    if (suggestions.length) this.#lastAiSuggestions = new Set(suggestions.map((t) => t.video_id));
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'ai',
      // PROVISIONAL shape — see AiState in stores/ai.svelte.ts.
      payload: { kind: 'ai_state', messages: this.#aiMessages, thinking, suggestions },
    });
  }

  /** A generated explanation for a DJ-Gem-suggested track the user just enqueued. */
  #gemFor(t: TrackModel): WhyGem {
    return {
      slot: 'From your DJ Gem chat',
      reasons: [`Picked up on your ask around ${t.artist}`, 'Available to stream on this source'],
      confidence: 0.8,
    };
  }

  /** The autoplay-provenance sibling event on the `ai` topic (ai.whygem). PROVISIONAL. */
  #pushWhyGem(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'ai',
      payload: { kind: 'why_gem_provenance', video_ids: [...this.#whyGem.keys()] },
    });
  }

  // ── downloads ────────────────────────────────────────────────────────────────────────

  #download(videoId: string, title: string): void {
    if (!videoId) return;
    const src =
      CATALOG.find((t) => t.video_id === videoId) ??
      this.#stations.find((t) => t.video_id === videoId);
    // Upsert as running-at-0 (a re-download of a failed item restarts it).
    this.#downloads = [
      ...this.#downloads.filter((d) => d.video_id !== videoId),
      { video_id: videoId, title, state: 'running', pct: 0, error: null },
    ];
    this.#pushDownloads();

    // A live stream (no duration) can't be downloaded — fail it, like the real core.
    if (src && src.duration_ms == null) {
      setTimeout(
        () =>
          this.#setDownload(videoId, {
            state: 'failed',
            pct: 0,
            error: "can't grab a live stream",
          }),
        200,
      );
      return;
    }
    [25, 50, 75].forEach((pct, i) =>
      setTimeout(
        () => this.#setDownload(videoId, { state: 'running', pct, error: null }),
        150 * (i + 1),
      ),
    );
    setTimeout(() => this.#setDownload(videoId, { state: 'done', pct: 100, error: null }), 600);
  }

  #setDownload(videoId: string, patch: Partial<DownloadStatus>): void {
    const at = this.#downloads.findIndex((d) => d.video_id === videoId);
    if (at < 0) return; // deleted mid-flight — drop the stale progress tick
    this.#downloads = this.#downloads.map((d, i) => (i === at ? { ...d, ...patch } : d));
    this.#pushDownloads();
  }

  #pushDownloads(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'downloads',
      // PROVISIONAL shape — see DownloadsSnapshot in stores/downloads.svelte.ts.
      payload: { kind: 'downloads_snapshot', items: this.#downloads },
    });
  }

  // ── settings ─────────────────────────────────────────────────────────────────────────

  /** Uniform provisional mutation: set model[group][field], then push. Cf. §13.3. */
  #applySetting(group: SettingGroup, field: string, value: unknown): void {
    const block = (this.#settings as unknown as Record<string, Record<string, unknown>>)[group];
    if (!block || typeof block !== 'object') return;
    block[field] = value;
    // EQ preset and manual band edits keep each other honest, like the real af chain.
    if (group === 'eq' && field === 'preset') {
      this.#settings.eq.bands = EQ_PRESETS[String(value)] ?? this.#settings.eq.bands;
    } else if (group === 'eq' && field === 'bands') {
      this.#settings.eq.preset = 'custom';
    } else if (group === 'theme' && field === 'preset') {
      // Re-resolve the 34 roles for the new preset, keeping the user's overrides on top —
      // the demo stand-in for ThemeConfig::effective_hex(preset, overrides).
      this.#settings.theme.roles = {
        ...presetPalette(String(value)),
        ...this.#settings.theme.overrides,
      };
    }
    this.#settings.rev++;
    this.#pushSettings();
  }

  /** Per-role override: hold it and bake it into the resolved roles (settings.theme-editor). */
  #themeSetOverride(role: string, hex: string): void {
    if (!role) return;
    this.#settings.theme.overrides[role] = hex;
    this.#settings.theme.roles[role] = hex;
    this.#settings.rev++;
    this.#pushSettings();
  }

  /** Drop an override → the role reverts to the current preset's resolved value. */
  #themeClearOverride(role: string): void {
    if (!(role in this.#settings.theme.overrides)) return;
    delete this.#settings.theme.overrides[role];
    this.#settings.theme.roles[role] = presetPalette(this.#settings.theme.preset)[role];
    this.#settings.rev++;
    this.#pushSettings();
  }

  // ── keymap (settings.hotkeys) ────────────────────────────────────────────────────────

  /** Bind a chord; returns any core-side shadow conflict (detection stays here, not the GUI). */
  #keymapBind(context: KeyContext, action: string, chord: string): { shadows: string } | null {
    if (!context || !action || !chord) return null;
    const conflict = this.#keymapConflict(context, action, chord);
    this.#settings.keymap.bindings[`${context}.${action}`] = chord;
    this.#settings.rev++;
    this.#pushSettings();
    return conflict;
  }

  /** The first other action whose chord this bind collides with, across the effective contexts. */
  #keymapConflict(context: KeyContext, action: string, chord: string): { shadows: string } | null {
    const order: KeyContext[] =
      context === 'Common'
        ? ['Common', 'Global']
        : context === 'Global'
          ? ['Global']
          : [context, 'Common', 'Global'];
    const self = `${context}.${action}`;
    for (const ctx of order) {
      for (const a of this.#settings.keymap.actions) {
        if (a.context !== ctx) continue;
        const key = `${ctx}.${a.id}`;
        if (key === self) continue;
        if (this.#settings.keymap.bindings[key] === chord) return { shadows: a.id };
      }
    }
    return null;
  }

  #keymapUnbind(context: KeyContext, action: string): void {
    if (!context || !action) return;
    delete this.#settings.keymap.bindings[`${context}.${action}`];
    this.#settings.rev++;
    this.#pushSettings();
  }

  #pushSettings(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'settings',
      // PROVISIONAL shape — see SettingsSnapshot in stores/settings.svelte.ts. A fresh clone
      // per push so the store's authoritative model never aliases the demo's mutable state.
      payload: {
        kind: 'settings_snapshot',
        model: JSON.parse(JSON.stringify(this.#settings)) as SettingsModelV8,
      },
    });
  }

  // ── playlists (library.playlists) ─────────────────────────────────────────────────────

  /** The fetch_playlist_detail response body, or null if the id is gone. */
  #playlistDetail(id: string): PlaylistDetail | null {
    const summary = this.#playlists.find((p) => p.id === id);
    if (!summary) return null;
    return {
      id,
      name: summary.name,
      description: summary.description,
      tracks: (this.#playlistTracks.get(id) ?? []).map((t) => ({ ...t })),
    };
  }

  /** Keep a summary's count in step with its track list. */
  #reconcileCount(id: string): void {
    const summary = this.#playlists.find((p) => p.id === id);
    if (summary) summary.count = this.#playlistTracks.get(id)?.length ?? 0;
  }

  #playlistCreate(name: string): void {
    const n = name.trim();
    if (!n) return;
    const id = `pl-${this.#nextPlaylist++}`;
    this.#playlists = [...this.#playlists, { id, name: n, count: 0, description: null }];
    this.#playlistTracks.set(id, []);
    this.#pushPlaylists();
  }

  #playlistDelete(id: string): void {
    this.#playlists = this.#playlists.filter((p) => p.id !== id);
    this.#playlistTracks.delete(id);
    this.#pushPlaylists();
  }

  #playlistAddTracks(id: string, videoIds: string[]): void {
    const list = this.#playlistTracks.get(id);
    if (!list) return;
    const resolve = (v: string) =>
      this.#searchIndex.get(v) ??
      CATALOG.find((t) => t.video_id === v) ??
      this.#queue.find((t) => t.video_id === v);
    for (const v of videoIds) {
      if (list.some((t) => t.video_id === v)) continue; // no dupes, like the real playlist
      const t = resolve(v);
      if (t) list.push({ ...t });
    }
    this.#reconcileCount(id);
    this.#pushPlaylists();
  }

  #playlistRemoveTrack(id: string, videoId: string): void {
    const list = this.#playlistTracks.get(id);
    if (!list) return;
    this.#playlistTracks.set(
      id,
      list.filter((t) => t.video_id !== videoId),
    );
    this.#reconcileCount(id);
    this.#pushPlaylists();
  }

  #playlistPlay(id: string): void {
    const list = this.#playlistTracks.get(id);
    if (!list || list.length === 0) return;
    this.#queue = list.map((t) => ({ ...t }));
    this.#rev++;
    this.#pushQueue();
    this.#jumpTo(0);
    this.#pushPlayer();
  }

  #pushPlaylists(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'playlists',
      // PROVISIONAL shape — see PlaylistsSnapshot in stores/playlists.svelte.ts.
      payload: { kind: 'playlists_snapshot', items: this.#playlists.map((p) => ({ ...p })) },
    });
  }

  // ── transfer wizard (transfer.wizard) ─────────────────────────────────────────────────

  #clearTransferTimers(): void {
    for (const t of this.#transferTimers) clearTimeout(t);
    this.#transferTimers = [];
  }

  #transferList(): void {
    this.#clearTransferTimers();
    this.#transfer = { ...this.#transfer, phase: 'listing', report: null, error: null };
    this.#pushTransfer();
    this.#transferTimers.push(
      setTimeout(() => {
        const sources: SpotifyPlaylist[] = [
          { id: 'sp-1', name: 'Discover Weekly (archive)', count: 30 },
          { id: 'sp-2', name: 'Focus Flow', count: 42 },
          { id: 'sp-3', name: 'Roadtrip 2025', count: 18 },
        ];
        this.#transfer = { ...this.#transfer, phase: 'ready', sources, job: null };
        this.#pushTransfer();
      }, 200),
    );
  }

  #transferStart(spec: TransferSpec): void {
    const ids = Array.isArray(spec.source_ids) ? spec.source_ids : [];
    const total = this.#transfer.sources
      .filter((s) => ids.includes(s.id))
      .reduce((n, s) => n + s.count, 0);
    if (total === 0) return;
    this.#clearTransferTimers();
    const destLabel =
      spec.dest?.kind === 'new'
        ? `new playlist “${spec.dest.name}”`
        : `existing playlist ${(spec.dest as { playlist_id: string })?.playlist_id ?? ''}`;
    this.#transfer = {
      ...this.#transfer,
      phase: 'running',
      job: { done: 0, total, matched: 0, failed: 0 },
      report: null,
      error: null,
    };
    this.#pushTransfer();

    // Coalesced progress: a handful of ticks, not one-per-track (the real wire coalesces).
    const ticks = 4;
    for (let i = 1; i <= ticks; i++) {
      this.#transferTimers.push(
        setTimeout(() => {
          if (this.#transfer.phase !== 'running') return; // cancelled
          const done = Math.round((total * i) / ticks);
          const failed = Math.round(done * 0.08);
          this.#transfer = {
            ...this.#transfer,
            job: { done, total, matched: done - failed, failed },
          };
          this.#pushTransfer();
        }, 150 * i),
      );
    }
    this.#transferTimers.push(
      setTimeout(
        () => {
          if (this.#transfer.phase !== 'running') return;
          const failed = Math.round(total * 0.08);
          this.#transfer = {
            ...this.#transfer,
            phase: 'done',
            job: { done: total, total, matched: total - failed, failed },
            report: {
              matched: total - failed,
              failed,
              skipped: 0,
              unmatched: ['Obscure B-side (live)', 'Untitled demo #7'].slice(0, failed ? 2 : 0),
              dest: destLabel,
            },
          };
          this.#pushTransfer();
        },
        150 * (ticks + 1),
      ),
    );
  }

  #transferCancel(): void {
    this.#clearTransferTimers();
    this.#transfer = {
      kind: 'transfer_state',
      phase: 'idle',
      sources: this.#transfer.sources,
      job: null,
      report: null,
      error: null,
    };
    this.#pushTransfer();
  }

  #pushTransfer(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'transfer',
      // PROVISIONAL shape — see TransferState in stores/transfer.svelte.ts.
      payload: JSON.parse(JSON.stringify(this.#transfer)) as TransferState,
    });
  }

  // ── accounts (settings.accounts) ──────────────────────────────────────────────────────

  #accountConnect(service: 'lastfm' | 'spotify'): void {
    // Browser-approval: hand the GUI an auth URL first (it opens it via win:openUrl).
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'accounts',
      // PROVISIONAL shape — see AccountsAuthUrl in stores/accounts.svelte.ts.
      payload: {
        kind: 'accounts_auth_url',
        service,
        url: `https://example.com/${service}/authorize?demo=1`,
      },
    });
    // …then the approval lands and the snapshot flips connected.
    setTimeout(() => {
      if (service === 'lastfm') {
        this.#accounts.lastfm = {
          connected: true,
          user: 'demo_listener',
          scrobbling: true,
          love_sync: this.#accounts.lastfm.love_sync,
        };
      } else {
        this.#accounts.spotify = {
          ...this.#accounts.spotify,
          connected: true,
          user: 'demo_spotify',
        };
      }
      this.#pushAccounts();
    }, 200);
  }

  #listenBrainzConfigure(p: Record<string, unknown>): void {
    const lb = this.#accounts.listenbrainz;
    if ('submit' in p) lb.submit = Boolean(p.submit);
    if ('token' in p) lb.has_token = String(p.token ?? '').length > 0; // write-only presence
    if ('custom_url' in p) lb.custom_url = String(p.custom_url ?? '') || null;
    this.#pushAccounts();
  }

  #accountSet(service: string, field: string, value: unknown): void {
    if (field === 'scrobble_local') {
      this.#accounts.scrobble_local = Boolean(value);
    } else if (service === 'lastfm') {
      (this.#accounts.lastfm as unknown as Record<string, unknown>)[field] = value;
    } else if (service === 'spotify') {
      (this.#accounts.spotify as unknown as Record<string, unknown>)[field] = value;
    } else if (service === 'listenbrainz') {
      (this.#accounts.listenbrainz as unknown as Record<string, unknown>)[field] = value;
    }
    this.#pushAccounts();
  }

  #pushAccounts(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'accounts',
      // PROVISIONAL shape — see AccountsSnapshot in stores/accounts.svelte.ts.
      payload: JSON.parse(JSON.stringify(this.#accounts)) as AccountsSnapshot,
    });
  }

  // ── pushes ──────────────────────────────────────────────────────────────────────────

  #pushPlayer(): void {
    const t = this.#current();
    this.#samplePosition();
    this.#armEndTimer();
    const model: PlayerModel = {
      track: t,
      paused: this.#paused,
      volume: this.#volume,
      speed_tenths: this.#speedTenths,
      elapsed_ms: t ? Math.round(this.#elapsedMs) : null,
      duration_ms: t?.duration_ms ?? null,
      position_epoch: this.#epoch,
      shuffle: this.#shuffle,
      repeat: this.#repeat,
      streaming: false,
      radio_mode: this.#radioMode,
      stream_now_playing:
        t?.duration_ms == null && t ? '지금 흐르는 곡: lo-fi beats to ship to' : null,
      owner_mode: 'daemon',
      eq: { preset: 'flat', bands: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], normalize: false },
      queue_pos: this.#pos,
      queue_len: this.#queue.length,
    };
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'player',
      payload: { kind: 'player_snapshot', model },
    });
  }

  #pushQueue(): void {
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'queue',
      payload: { kind: 'queue_snapshot', model: { rev: this.#rev, items: this.#queue } },
    });
  }

  #pushLyrics(): void {
    const t = this.#current();
    this.#emit({
      v: 1,
      kind: 'event',
      topic: 'lyrics',
      // PROVISIONAL shape — see TODO(wire:B1/lyrics.live) in lyrics.svelte.ts.
      payload: {
        kind: 'lyrics_snapshot',
        video_id: t?.video_id ?? null,
        lines: (t && LYRICS[t.video_id]) ?? [],
      },
    });
  }

  /** Tracks end on schedule and auto-advance, so the demo feels alive. */
  #armEndTimer(): void {
    if (this.#endTimer) clearTimeout(this.#endTimer);
    this.#endTimer = null;
    const t = this.#current();
    if (!t || this.#paused || t.duration_ms == null) return;
    const remaining = (t.duration_ms - this.#elapsedMs) / (this.#speedTenths / 10);
    this.#endTimer = setTimeout(
      () => {
        this.#advance(1, false);
        this.#pushPlayer();
      },
      Math.max(250, remaining),
    );
  }

  #emit(env: InEnvelope): void {
    this.#cb?.(env);
  }
}
