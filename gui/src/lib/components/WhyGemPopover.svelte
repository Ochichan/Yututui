<script lang="ts">
  // The Why-DJ-Gem popover (docs/gui/07 §13): anchored to a queue row's "why?" affordance,
  // it shows the pick's slot role, reason codes, and model confidence — the GUI echo of the
  // TUI overlay. State + fetch live in stores/whygem.svelte.ts; this is pure presentation.
  import type { WhyGemStore } from '../stores/whygem.svelte';
  import { t } from '../i18n.svelte';

  interface Props {
    whygem: WhyGemStore;
  }
  const { whygem }: Props = $props();

  const pct = $derived(Math.round((whygem.detail?.confidence ?? 0) * 100));
</script>

<svelte:window
  onkeydown={(e) => {
    if (whygem.openId && e.key === 'Escape') {
      e.stopPropagation();
      whygem.close();
    }
  }}
/>

{#if whygem.openId}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="scrim" role="presentation" onclick={() => whygem.close()}></div>
  <div
    class="pop"
    role="dialog"
    aria-label={t('whygem.affordance')}
    style:left="{whygem.anchor?.x ?? 0}px"
    style:top="{whygem.anchor?.y ?? 0}px"
  >
    <header>
      <span class="gem" aria-hidden="true">💎</span>
      <h3>{t('whygem.title')}</h3>
      <button class="x" onclick={() => whygem.close()} aria-label={t('common.close')}>✕</button>
    </header>

    {#if whygem.loading}
      <p class="muted">{t('whygem.reading')}</p>
    {:else if whygem.detail}
      <p class="slot">{whygem.detail.slot}</p>
      {#if whygem.detail.reasons.length}
        <ul class="reasons">
          {#each whygem.detail.reasons as reason}
            <li>{reason}</li>
          {/each}
        </ul>
      {/if}
      <div class="conf" title={t('whygem.confidence')}>
        <span class="conf-label">{t('whygem.confidence')}</span>
        <span class="track"><i style:width="{pct}%"></i></span>
        <span class="mono">{pct}%</span>
      </div>
    {:else}
      <p class="muted">{t('whygem.none')}</p>
    {/if}
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 55;
    background: transparent;
  }
  .pop {
    position: fixed;
    z-index: 56;
    width: 240px;
    /* Anchored to the affordance's bottom-left; grows leftward so it clears the right edge. */
    transform: translate(-100%, var(--space-2));
    background: var(--surface-1);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-m);
    box-shadow: var(--elev-3);
    padding: var(--space-3);
    font-size: 12px;
    animation: pop-in 120ms ease;
  }
  @keyframes pop-in {
    from {
      opacity: 0;
      transform: translate(-100%, calc(var(--space-2) - 4px));
    }
  }
  header {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: var(--space-2);
  }
  .gem {
    font-size: 13px;
  }
  h3 {
    flex: 1;
    margin: 0;
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--role-text-muted);
  }
  .x {
    border: none;
    background: transparent;
    color: var(--role-text-subtle);
    font-size: 12px;
    padding: 0 var(--space-1);
    border-radius: var(--radius-s);
  }
  .x:hover {
    color: var(--role-text-primary);
    background: var(--surface-2);
  }
  .slot {
    margin: 0 0 var(--space-2);
    font-size: 13px;
    font-weight: 600;
    color: var(--role-accent);
  }
  .reasons {
    margin: 0 0 var(--space-3);
    padding-left: var(--space-4);
    color: var(--role-text-muted);
    line-height: 1.5;
  }
  .reasons li {
    margin-bottom: 2px;
  }
  .conf {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .conf-label {
    color: var(--role-text-subtle);
    text-transform: uppercase;
    font-size: 9.5px;
    letter-spacing: 0.06em;
  }
  .track {
    flex: 1;
    height: 5px;
    border-radius: var(--radius-pill);
    background: var(--surface-2);
    overflow: hidden;
  }
  .track i {
    display: block;
    height: 100%;
    background: var(--role-accent);
    border-radius: var(--radius-pill);
  }
  .mono {
    font-family: var(--font-mono);
    color: var(--role-text-muted);
  }
  .muted {
    margin: 0;
    color: var(--role-text-subtle);
    font-style: italic;
  }
</style>
