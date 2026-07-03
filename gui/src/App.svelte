<script lang="ts">
  // The global frame (docs/gui/07 §0): nav rail · active view · collapsible queue dock,
  // persistent transport bar, connection banner, toast host, overlay hosts (help / about /
  // patch-bay). Keyboard is the provisional set (lib/keyboard/provisional.ts) until the
  // M3 dispatcher honors the user's real keymap.
  import type { AppCtx } from './lib/ctx';
  import { NAV_ITEMS } from './lib/stores/ui.svelte';
  import NowPlaying from './views/NowPlaying.svelte';
  import SearchView from './views/SearchView.svelte';
  import LibraryView from './views/LibraryView.svelte';
  import AiView from './views/AiView.svelte';
  import SettingsView from './views/settings/SettingsView.svelte';
  import QueuePanel from './views/QueuePanel.svelte';
  import TransportBar from './views/TransportBar.svelte';
  import HelpOverlay from './views/overlays/HelpOverlay.svelte';
  import AboutCard from './views/overlays/AboutCard.svelte';
  import WipModal from './lib/components/WipModal.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // The ctx bundle is assembled once in main.ts and its identity never changes — the
  // stores inside are the reactive things, so capturing them at init is intended.
  // svelte-ignore state_referenced_locally
  const { ui, connection, playback, toasts, wip, client, boot, demo } = ctx;

  const bannerText = $derived.by(() => {
    switch (connection.info.state) {
      case 'connecting':
        return 'Connecting to player…';
      case 'degraded':
        return 'Reconnecting to player…';
      case 'offline':
        return connection.info.reason === 'no_core'
          ? 'Player is not running.'
          : 'Player connection lost — reconnecting…';
      default:
        return '';
    }
  });

  const stateColor = $derived(
    connection.info.state === 'online'
      ? 'var(--role-success)'
      : connection.info.state === 'degraded'
        ? 'var(--role-warning)'
        : 'var(--role-error)',
  );

  const radioMode = $derived(playback.model?.radio_mode ?? false);

  function switchMode(radio: boolean) {
    if (radio === radioMode) return;
    // Flip the mode via the RadioMode setting change; the library tab set + search default
    // follow player.radio_mode on the next push (docs/gui/07 §16).
    playback.setRadioMode(radio);
  }

  // Provisional keyboard (single source: lib/keyboard/provisional.ts — table shown in
  // Help + Settings→Hotkeys). TODO(wire:M3/settings.hotkeys): replace with the dispatcher.
  function onkeydown(e: KeyboardEvent) {
    if (e.isComposing) return;
    if (e.key === 'Escape') {
      if (wip.active) {
        wip.close();
        e.preventDefault();
      } else if (ui.closeTopOverlay()) {
        e.preventDefault();
      }
      return;
    }
    // is_typeable guard (lite): never steal plain keys from fields or buttons.
    const t = e.target;
    if (
      t instanceof HTMLElement &&
      t.closest('input, textarea, select, button, [contenteditable]')
    ) {
      return;
    }
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    switch (e.key) {
      case ' ':
        playback.togglePause();
        break;
      case 'ArrowLeft':
        playback.seekTo(Math.max(0, (playback.positionMs ?? 0) - 5000));
        break;
      case 'ArrowRight':
        playback.seekTo((playback.positionMs ?? 0) + 5000);
        break;
      case 'ArrowUp':
        playback.setVolume(playback.volume + 5);
        break;
      case 'ArrowDown':
        playback.setVolume(playback.volume - 5);
        break;
      case '1':
      case '2':
      case '3':
      case '4':
      case '5':
        ui.setView(NAV_ITEMS[Number(e.key) - 1].id);
        break;
      case 'q':
        ui.toggleQueue();
        break;
      case '?':
        ui.helpOpen = !ui.helpOpen;
        break;
      default:
        return;
    }
    e.preventDefault();
  }
</script>

<svelte:window {onkeydown} />

{#if connection.info.state !== 'online'}
  <div class="banner" role="status" class:offline={connection.info.state === 'offline'}>
    <span class="dot" style:background={stateColor}></span>
    <span>{bannerText}</span>
    {#if connection.info.state === 'offline'}
      <button class="banner-action" onclick={() => client.win('startDaemon')}>Start daemon</button>
    {/if}
  </div>
{/if}

<div class="frame">
  <div class="shell">
    <nav class="rail" aria-label="Primary">
      {#each NAV_ITEMS as item, i (item.id)}
        <button
          class="rail-btn"
          class:active={ui.view === item.id}
          aria-current={ui.view === item.id ? 'page' : undefined}
          title="{item.label} ({i + 1})"
          onclick={() => ui.setView(item.id)}
        >
          <span class="glyph" aria-hidden="true">{item.glyph}</span>
          <span class="rail-label">{item.label}</span>
        </button>
      {/each}

      <div class="rail-spacer"></div>

      <div class="mode-switch" role="radiogroup" aria-label="Player mode">
        <button
          class="mode"
          class:on={!radioMode}
          role="radio"
          aria-checked={!radioMode}
          onclick={() => switchMode(false)}>♪ Music</button
        >
        <button
          class="mode"
          class:on={radioMode}
          role="radio"
          aria-checked={radioMode}
          onclick={() => switchMode(true)}>📻 Radio</button
        >
      </div>

      <div class="rail-foot">
        <button class="foot-line" onclick={() => (ui.aboutOpen = true)} title="About ytm-tui">
          <span class="dot" style:background={stateColor}></span>
          <span class="mono">{connection.info.state}</span>
        </button>
        <span class="foot-line mono dim">
          core {connection.info.coreVersion ?? '—'} · v{connection.info.protocolVersion ??
            boot.protocolVersion}
        </span>
        {#if demo}
          <span
            class="demo-chip mono"
            title="Running against the in-page demo core — no Rust shell"
          >
            ● demo core
          </span>
        {/if}
      </div>
    </nav>

    <main class="view">
      {#if ui.view === 'now'}
        <NowPlaying {ctx} />
      {:else if ui.view === 'search'}
        <SearchView {ctx} />
      {:else if ui.view === 'library'}
        <LibraryView {ctx} />
      {:else if ui.view === 'ai'}
        <AiView {ctx} />
      {:else}
        <SettingsView {ctx} />
      {/if}
    </main>

    {#if ui.queueOpen}
      <QueuePanel {ctx} />
    {/if}
  </div>

  <TransportBar {ctx} />
</div>

{#if ui.helpOpen}
  <HelpOverlay {ctx} />
{/if}
{#if ui.aboutOpen}
  <AboutCard {ctx} />
{/if}
<WipModal {wip} {client} {toasts} transportLive={!demo} />

<div class="toasts" aria-live="polite">
  {#each toasts.toasts as t (t.id)}
    <button class="toast {t.severity}" onclick={() => toasts.dismiss(t.id)} title="Dismiss"
      >{t.text}</button
    >
  {/each}
</div>

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-4);
    background: var(--surface-2);
    color: var(--role-text-primary);
    border-bottom: 1px solid var(--role-border-muted);
    font-size: 13px;
  }
  .banner.offline {
    border-bottom-color: var(--role-error);
  }
  .banner-action {
    margin-left: auto;
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-primary);
  }

  .frame {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  :global(body:has(.banner)) .frame {
    height: calc(100% - 38px);
  }
  .shell {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  .rail {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    width: 196px;
    flex: none;
    padding: var(--space-4) var(--space-2) var(--space-3);
    background: var(--surface-1);
    border-right: 1px solid var(--role-border-muted);
  }
  .rail-btn {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: none;
    border-radius: var(--radius-m);
    background: transparent;
    color: var(--role-text-muted);
    text-align: left;
    transition:
      background 120ms ease,
      color 120ms ease;
  }
  .rail-btn:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .rail-btn.active {
    background: var(--surface-2);
    color: var(--role-text-primary);
    box-shadow: inset 3px 0 0 var(--role-accent);
  }
  .glyph {
    width: 20px;
    text-align: center;
    color: var(--role-accent);
    font-size: 16px;
  }
  .rail-label {
    font-size: 14px;
  }
  .rail-spacer {
    flex: 1;
  }

  .mode-switch {
    display: flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    margin: 0 var(--space-2) var(--space-3);
  }
  .mode {
    flex: 1;
    padding: var(--space-1) 0;
    border: none;
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 11px;
  }
  .mode.on {
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-weight: 600;
  }

  .rail-foot {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: 0 var(--space-2);
  }
  .foot-line {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    border: none;
    background: transparent;
    padding: 0;
    font-size: 10.5px;
    color: var(--role-text-subtle);
    text-align: left;
  }
  button.foot-line:hover {
    color: var(--role-text-primary);
  }
  .foot-line.dim {
    cursor: default;
  }
  .demo-chip {
    align-self: flex-start;
    padding: 1px 8px;
    border-radius: var(--radius-pill);
    background: var(--surface-2);
    color: var(--role-warning);
    font-size: 10px;
  }
  .mono {
    font-family: var(--font-mono);
  }
  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: var(--radius-pill);
    flex: none;
  }

  .view {
    flex: 1;
    min-width: 0;
    min-height: 0;
    background: var(--surface-0);
  }

  .toasts {
    position: fixed;
    left: var(--space-4);
    bottom: 92px;
    z-index: 70;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    max-width: 380px;
  }
  .toast {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--role-border-muted);
    border-left: 3px solid var(--role-text-subtle);
    border-radius: var(--radius-m);
    background: var(--surface-2);
    color: var(--role-text-primary);
    font-size: 12.5px;
    text-align: left;
    box-shadow: var(--elev-2);
  }
  .toast.success {
    border-left-color: var(--role-success);
  }
  .toast.error {
    border-left-color: var(--role-error);
  }
  .toast.info {
    border-left-color: var(--role-accent);
  }
</style>
