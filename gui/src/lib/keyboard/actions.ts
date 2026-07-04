// Action id → runtime effect (docs/gui/05 §8, 07 §8). The dispatcher resolves a keypress to
// an action id via the keymap store; this maps the id to a GUI effect. Actions whose effect
// needs selection/model state not yet modeled here return false (unhandled) so the key is NOT
// swallowed — the binding still renders and rebinds in Settings→Hotkeys, its effect lands
// with the feature it belongs to.

import type { AppCtx } from '../ctx';
import { NAV_ITEMS, type SettingsTab } from '../stores/ui.svelte';

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
  const { playback, ui, wip } = ctx;
  switch (id) {
    case 'play_pause':
      playback.togglePause();
      return true;
    case 'seek_back':
      playback.seekTo(Math.max(0, (playback.positionMs ?? 0) - 5000));
      return true;
    case 'seek_forward':
      playback.seekTo((playback.positionMs ?? 0) + 5000);
      return true;
    case 'volume_up':
      playback.setVolume(playback.volume + 5);
      return true;
    case 'volume_down':
      playback.setVolume(playback.volume - 5);
      return true;
    case 'next':
      playback.next();
      return true;
    case 'prev':
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
    case 'clear_upcoming':
      ctx.client.cmd('queue_clear_upcoming');
      return true;
    case 'toggle_queue':
      ui.toggleQueue();
      return true;
    case 'help':
      ui.helpOpen = !ui.helpOpen;
      return true;
    case 'back':
      if (wip.active) {
        wip.close();
        return true;
      }
      return ui.closeTopOverlay();
    case 'view_now':
    case 'view_search':
    case 'view_library':
    case 'view_ai':
    case 'view_settings': {
      const item = NAV_ITEMS.find((n) => `view_${n.id}` === id);
      if (item) {
        ui.setView(item.id);
        return true;
      }
      return false;
    }
    case 'next_tab':
    case 'prev_tab': {
      const cur = SETTINGS_TABS.indexOf(ui.settingsTab);
      if (cur < 0) return false;
      const delta = id === 'next_tab' ? 1 : SETTINGS_TABS.length - 1;
      ui.settingsTab = SETTINGS_TABS[(cur + delta) % SETTINGS_TABS.length];
      return true;
    }
    default:
      return false;
  }
}
