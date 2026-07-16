// Full-app smoke: mount App against the demo core exactly like main.ts does, and assert
// the wired tier actually renders — the strongest cheap proof that the frame, stores,
// and demo core agree end to end.

import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, fireEvent, within } from '@testing-library/svelte';
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
import { PlaylistsStore } from '../src/lib/stores/playlists.svelte';
import { TransferStore } from '../src/lib/stores/transfer.svelte';
import { AccountsStore } from '../src/lib/stores/accounts.svelte';
import { SettingsStore } from '../src/lib/stores/settings.svelte';
import { AnimStore } from '../src/lib/stores/anim.svelte';
import { KeymapStore } from '../src/lib/stores/keymap.svelte';
import { LyricsStore } from '../src/lib/stores/lyrics.svelte';
import { WhyGemStore } from '../src/lib/stores/whygem.svelte';
import { ToastStore } from '../src/lib/stores/toasts.svelte';

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
    theme: new ThemeStore(client),
    ui: new UiStore(),
    playback: new PlaybackStore(client),
    queue: new QueueStore(client, (message) => toasts.show('error', message)),
    search: new SearchStore(client),
    library: new LibraryStore(client),
    ai: new AiStore(client),
    downloads: new DownloadsStore(client),
    playlists: new PlaylistsStore(client),
    transfer: new TransferStore(client),
    accounts: new AccountsStore(client),
    settings: new SettingsStore(client, (message) => toasts.show('error', message)),
    anim: new AnimStore(client),
    keymap: new KeymapStore(client),
    lyrics: new LyricsStore(client),
    whygem: new WhyGemStore(client),
    toasts,
  };
  client.sub([
    'player',
    'queue',
    'lyrics',
    'search',
    'library',
    'playlists',
    'ai',
    'downloads',
    'transfer',
    'accounts',
    'settings',
    'system',
  ]);
  return ctx;
}

const settle = () => new Promise((r) => setTimeout(r, 250));
afterEach(cleanup);

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

  it('the library playlists tab lists demo playlists (wired, not the pending card)', async () => {
    const ctx = assemble();
    const { container } = render(App, { props: { ctx } });
    const q = within(container);
    await settle();

    ctx.ui.view = 'library';
    ctx.ui.libraryTab = 'playlists';
    await settle();

    // The demo core seeds two playlists on the `playlists` topic.
    expect(q.getByText('Late-night coding')).toBeTruthy();
    expect(q.queryByText('Wire pending — lands in M2')).toBeNull();
  });

  it('renders one accessible long-form selector and hides the legacy cache rows', async () => {
    const ctx = assemble();
    const { container } = render(App, { props: { ctx } });
    const q = within(container);
    await settle();

    ctx.ui.view = 'settings';
    ctx.ui.settingsTab = 'playback';
    await settle();

    const select = q.getByLabelText('Long-form seek') as HTMLSelectElement;
    expect(select.disabled).toBe(false);
    expect(select.value).toBe('off');
    expect(within(select).getByRole('option', { name: 'Auto (experimental)' })).toBeTruthy();
    expect(within(select).getByRole('option', { name: 'Off' })).toBeTruthy();
    expect(within(select).getByRole('option', { name: 'On' })).toBeTruthy();
    expect(q.getByText('Backend')).toBeTruthy();
    expect(q.getByText('mpv output')).toBeTruthy();
    expect(q.getByText('mpv device')).toBeTruthy();
    expect(q.queryByText('Cache forward')).toBeNull();
    expect(q.queryByText('Cache back')).toBeNull();
  });

  it('the keymap dispatcher routes a real keypress to its action', async () => {
    const ctx = assemble();
    render(App, { props: { ctx } });
    await settle(); // let the settings push seed the keymap model

    expect(ctx.ui.view).toBe('now');
    // 's' is player open_search; the dispatcher resolves + runs it against the live keymap.
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 's', bubbles: true }));
    expect(ctx.ui.view).toBe('search');

    // '2' is a GUI-fixed rail digit (not in the core keymap) — jumps back to view 2 = search,
    // then '1' back home so the next assertion starts from a known view.
    window.dispatchEvent(new KeyboardEvent('keydown', { key: '1', bubbles: true }));
    expect(ctx.ui.view).toBe('now');

    // '?' is global toggle_help.
    window.dispatchEvent(new KeyboardEvent('keydown', { key: '?', shiftKey: true, bubbles: true }));
    expect(ctx.ui.helpOpen).toBe(true);
  });

  it('routes text editing through live remaps and suppresses unbound factory keys', async () => {
    const ctx = assemble();
    const { container } = render(App, { props: { ctx } });
    await settle();

    ctx.ui.view = 'search';
    await settle();
    const field = container.querySelector<HTMLInputElement>('#search-query')!;
    const value = 'a👨‍👩‍👧‍👦 beta';
    await fireEvent.input(field, { target: { value } });
    const familyEnd = 'a👨‍👩‍👧‍👦'.length;

    field.setSelectionRange(familyEnd, familyEnd);
    const factoryLeft = new KeyboardEvent('keydown', {
      key: 'ArrowLeft',
      code: 'ArrowLeft',
      bubbles: true,
      cancelable: true,
    });
    field.dispatchEvent(factoryLeft);
    expect(factoryLeft.defaultPrevented).toBe(true);
    expect(field.selectionStart).toBe(1);

    await ctx.keymap.rebind('common', 'move_cursor_left', 'f8');
    field.setSelectionRange(familyEnd, familyEnd);
    const movedFactoryLeft = new KeyboardEvent('keydown', {
      key: 'ArrowLeft',
      code: 'ArrowLeft',
      bubbles: true,
      cancelable: true,
    });
    field.dispatchEvent(movedFactoryLeft);
    expect(movedFactoryLeft.defaultPrevented).toBe(true);
    expect(field.selectionStart).toBe(familyEnd);

    const remappedLeft = new KeyboardEvent('keydown', {
      key: 'F8',
      code: 'F8',
      bubbles: true,
      cancelable: true,
    });
    field.dispatchEvent(remappedLeft);
    expect(remappedLeft.defaultPrevented).toBe(true);
    expect(field.selectionStart).toBe(1);

    ctx.keymap.unbind('common', 'move_cursor_word_right');
    await settle();
    field.setSelectionRange(0, 0);
    const unboundWordRight = new KeyboardEvent('keydown', {
      key: 'ArrowRight',
      code: 'ArrowRight',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    field.dispatchEvent(unboundWordRight);
    expect(unboundWordRight.defaultPrevented).toBe(true);
    expect(field.selectionStart).toBe(0);
  });

  it('keeps Ctrl+Backspace editing distinct from Ctrl+H navigation', async () => {
    const ctx = assemble();
    const { container } = render(App, { props: { ctx } });
    await settle();

    ctx.ui.view = 'search';
    await settle();
    const field = container.querySelector<HTMLInputElement>('#search-query')!;
    await fireEvent.input(field, { target: { value: 'alpha beta' } });
    field.focus();
    field.setSelectionRange(field.value.length, field.value.length);

    const deleteWord = new KeyboardEvent('keydown', {
      key: 'Backspace',
      code: 'Backspace',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    field.dispatchEvent(deleteWord);
    expect(deleteWord.defaultPrevented).toBe(true);
    expect(field.value).toBe('alpha ');
    expect(ctx.ui.view).toBe('search');

    const home = new KeyboardEvent('keydown', {
      key: 'h',
      code: 'KeyH',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    field.dispatchEvent(home);
    expect(home.defaultPrevented).toBe(true);
    expect(ctx.ui.view).toBe('now');
  });

  it('flushes pending volume when the WebView is hidden', async () => {
    const ctx = assemble();
    const flush = vi.spyOn(ctx.playback, 'flushVolume');
    render(App, { props: { ctx } });
    await settle();

    window.dispatchEvent(new Event('pagehide'));
    expect(flush).toHaveBeenCalled();
  });

  it('the help overlay renders the live keymap cheat sheet and filters it', async () => {
    const ctx = assemble();
    // Scope to this render's subtree so repeated labels elsewhere in the frame stay unambiguous.
    const { container } = render(App, { props: { ctx } });
    const q = within(container);
    await settle(); // seed the keymap model

    ctx.ui.helpOpen = true;
    await settle();
    // The grouped cheat sheet reads the keymap model (not a fabricated table). 'Global' and
    // These are context group headers unique to the overlay.
    expect(q.getByText('Global')).toBeTruthy();
    expect(q.getByText('Common navigation & text editing')).toBeTruthy();
    expect(q.getAllByText('Play / pause').length).toBeGreaterThan(0);

    // Filtering narrows the rows.
    await fireEvent.input(q.getByLabelText('Filter shortcuts'), { target: { value: 'volume' } });
    expect(q.getByText('Volume up')).toBeTruthy();
    expect(q.queryByText('Play / pause')).toBeNull();
  });
});
