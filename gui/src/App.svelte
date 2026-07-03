<script lang="ts">
  import type { BootPayload } from './lib/ipc/boot';
  import type { Client } from './lib/ipc/client';
  import type { ConnectionStore } from './lib/stores/connection.svelte';
  import { NAV_ITEMS, type UiStore } from './lib/stores/ui.svelte';

  interface Props {
    boot: BootPayload;
    client: Client;
    connection: ConnectionStore;
    ui: UiStore;
  }
  const { boot, client, connection, ui }: Props = $props();

  // IPC self-test: prove the bridge echo (req ping → res pong) end to end (docs/gui/09 §3).
  let ping = $state<'idle' | 'pending' | string>('idle');
  async function selfTest() {
    ping = 'pending';
    try {
      const pong = await client.req<string>('ping');
      ping = String(pong);
    } catch (e) {
      ping = `error: ${(e as Error).message}`;
    }
  }
  $effect(() => {
    // Re-run the ping whenever we (re)connect, so the footer reflects a live round-trip.
    if (connection.info.state === 'online') void selfTest();
  });

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
</script>

{#if connection.info.state !== 'online'}
  <div class="banner" role="status" class:offline={connection.info.state === 'offline'}>
    <span class="dot" style:background={stateColor}></span>
    <span>{bannerText}</span>
    {#if connection.info.state === 'offline'}
      <button class="banner-action" onclick={() => client.win('startDaemon')}>Start daemon</button>
    {/if}
  </div>
{/if}

<div class="shell">
  <nav class="rail" aria-label="Primary">
    {#each NAV_ITEMS as item (item.id)}
      <button
        class="rail-btn"
        class:active={ui.view === item.id}
        aria-current={ui.view === item.id ? 'page' : undefined}
        title={item.label}
        onclick={() => ui.setView(item.id)}
      >
        <span class="glyph" aria-hidden="true">{item.glyph}</span>
        <span class="rail-label">{item.label}</span>
      </button>
    {/each}
  </nav>

  <main class="view">
    <header class="view-head">
      <h1>{NAV_ITEMS.find((n) => n.id === ui.view)?.label}</h1>
    </header>
    <section class="view-body">
      <div class="placeholder">
        <p class="ph-title">Walking skeleton</p>
        <p class="ph-sub">
          This screen lands in a later milestone. The shell, theme, IPC bridge, and live
          connection state are the M0 deliverable.
        </p>
      </div>
    </section>
  </main>
</div>

<footer class="statusbar">
  <span class="s-item">
    <span class="dot" style:background={stateColor}></span>
    {connection.info.state}
  </span>
  <span class="s-item">core {connection.info.coreVersion ?? '—'}</span>
  <span class="s-item">v{connection.info.protocolVersion ?? boot.protocolVersion}</span>
  <span class="s-item">owner {connection.info.ownerMode ?? '—'}</span>
  <span class="s-spacer"></span>
  <button class="s-item s-btn" onclick={selfTest} title="Round-trip an IPC ping">
    ipc: {ping}
  </button>
  <span class="s-item">{boot.platform} · ytt-desktop {boot.version}</span>
</footer>

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
    background: var(--surface-2);
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

  .shell {
    display: grid;
    grid-template-columns: 208px 1fr;
    height: calc(100vh - 34px);
  }

  .rail {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-4) var(--space-2);
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
    transition: background 120ms ease, color 120ms ease;
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

  .view {
    display: flex;
    flex-direction: column;
    min-width: 0;
    background: var(--surface-0);
  }
  .view-head {
    padding: var(--space-6) var(--space-8) var(--space-3);
  }
  .view-head h1 {
    margin: 0;
    font-size: 22px;
    font-weight: 600;
  }
  .view-body {
    flex: 1;
    display: grid;
    place-items: center;
    padding: var(--space-8);
  }
  .placeholder {
    max-width: 420px;
    text-align: center;
    padding: var(--space-8);
    border: 1px dashed var(--role-border-primary);
    border-radius: var(--radius-l);
    background: var(--surface-1);
  }
  .ph-title {
    margin: 0 0 var(--space-2);
    font-size: 16px;
    font-weight: 600;
    color: var(--role-accent);
  }
  .ph-sub {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--role-text-muted);
  }

  .statusbar {
    display: flex;
    align-items: center;
    gap: var(--space-4);
    height: 34px;
    padding: 0 var(--space-4);
    background: var(--surface-1);
    border-top: 1px solid var(--role-border-muted);
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .s-spacer {
    flex: 1;
  }
  .s-btn {
    border: none;
    background: transparent;
    color: var(--role-text-subtle);
  }
  .s-btn:hover {
    color: var(--role-text-primary);
  }
  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: var(--radius-pill);
  }
</style>
