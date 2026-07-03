// Mirrors the `player` topic and owns the client-only playback niceties: position
// interpolation, optimistic seek hold, and the volume local echo — constants ported from
// the mini player per docs/gui/05 §5.1. Truth is always the next authoritative push.
//
// This store is LIVE-WIRED: it consumes real player_snapshot pushes today (B0 wire) and
// sends the v7-frozen transport commands. The desktop bridge forwards them once the
// gateway→bridge command path lands (M1) — see gui/WIRING.md "already-wired tier".

import type { PlayerModel } from '../../generated/protocol/PlayerModel';
import type { PushEvent } from '../../generated/protocol/PushEvent';
import type { Client } from '../ipc/client';
import { SEEK_HOLD_MS, VOLUME_ECHO_MS, VOLUME_SEND_DEBOUNCE_MS, interpolate } from '../time';

export class PlaybackStore {
  model = $state<PlayerModel | null>(null);
  /** 1 Hz heartbeat while playing+visible; reads of `positionMs` re-run on it. */
  #now = $state(performance.now());
  #anchorAt = performance.now();
  #holdUntil = 0;
  #localVolume = $state<number | null>(null);
  #volumeEchoUntil = 0;
  #volumeTimer: ReturnType<typeof setTimeout> | null = null;
  #ticker: ReturnType<typeof setInterval> | null = null;
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    client.on('player', (payload) => this.#onPush(payload as PushEvent));
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', () => this.#retick());
    }
  }

  get track() {
    return this.model?.track ?? null;
  }

  get paused(): boolean {
    return this.model?.paused ?? true;
  }

  /** Live stream ⇒ "ON AIR" seek treatment (docs/gui/07 §1). */
  get live(): boolean {
    return this.model != null && this.model.duration_ms == null && this.model.elapsed_ms != null;
  }

  /** Interpolated position; null when nothing is playing. */
  get positionMs(): number | null {
    const m = this.model;
    if (!m) return null;
    return interpolate(
      {
        elapsedMs: m.elapsed_ms,
        anchorAt: this.#anchorAt,
        speedTenths: m.speed_tenths,
        paused: m.paused,
        durationMs: m.duration_ms,
      },
      this.#now,
    );
  }

  /** Volume with the local drag echo applied (panel.rs:1845). */
  get volume(): number {
    if (this.#localVolume != null && performance.now() < this.#volumeEchoUntil) {
      return this.#localVolume;
    }
    return this.model?.volume ?? 0;
  }

  // ── commands (fire-and-forget; truth arrives via the next push) ──────────────────

  togglePause(): void {
    if (this.model) {
      // Freeze the interpolated position before flipping so pause doesn't rewind.
      const pos = this.positionMs;
      if (pos != null) this.model.elapsed_ms = pos;
      this.model.paused = !this.model.paused; // optimistic
    }
    this.#anchorRebase();
    this.#client.cmd('toggle_pause');
    this.#retick();
  }

  next(): void {
    this.#client.cmd('next');
  }

  prev(): void {
    this.#client.cmd('prev');
  }

  toggleShuffle(): void {
    if (this.model) this.model.shuffle = !this.model.shuffle; // optimistic
    this.#client.cmd('toggle_shuffle');
  }

  cycleRepeat(): void {
    if (this.model) {
      this.model.repeat =
        this.model.repeat === 'off' ? 'all' : this.model.repeat === 'all' ? 'one' : 'off';
    }
    this.#client.cmd('cycle_repeat');
  }

  /** Music ⇄ Radio mode. Truth is `player.radio_mode`; the switch flips it via the existing
   *  RadioMode setting change (works v7+), reflected on the next player push. */
  setRadioMode(on: boolean): void {
    if (this.model) this.model.radio_mode = on; // optimistic
    this.#client.cmd('set_setting', {
      change: { setting: 'radio_mode', state: on ? 'on' : 'off' },
    });
  }

  /** 👍/–/👎 cycle synthesized core-side from favorite+disliked (docs/gui/02 §11.2). */
  cycleRating(): void {
    const t = this.track;
    if (!t) return;
    // Optimistic mirror of the TUI's CycleRating: none → up → down → none.
    if (!t.favorite && !t.disliked) t.favorite = true;
    else if (t.favorite) [t.favorite, t.disliked] = [false, true];
    else t.disliked = false;
    this.#client.cmd('rate', { video_id: t.video_id, rating: 'cycle' });
  }

  /** Optimistic seek: anchor locally, hold off authoritative pushes (panel.rs:1797). */
  seekTo(ms: number): void {
    const m = this.model;
    if (!m) return;
    const clamped = m.duration_ms == null ? ms : Math.max(0, Math.min(ms, m.duration_ms));
    m.elapsed_ms = clamped;
    this.#anchorRebase();
    this.#holdUntil = performance.now() + SEEK_HOLD_MS;
    this.#client.cmd('seek_to', { ms: Math.round(clamped) });
  }

  /** Local echo + 70 ms debounce so dragging is smooth (panel.rs:1845,2006). */
  setVolume(percent: number): void {
    const v = Math.max(0, Math.min(100, Math.round(percent)));
    this.#localVolume = v;
    this.#volumeEchoUntil = performance.now() + VOLUME_ECHO_MS;
    if (this.#volumeTimer) clearTimeout(this.#volumeTimer);
    this.#volumeTimer = setTimeout(() => {
      this.#volumeTimer = null;
      this.#client.cmd('set_volume', { percent: v });
    }, VOLUME_SEND_DEBOUNCE_MS);
  }

  // ── push intake ───────────────────────────────────────────────────────────────────

  #onPush(ev: PushEvent): void {
    if (ev.kind !== 'player_snapshot') return;
    const now = performance.now();
    const incoming = ev.model;
    if (now < this.#holdUntil && this.model) {
      // Mid-hold after a local seek: take everything except the position fields so the
      // scrubber doesn't jump back (docs/gui/05 §5.1).
      incoming.elapsed_ms = this.model.elapsed_ms;
    } else {
      this.#anchorAt = now;
    }
    this.model = incoming;
    this.#retick();
  }

  #anchorRebase(): void {
    this.#anchorAt = performance.now();
  }

  /** Base 1 Hz time ticker — runs only while playing and visible (docs/gui/06 §5). */
  #retick(): void {
    const shouldRun =
      this.model != null &&
      !this.model.paused &&
      this.model.elapsed_ms != null &&
      (typeof document === 'undefined' || document.visibilityState === 'visible');
    if (shouldRun && this.#ticker == null) {
      this.#ticker = setInterval(() => {
        this.#now = performance.now();
      }, 1000);
      this.#now = performance.now();
    } else if (!shouldRun && this.#ticker != null) {
      clearInterval(this.#ticker);
      this.#ticker = null;
      this.#now = performance.now();
    }
  }
}
