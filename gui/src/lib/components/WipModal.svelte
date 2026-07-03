<script lang="ts">
  // The Patch Bay modal — what a not-yet-wired feature shows instead of doing nothing.
  // It is honest to users (this is finished UI waiting for its wire) and operational for
  // follow-up agents ("Copy agent brief" produces their exact marching orders, generated
  // from the registry so it can never drift from the spec).
  import { WIRING, agentBrief, marker } from '../wiring/registry';
  import type { WipStore } from '../wiring/wip.svelte';
  import type { Client } from '../ipc/client';
  import type { ToastStore } from '../stores/toasts.svelte';
  import Modal from './Modal.svelte';

  interface Props {
    wip: WipStore;
    client: Client;
    toasts: ToastStore;
    transportLive: boolean;
  }
  const { wip, client, toasts, transportLive }: Props = $props();

  const spec = $derived(wip.active ? WIRING[wip.active] : null);

  async function copyBrief() {
    if (!wip.active) return;
    const text = agentBrief(wip.active);
    try {
      // navigator.clipboard is not a guaranteed secure context under ytm:// on macOS —
      // in the real shell the clipboard rides the win op (docs/gui/05 §4.1).
      if (transportLive) {
        client.win('copyText', { text });
      } else {
        await navigator.clipboard.writeText(text);
      }
      toasts.show('success', 'Agent brief copied — paste it to the next Claude.');
    } catch {
      toasts.show('error', 'Could not reach the clipboard.');
    }
  }
</script>

{#if wip.active && spec}
  <Modal title="Not wired up yet" onclose={() => wip.close()} width="560px">
    <div class="bay">
      <svg viewBox="0 0 300 84" aria-hidden="true">
        <!-- left jack: the UI side, powered -->
        <circle
          cx="36"
          cy="34"
          r="13"
          fill="none"
          stroke="var(--role-border-primary)"
          stroke-width="2"
        />
        <circle cx="36" cy="34" r="6" fill="var(--role-accent)">
          <animate attributeName="opacity" values="1;0.5;1" dur="2s" repeatCount="indefinite" />
        </circle>
        <!-- right jack: the core side, dark -->
        <circle
          cx="264"
          cy="34"
          r="13"
          fill="none"
          stroke="var(--role-border-primary)"
          stroke-width="2"
        />
        <circle cx="264" cy="34" r="6" fill="var(--role-gauge-empty)" />
        <!-- the cable: plugged in on the left, dangling short of the right jack -->
        <path
          d="M 49 34 C 110 34 150 30 186 52 C 200 61 210 64 219 60"
          fill="none"
          stroke="var(--role-accent)"
          stroke-width="3"
          stroke-linecap="round"
          stroke-dasharray="7 5"
        >
          <animate
            attributeName="stroke-dashoffset"
            from="24"
            to="0"
            dur="1.6s"
            repeatCount="indefinite"
          />
        </path>
        <!-- the loose plug -->
        <rect
          x="219"
          y="54"
          width="14"
          height="10"
          rx="2.5"
          fill="var(--role-accent)"
          transform="rotate(-18 226 59)"
        />
        <text x="36" y="70" text-anchor="middle" class="jack-label">UI · ready</text>
        <text x="264" y="70" text-anchor="middle" class="jack-label">core · {spec.milestone}</text>
      </svg>
      <p class="cat mono" aria-hidden="true">=^..^=&nbsp; &lt; the wire naps here for now</p>
    </div>

    <h3>{spec.title}</h3>
    <p class="lede">
      This surface is finished UI waiting on its wire, which lands in
      <strong>{spec.milestone}</strong>. A follow-up agent picks it up from here — the button below
      copies its exact marching orders.
    </p>

    <dl>
      <dt>Spec</dt>
      <dd class="mono">{spec.brief}</dd>
      <dt>Protocol</dt>
      <dd class="mono">{spec.protocol}</dd>
      <dt>Seam</dt>
      <dd class="mono">{spec.seam}</dd>
      <dt>Call sites</dt>
      <dd class="mono">rg "{wip.active ? marker(wip.active) : ''}" gui/src</dd>
      {#if spec.notes}
        <dt>Notes</dt>
        <dd>{spec.notes}</dd>
      {/if}
    </dl>

    <div class="row">
      <button class="primary" onclick={copyBrief}>Copy agent brief</button>
      <button class="ghost" onclick={() => wip.close()}>OK</button>
    </div>
  </Modal>
{/if}

<style>
  .bay {
    margin-bottom: var(--space-4);
    padding: var(--space-3) var(--space-3) var(--space-2);
    border: 1px dashed var(--role-border-primary);
    border-radius: var(--radius-m);
    background: var(--surface-0);
    text-align: center;
  }
  svg {
    width: 100%;
    max-width: 320px;
    height: auto;
  }
  .jack-label {
    fill: var(--role-text-subtle);
    font-family: var(--font-mono);
    font-size: 9px;
  }
  .cat {
    margin: var(--space-1) 0 0;
    color: var(--role-text-subtle);
    font-size: 11px;
  }
  .mono {
    font-family: var(--font-mono);
  }
  h3 {
    margin: 0 0 var(--space-1);
    font-size: 15px;
  }
  .lede {
    margin: 0 0 var(--space-4);
    font-size: 12.5px;
    line-height: 1.55;
    color: var(--role-text-muted);
  }
  dl {
    display: grid;
    grid-template-columns: 76px 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0 0 var(--space-6);
    font-size: 11.5px;
  }
  dt {
    color: var(--role-settings-label);
  }
  dd {
    margin: 0;
    color: var(--role-settings-value);
    overflow-wrap: anywhere;
  }
  .row {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
  }
  .primary {
    padding: var(--space-2) var(--space-4);
    border: none;
    border-radius: var(--radius-pill);
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-weight: 600;
    font-size: 12.5px;
  }
  .primary:hover {
    background: var(--role-accent-alt);
  }
  .ghost {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-pill);
    background: transparent;
    font-size: 12.5px;
  }
  .ghost:hover {
    background: var(--surface-2);
  }
</style>
