// The store bundle main.ts assembles and every view receives — explicit props, no
// implicit context magic, so component tests can hand in fakes wholesale.

import type { BootPayload } from './ipc/boot';
import type { Client } from './ipc/client';
import type { ConnectionStore } from './stores/connection.svelte';
import type { ThemeStore } from './stores/theme.svelte';
import type { UiStore } from './stores/ui.svelte';
import type { PlaybackStore } from './stores/playback.svelte';
import type { QueueStore } from './stores/queue.svelte';
import type { SearchStore } from './stores/search.svelte';
import type { LyricsStore } from './stores/lyrics.svelte';
import type { ToastStore } from './stores/toasts.svelte';
import type { WipStore } from './wiring/wip.svelte';

export interface AppCtx {
  boot: BootPayload;
  client: Client;
  /** True when running against the in-page demo core instead of the Rust shell. */
  demo: boolean;
  connection: ConnectionStore;
  theme: ThemeStore;
  ui: UiStore;
  playback: PlaybackStore;
  queue: QueueStore;
  search: SearchStore;
  lyrics: LyricsStore;
  toasts: ToastStore;
  wip: WipStore;
}
