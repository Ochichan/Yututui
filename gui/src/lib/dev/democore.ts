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
      case 'queue_clear_upcoming':
        this.#queue = this.#queue.slice(0, this.#pos + 1);
        this.#rev++;
        this.#pushQueue();
        break;
      case 'rate':
        this.#rate(String(p.video_id ?? ''));
        break;
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
      radio_mode: false,
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
