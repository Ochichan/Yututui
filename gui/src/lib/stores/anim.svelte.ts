// The animation runtime (docs/gui/06 §5). Mirrors the core's `AnimationsConfig`
// (src/config.rs): a global `master` kill-switch, an fps target, `pause_unfocused`, and 40
// independent effect flags. This store owns the *behaviour* — the shared, fps-gated rAF
// ticker and the `<html class="no-anim">` transition kill — while stores/settings.svelte.ts
// owns the editable form model. Both consume the `settings` push (each client.on handler
// fires); the anim store keeps only its own block, exactly like every other topic store.
//
// The self-suspend contract (docs/gui/06 §5, the web port of the TUI's
// `ambient_animation_running()` rule): the rAF loop runs iff master is on, reduced-motion is
// off, we're focused-or-not-gated, AND at least one consumer is actively subscribed. An
// effect subscribes only while it has work (a one-shot for its duration; an ambient effect
// while playing+visible), so "paused + no active one-shot" ⇒ zero subscribers ⇒ zero rAF
// callbacks — true zero overhead, not a gated no-op. Effect consumers land incrementally
// (most in M5); today the ticker and the master/reduced-motion contract are wired and tested.

import type { Client } from '../ipc/client';
import type { SettingsSnapshot } from './settings.svelte';

// Frame-rate bounds mirror src/config.rs (FPS_MIN/FPS_MAX/FPS_DEFAULT).
export const FPS_MIN = 5;
export const FPS_MAX = 60;
export const FPS_DEFAULT = 30;

/** Feature groups in typical-use, low-to-high incremental render-cost order. */
export const EFFECT_GROUPS = [
  {
    id: 'oneShot',
    effects: [
      'error_shake',
      'like_burst',
      'track_intro',
      'seek_flash',
      'pause_flash',
      'volume_flash',
      'toast',
    ],
  },
  {
    id: 'uiWide',
    effects: ['about_fx', 'popup_fade', 'tabs', 'stagger', 'activity', 'caret', 'selection'],
  },
  {
    id: 'element',
    effects: [
      'time_glow',
      'heart',
      'spinner',
      'controls',
      'eq_bars',
      'seekbar',
      'progress_sparkle',
      'title',
      'lyrics',
      'border_chase',
      'border',
    ],
  },
  {
    id: 'filler',
    effects: [
      'bounce',
      'comets',
      'snow',
      'starfield',
      'fireflies',
      'cube',
      'aquarium',
      'waves',
      'visualizer',
    ],
  },
  {
    id: 'showpiece',
    effects: ['fireworks', 'rain', 'life', 'pipes', 'donut', 'plasma'],
  },
] as const;

export type EffectId = (typeof EFFECT_GROUPS)[number]['effects'][number];
/** The 40 flags flattened in their Settings order (docs/gui/06 §5.1). */
export const EFFECT_IDS: readonly EffectId[] = EFFECT_GROUPS.flatMap((group) => group.effects);
export type EffectFlags = Record<EffectId, boolean>;

export interface AnimationsModel extends EffectFlags {
  /** Global enable. Off ⇒ the app paints identically to today, zero overhead. */
  master: boolean;
  /** Park the ticker while the window is unfocused/hidden (defaults true). */
  pause_unfocused: boolean;
  /** Target frame rate; read through clampFps so a corrupt config can't spin/freeze. */
  fps: number;
}

/** Clamp an fps target to [FPS_MIN, FPS_MAX]; NaN falls back to the default. */
export function clampFps(fps: number): number {
  if (!Number.isFinite(fps)) return FPS_DEFAULT;
  return Math.max(FPS_MIN, Math.min(FPS_MAX, Math.round(fps)));
}

/** The core defaults (src/config.rs): everything off, `pause_unfocused` on, 30 fps. */
export function defaultAnimations(): AnimationsModel {
  const flags = Object.fromEntries(EFFECT_IDS.map((id) => [id, false])) as EffectFlags;
  return { ...flags, master: false, pause_unfocused: true, fps: FPS_DEFAULT };
}

const REDUCED_MOTION = '(prefers-reduced-motion: reduce)';

export class AnimStore {
  /** Last authoritative animations block; null until the first `settings` snapshot. */
  config = $state<AnimationsModel | null>(null);

  #reducedMotion = false;
  #pageVisible = true;
  #windowFocused = true;
  #raf: number | null = null;
  #lastFrameAt = 0;
  #subscribers = new Set<(now: number) => void>();
  /** Dev/test counter: total rAF callbacks delivered, for the "0 when idle" assertion. */
  #frames = 0;

  constructor(client: Client) {
    client.on('settings', (payload) => this.#onPush(payload as SettingsSnapshot));

    if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
      const mq = window.matchMedia(REDUCED_MOTION);
      this.#reducedMotion = mq.matches;
      // Accessibility can flip mid-session (OS toggle); react to it.
      mq.addEventListener?.('change', (e) => {
        this.#reducedMotion = e.matches;
        this.#applyClass();
        this.#retick();
      });
    }
    if (typeof document !== 'undefined') {
      this.#pageVisible = document.visibilityState !== 'hidden';
      document.addEventListener('visibilitychange', () => {
        this.#pageVisible = document.visibilityState !== 'hidden';
        this.#retick();
      });
      window.addEventListener('blur', () => {
        this.#windowFocused = false;
        this.#retick();
      });
      window.addEventListener('focus', () => {
        this.#windowFocused = true;
        this.#retick();
      });
    }
  }

  /** True when animations are effectively enabled (master on AND reduced-motion off). */
  get enabled(): boolean {
    return (this.config?.master ?? false) && !this.#reducedMotion;
  }

  /** Whether a given effect should render right now (enabled AND its flag is on). */
  isOn(effect: EffectId): boolean {
    return this.enabled && (this.config?.[effect] ?? false);
  }

  /** The clamped, effective frame rate the ticker throttles to. */
  get fps(): number {
    return clampFps(this.config?.fps ?? FPS_DEFAULT);
  }

  /** True while the shared rAF loop is scheduled. */
  get running(): boolean {
    return this.#raf != null;
  }

  /** Total frames delivered since construction (dev/test introspection). */
  get frameCount(): number {
    return this.#frames;
  }

  /**
   * Subscribe to the shared, fps-gated frame clock. The loop starts on the first active
   * subscriber and stops when the last one leaves (or when disabled/unfocused). Returns an
   * unsubscribe fn; call it the moment the effect goes idle to honour the zero-overhead rule.
   */
  frame(cb: (now: number) => void): () => void {
    this.#subscribers.add(cb);
    this.#retick();
    return () => {
      this.#subscribers.delete(cb);
      this.#retick();
    };
  }

  // ── internals ────────────────────────────────────────────────────────────────────────

  #onPush(snap: SettingsSnapshot): void {
    if (snap?.kind !== 'settings_snapshot') return;
    const anim = snap.model.animations;
    if (!anim) return;
    this.config = anim;
    this.#applyClass();
    this.#retick();
  }

  /** master off (or reduced-motion) ⇒ `<html class="no-anim">` kills every CSS transition. */
  #applyClass(): void {
    if (typeof document === 'undefined') return;
    document.documentElement.classList.toggle('no-anim', !this.enabled);
  }

  #shouldRun(): boolean {
    if (!this.enabled) return false;
    if (this.#subscribers.size === 0) return false;
    if (this.config?.pause_unfocused && !(this.#pageVisible && this.#windowFocused)) return false;
    return true;
  }

  #retick(): void {
    const run = this.#shouldRun();
    if (run && this.#raf == null) {
      this.#lastFrameAt = 0;
      this.#raf = requestAnimationFrame(this.#loop);
    } else if (!run && this.#raf != null) {
      cancelAnimationFrame(this.#raf);
      this.#raf = null;
    }
  }

  #loop = (now: number): void => {
    // Re-arm first so an unsubscribe inside a callback still leaves the loop consistent.
    this.#raf = requestAnimationFrame(this.#loop);
    const minInterval = 1000 / this.fps;
    if (this.#lastFrameAt !== 0 && now - this.#lastFrameAt < minInterval) return;
    this.#lastFrameAt = now;
    this.#frames++;
    for (const cb of this.#subscribers) cb(now);
    // A callback may have removed the last subscriber; collapse the loop if so.
    if (!this.#shouldRun()) this.#retick();
  };
}
