// Full-app smoke: mount App against the demo core exactly like main.ts does, and assert
// the wired tier actually renders — the strongest cheap proof that the frame, stores,
// and demo core agree end to end.

import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import App from '../src/App.svelte';
import { Client } from '../src/lib/ipc/client';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { AppCtx } from '../src/lib/ctx';
import { readBoot } from '../src/lib/ipc/boot';
import { ConnectionStore } from '../src/lib/stores/connection.svelte';
import { ThemeStore } from '../src/lib/stores/theme.svelte';
import { UiStore } from '../src/lib/stores/ui.svelte';
import { PlaybackStore } from '../src/lib/stores/playback.svelte';
import { QueueStore } from '../src/lib/stores/queue.svelte';
import { SearchStore } from '../src/lib/stores/search.svelte';
import { LibraryStore } from '../src/lib/stores/library.svelte';
import { AiStore } from '../src/lib/stores/ai.svelte';
import { DownloadsStore } from '../src/lib/stores/downloads.svelte';
import { SettingsStore } from '../src/lib/stores/settings.svelte';
import { AnimStore } from '../src/lib/stores/anim.svelte';
import { LyricsStore } from '../src/lib/stores/lyrics.svelte';
import { ToastStore } from '../src/lib/stores/toasts.svelte';
import { WipStore } from '../src/lib/wiring/wip.svelte';

function assemble(): AppCtx {
  const client = new Client(new DemoCoreTransport());
  const connection = new ConnectionStore(client);
  const toasts = new ToastStore();
  toasts.attach(client);
  const ctx: AppCtx = {
    boot: readBoot(),
    client,
    demo: true,
    connection,
    theme: new ThemeStore(),
    ui: new UiStore(),
    playback: new PlaybackStore(client),
    queue: new QueueStore(client),
    search: new SearchStore(client),
    library: new LibraryStore(client),
    ai: new AiStore(client),
    downloads: new DownloadsStore(client),
    settings: new SettingsStore(client),
    anim: new AnimStore(client),
    lyrics: new LyricsStore(client),
    toasts,
    wip: new WipStore(connection),
  };
  client.sub([
    'player',
    'queue',
    'lyrics',
    'search',
    'library',
    'ai',
    'downloads',
    'settings',
    'system',
  ]);
  return ctx;
}

const settle = () => new Promise((r) => setTimeout(r, 250));

describe('App against the demo core', () => {
  it('boots online and renders the wired tier', async () => {
    const ctx = assemble();
    render(App, { props: { ctx } });
    await settle();

    // Transport bar + queue dock show the demo core's live state.
    expect(screen.getAllByText('Purrple Rain').length).toBeGreaterThan(0);
    expect(screen.getAllByText(/The Whisker Quartet/).length).toBeGreaterThan(0);
    expect(ctx.connection.online).toBe(true);
    expect(ctx.queue.items.length).toBe(10);
    expect(ctx.playback.track?.video_id).toBe('demo-001');
  });

  it('a pending feature opens the patch-bay modal instead of doing nothing', async () => {
    const ctx = assemble();
    render(App, { props: { ctx } });
    await settle();

    ctx.wip.gate('library.playlists');
    await settle();
    expect(ctx.wip.active).toBe('library.playlists');
    expect(screen.getByText('Not wired up yet')).toBeTruthy();
    expect(screen.getByText('Copy agent brief')).toBeTruthy();
  });
});
