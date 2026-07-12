// Action id → runtime effect (docs/gui/05 §8, 07 §8). The dispatcher resolves a keypress to
// an action id via the keymap store; this maps the id to a GUI effect. The ids are the
// src/keymap.rs action ids (the wire catalog). Actions whose effect needs selection/model
// state not yet modeled here return false (unhandled) so the key is NOT swallowed — the
// binding still renders and rebinds in Settings→Hotkeys (it drives the shared TUI keymap),
// its GUI effect lands with the feature it belongs to.

import type { AppCtx } from '../ctx';
import type { SettingsTab } from '../stores/ui.svelte';

const SETTINGS_TABS: SettingsTab[] = [
  'general',
  'playback',
  'hotkeys',
  'graphics',
  'djgem',
  'accounts',
];

/** Run the action; returns true if a handler fired (caller then preventDefaults the event). */
export function runAction(id: string, ctx: AppCtx): boolean {
  const { playback, ui } = ctx;
  switch (id) {
    case 'toggle_pause':
      playback.togglePause();
      return true;
    case 'seek_back':
      playback.seekTo(Math.max(0, (playback.positionMs ?? 0) - 5000));
      return true;
    case 'seek_forward':
      playback.seekTo((playback.positionMs ?? 0) + 5000);
      return true;
    case 'vol_up':
      playback.setVolume(playback.volume + 5);
      return true;
    case 'vol_down':
      playback.setVolume(playback.volume - 5);
      return true;
    case 'next_track':
      playback.next();
      return true;
    case 'prev_track':
      playback.prev();
      return true;
    case 'toggle_shuffle':
      playback.toggleShuffle();
      return true;
    case 'cycle_repeat':
      playback.cycleRepeat();
      return true;
    case 'cycle_rating':
      playback.cycleRating();
      return true;
    case 'open_queue':
      ui.toggleQueue();
      return true;
    case 'toggle_help':
      ui.helpOpen = !ui.helpOpen;
      return true;
    case 'toggle_about':
      ui.aboutOpen = !ui.aboutOpen;
      return true;
    case 'back':
      return ui.closeTopOverlay();
    case 'home':
      ui.setView('now');
      return true;
    case 'open_search':
      ui.setView('search');
      return true;
    case 'open_library':
      ui.setView('library');
      return true;
    case 'open_ai':
      ui.setView('ai');
      return true;
    case 'open_settings':
      ui.setView('settings');
      return true;
    case 'focus_next':
    case 'focus_prev': {
      // The GUI's tab focus lives in Settings only; elsewhere the browser's own Tab order
      // applies, so the key is deliberately NOT swallowed.
      if (ui.view !== 'settings') return false;
      const cur = SETTINGS_TABS.indexOf(ui.settingsTab);
      if (cur < 0) return false;
      const delta = id === 'focus_next' ? 1 : SETTINGS_TABS.length - 1;
      ui.settingsTab = SETTINGS_TABS[(cur + delta) % SETTINGS_TABS.length];
      return true;
    }
    default:
      return false;
  }
}
