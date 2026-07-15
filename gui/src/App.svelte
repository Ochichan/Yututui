<script lang="ts">
  // The global frame (docs/gui/07 §0): nav rail · active view · collapsible queue dock,
  // persistent transport bar, connection banner, toast host, overlay hosts (help / about /
  // patch-bay). Keyboard runs the real dispatcher (lib/keyboard/ + the keymap store),
  // honoring the user's remappable keymap.
  import type { AppCtx } from './lib/ctx';
  import { NAV_ITEMS } from './lib/stores/ui.svelte';
  import { i18n, t } from './lib/i18n.svelte';
  import NowPlaying from './views/NowPlaying.svelte';
  import SearchView from './views/SearchView.svelte';
  import LibraryView from './views/LibraryView.svelte';
  import AiView from './views/AiView.svelte';
  import SettingsView from './views/settings/SettingsView.svelte';
  import QueuePanel from './views/QueuePanel.svelte';
  import TransportBar from './views/TransportBar.svelte';
  import HelpOverlay from './views/overlays/HelpOverlay.svelte';
  import AboutCard from './views/overlays/AboutCard.svelte';
  import ChordCapture from './lib/components/ChordCapture.svelte';
  import { chordFromEvent, isTypeableTarget, isPlainTypeable } from './lib/keyboard/chord';
  import { applyTextEditAction, isTextInputTarget } from './lib/keyboard/text-edit';
  import { resolveContext } from './lib/keyboard/dispatcher';
  import { runAction } from './lib/keyboard/actions';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // The ctx bundle is assembled once in main.ts and its identity never changes — the
  // stores inside are the reactive things, so capturing them at init is intended.
  // svelte-ignore state_referenced_locally
  const { ui, connection, playback, settings, toasts, client, boot, demo, keymap } = ctx;

  // Live-switch the whole UI language off the settings model (i18n.catalog).
  $effect(() => {
    i18n.set(settings.ui?.language);
    document.documentElement.lang = i18n.lang;
  });

  const bannerText = $derived.by(() => {
    switch (connection.info.state) {
      case 'connecting':
        return t('conn.connecting');
      case 'degraded':
        return t('conn.reconnecting');
      case 'offline':
        return connection.info.reason === 'no_core' ? t('conn.notRunning') : t('conn.lost');
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

  let persistUiTimer: ReturnType<typeof setTimeout> | null = null;

  function persistUiNow() {
    if (!client || !window.ipc) return;
    if (persistUiTimer) {
      clearTimeout(persistUiTimer);
      persistUiTimer = null;
    }
    client.win('persistUi', ui.snapshot());
  }

  function persistUiSoon() {
    if (!client || !window.ipc) return;
    if (persistUiTimer) clearTimeout(persistUiTimer);
    persistUiTimer = setTimeout(() => {
      persistUiTimer = null;
      persistUiNow();
    }, 100);
  }

  function persistBeforePageHide() {
    playback.flushVolume();
    persistUiNow();
  }

  $effect(() => {
    // Element scroll events do not bubble. Capture scroll and opted-in draft input at the
    // document boundary so the host receives the last small snapshot before idle teardown.
    document.addEventListener('scroll', persistUiSoon, true);
    document.addEventListener('input', persistUiSoon, true);
    window.addEventListener('pagehide', persistBeforePageHide);
    return () => {
      document.removeEventListener('scroll', persistUiSoon, true);
      document.removeEventListener('input', persistUiSoon, true);
      window.removeEventListener('pagehide', persistBeforePageHide);
      if (persistUiTimer) clearTimeout(persistUiTimer);
    };
  });

  $effect(() => {
    // Make all host-restored navigation fields dependencies of this debounced snapshot.
    void ui.view;
    void ui.queueOpen;
    void ui.settingsTab;
    void ui.libraryTab;
    persistUiSoon();
  });

  function switchMode(radio: boolean) {
    if (radio === radioMode) return;
    // Flip the mode via the RadioMode setting change; the library tab set + search default
    // follow player.radio_mode on the next push (docs/gui/07 §16).
    playback.setRadioMode(radio);
  }

  // The real keymap dispatcher (docs/gui/05 §8): normalize the chord (3-branch Korean rule),
  // resolve the focus context, look it up in the user's keymap (specific → common → global),
  // and run the action. The is_typeable guard keeps plain keys flowing into text fields.
  function onkeydown(e: KeyboardEvent) {
    // While a chord is being captured, ChordCapture owns the keyboard.
    if (keymap.capture) return;
    const chord = chordFromEvent(e);
    if (!chord) return;
    const typeable = isTypeableTarget(e.target);
    const textInput = isTextInputTarget(e.target);
    if (textInput) {
      const textEditAction = keymap.textEditMatch(chord);
      if (textEditAction) {
        if (applyTextEditAction(e.target, textEditAction)) e.preventDefault();
        return;
      }
    }
    if (typeable && isPlainTypeable(e)) return;
    const context = resolveContext(ui.view, document.activeElement);
    const action = keymap.match(context, chord);
    if (!action) {
      // GUI-fixed keys, like the TUI's own fixed handlers (not remappable): plain digits
      // jump the rail (the rail buttons advertise them), Esc closes the top overlay.
      if (/^[1-9]$/.test(chord)) {
        const item = NAV_ITEMS[Number(chord) - 1];
        if (item) {
          ui.setView(item.id);
          e.preventDefault();
        }
        return;
      }
      if (chord === 'esc' && ui.closeTopOverlay()) e.preventDefault();
      if (textInput && keymap.isTextEditFactoryChord(chord)) e.preventDefault();
      return;
    }
    if (runAction(action, ctx)) {
      e.preventDefault();
      return;
    }
    if (textInput && keymap.isTextEditFactoryChord(chord)) e.preventDefault();
  }
</script>

<svelte:window {onkeydown} onscroll={persistUiSoon} onfocusin={persistUiSoon} />

<div class="frame">
  {#if connection.info.state !== 'online'}
    <div class="banner" role="status" class:offline={connection.info.state === 'offline'}>
      <span class="dot" style:background={stateColor}></span>
      <span>{bannerText}</span>
      {#if connection.info.state === 'offline'}
        <button class="banner-action" onclick={() => client.win('startDaemon')}
          >{t('conn.startDaemon')}</button
        >
      {/if}
    </div>
  {/if}

  <div class="shell">
    <nav class="rail" aria-label={t('nav.primary')}>
      {#each NAV_ITEMS as item, i (item.id)}
        <button
          class="rail-btn"
          class:active={ui.view === item.id}
          aria-current={ui.view === item.id ? 'page' : undefined}
          title="{t(`nav.${item.id}`)} ({i + 1})"
          onclick={() => ui.setView(item.id)}
        >
          <span class="glyph" aria-hidden="true">{item.glyph}</span>
          <span class="rail-label">{t(`nav.${item.id}`)}</span>
        </button>
      {/each}

      <div class="rail-spacer"></div>

      <div class="mode-switch" role="radiogroup" aria-label={t('mode.group')}>
        <button
          class="mode"
          class:on={!radioMode}
          role="radio"
          aria-checked={!radioMode}
          onclick={() => switchMode(false)}>♪ {t('mode.music')}</button
        >
        <button
          class="mode"
          class:on={radioMode}
          role="radio"
          aria-checked={radioMode}
          onclick={() => switchMode(true)}>📻 {t('mode.radio')}</button
        >
      </div>

      <div class="rail-foot">
        <button class="foot-line" onclick={() => (ui.aboutOpen = true)} title={t('app.about')}>
          <span class="dot" style:background={stateColor}></span>
          <span class="mono">{connection.info.state}</span>
        </button>
        <span class="foot-line mono dim">
          {t('app.core', {
            core: connection.info.coreVersion ?? '—',
            proto: connection.info.protocolVersion ?? boot.protocolVersion,
          })}
        </span>
        {#if demo}
          <span class="demo-chip mono" title={t('app.demoCoreHint')}>
            {t('app.demoCore')}
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
<ChordCapture {keymap} />

<div class="toasts" aria-live="polite">
  {#each toasts.toasts as toast (toast.id)}
    <button
      class="toast {toast.severity}"
      onclick={() => toasts.dismiss(toast.id)}
      title={t('common.dismiss')}>{toast.text}</button
    >
  {/each}
</div>

<style>
  /* The banner lives inside the flex frame, so however tall it wraps (long ko strings at
     narrow widths) the shell shrinks to fit and the transport bar is never pushed off. */
  .banner {
    display: flex;
    flex: none;
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
    gap: 3px;
    padding: 3px;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    margin: 0 var(--space-2) var(--space-4);
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
    padding: 2px 10px;
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
    bottom: calc(var(--transport-h) + var(--space-4));
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
