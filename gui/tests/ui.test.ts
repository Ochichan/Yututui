import { describe, expect, it, vi } from 'vitest';
import { UiStore } from '../src/lib/stores/ui.svelte';

describe('UiStore WebView snapshot', () => {
  it('restores only a complete bounded snapshot', () => {
    const ui = new UiStore({
      view: 'settings',
      queueOpen: false,
      settingsTab: 'playback',
      libraryTab: 'favorites',
      scrollY: 420,
      activeControl: 'speed',
      scrollPositions: { 'settings-pane': 73 },
      drafts: { 'search-query': 'cats' },
    });
    expect(ui.view).toBe('settings');
    expect(ui.queueOpen).toBe(false);
    expect(ui.settingsTab).toBe('playback');
    expect(ui.libraryTab).toBe('favorites');

    const invalid = new UiStore({
      view: 'look-alike',
      queueOpen: false,
      settingsTab: 'playback',
      libraryTab: 'favorites',
      scrollY: 420,
    });
    expect(invalid.view).toBe('now');
    expect(invalid.queueOpen).toBe(true);
  });

  it('restores bounded scroll and focus after mount', () => {
    document.body.innerHTML = `
      <button id="speed">Speed</button>
      <div data-ui-scroll-key="search-results"></div>
      <input id="search-query" data-ui-draft-key="search-query" value="old" />
      <input type="password" data-ui-draft-key="secret" value="do-not-copy" />
    `;
    const scrollTo = vi.spyOn(window, 'scrollTo').mockImplementation(() => undefined);
    const results = document.querySelector<HTMLElement>('[data-ui-scroll-key]')!;
    const query = document.querySelector<HTMLInputElement>('#search-query')!;
    const ui = new UiStore();
    ui.restoreDocument({
      view: 'now',
      queueOpen: true,
      settingsTab: 'general',
      libraryTab: 'all',
      scrollY: 42,
      activeControl: 'speed',
      scrollPositions: { 'search-results': 321 },
      drafts: { 'search-query': 'restored', secret: 'blocked' },
    });
    expect(scrollTo).toHaveBeenCalledWith({ top: 42, behavior: 'instant' });
    expect(results.scrollTop).toBe(321);
    expect(query.value).toBe('restored');
    expect(document.activeElement?.id).toBe('speed');

    results.scrollTop = 654;
    query.value = 'next draft';
    const persisted = ui.snapshot();
    expect(persisted.scrollPositions).toEqual({ 'search-results': 654 });
    expect(persisted.drafts).toEqual({ 'search-query': 'next draft' });
    expect(persisted.drafts).not.toHaveProperty('secret');
    scrollTo.mockRestore();
  });

  it('keeps restored scroll and draft entries for inactive views', () => {
    document.body.innerHTML = `
      <div data-ui-scroll-key="search-results"></div>
      <input data-ui-draft-key="search-query" value="old" />
    `;
    const results = document.querySelector<HTMLElement>('[data-ui-scroll-key]')!;
    const query = document.querySelector<HTMLInputElement>('[data-ui-draft-key]')!;
    const ui = new UiStore({
      view: 'search',
      queueOpen: false,
      settingsTab: 'general',
      libraryTab: 'all',
      scrollY: 0,
      scrollPositions: { 'search-results': 321, 'queue-list': 88 },
      drafts: { 'search-query': 'restored', 'ai-prompt': 'saved prompt' },
    });

    results.scrollTop = 654;
    query.value = 'next draft';
    expect(ui.snapshot().scrollPositions).toEqual({ 'search-results': 654, 'queue-list': 88 });
    expect(ui.snapshot().drafts).toEqual({
      'search-query': 'next draft',
      'ai-prompt': 'saved prompt',
    });
  });

  it('rejects oversized nested snapshot state as one unit', () => {
    const ui = new UiStore({
      view: 'search',
      queueOpen: true,
      settingsTab: 'general',
      libraryTab: 'all',
      scrollY: 0,
      scrollPositions: Object.fromEntries(
        Array.from({ length: 17 }, (_, index) => [`scroll-${index}`, index]),
      ),
    });
    expect(ui.view).toBe('now');
    expect(ui.queueOpen).toBe(true);
  });
});
