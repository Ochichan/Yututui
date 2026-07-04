<script lang="ts">
  // Spotify import wizard (docs/gui/07 §14): a Modal-hosted, phase-driven flow — list the
  // user's Spotify playlists, pick sources + a destination, then watch a coalesced-progress
  // import end in a match report. Driven entirely by the transfer store's pushed state
  // machine (stores/transfer.svelte.ts). The destination offers an existing YTM playlist
  // (from ctx.playlists.list) as the mainline path, since dev-mode Spotify apps 403 on
  // creation. Wired — was the transfer.wizard patch-bay seam.
  import type { AppCtx } from '../../lib/ctx';
  import type { TransferDest } from '../../lib/stores/transfer.svelte';
  import Modal from '../../lib/components/Modal.svelte';

  interface Props {
    ctx: AppCtx;
    onclose: () => void;
  }
  const { ctx, onclose }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores are reactive
  const { transfer, playlists } = ctx;

  const xfer = $derived(transfer.state);

  // Selection + destination are local wizard state until Start builds the spec.
  let picked = $state<Set<string>>(new Set());
  let destKind = $state<'new' | 'existing'>('new');
  let newName = $state('Spotify import');
  let existingId = $state('');

  function toggle(id: string) {
    const next = new Set(picked);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    picked = next;
  }

  const destReady = $derived(
    destKind === 'new' ? newName.trim().length > 0 : existingId.length > 0,
  );
  const canStart = $derived(picked.size > 0 && destReady);

  function start() {
    if (!canStart) return;
    const dest: TransferDest =
      destKind === 'new'
        ? { kind: 'new', name: newName.trim() }
        : { kind: 'existing', playlist_id: existingId };
    transfer.start({ source_ids: [...picked], dest });
  }

  function close() {
    transfer.reset();
    onclose();
  }

  const pct = $derived(
    xfer.job && xfer.job.total > 0 ? Math.round((xfer.job.done / xfer.job.total) * 100) : 0,
  );
</script>

<Modal title="Import from Spotify" width="560px" onclose={close}>
  {#if xfer.phase === 'idle' || xfer.phase === 'listing'}
    <div class="step center">
      {#if xfer.phase === 'listing'}
        <p class="hint">Fetching your Spotify playlists…</p>
      {:else}
        <p class="hint">Connect to Spotify and list your playlists to import.</p>
        <button class="btn primary" onclick={() => transfer.listSpotify()}>Connect Spotify</button>
      {/if}
    </div>
  {:else if xfer.phase === 'ready'}
    <div class="step">
      <p class="lbl">1 · Pick playlists to import</p>
      <div class="sources" role="group" aria-label="Spotify playlists">
        {#each xfer.sources as s (s.id)}
          <label class="src">
            <input type="checkbox" checked={picked.has(s.id)} onchange={() => toggle(s.id)} />
            <span class="sname">{s.name}</span>
            <span class="scount mono">{s.count}</span>
          </label>
        {/each}
      </div>

      <p class="lbl">2 · Destination</p>
      <div class="dest">
        <label class="dopt">
          <input type="radio" name="dest" value="new" bind:group={destKind} />
          <span>New playlist</span>
          <input
            class="ti"
            bind:value={newName}
            disabled={destKind !== 'new'}
            placeholder="name"
            size="18"
          />
        </label>
        <label class="dopt">
          <input
            type="radio"
            name="dest"
            value="existing"
            bind:group={destKind}
            disabled={playlists.list.length === 0}
          />
          <span>Append to existing</span>
          <select class="ti" bind:value={existingId} disabled={destKind !== 'existing'}>
            <option value="" disabled>choose…</option>
            {#each playlists.list as p (p.id)}
              <option value={p.id}>{p.name} ({p.count})</option>
            {/each}
          </select>
        </label>
        {#if playlists.list.length === 0}
          <p class="note">No local playlists yet — create one to append, or import to a new one.</p>
        {/if}
      </div>

      <div class="frow">
        <button class="btn" onclick={close}>Cancel</button>
        <button class="btn primary" disabled={!canStart} onclick={start}>Import</button>
      </div>
    </div>
  {:else if xfer.phase === 'running'}
    <div class="step">
      <p class="lbl">Importing…</p>
      <div class="bar"><span class="fill" style:width="{pct}%"></span></div>
      <p class="prog mono">
        {xfer.job?.done ?? 0} / {xfer.job?.total ?? 0} · matched {xfer.job?.matched ?? 0} · failed
        {xfer.job?.failed ?? 0}
      </p>
      <div class="frow">
        <button class="btn danger" onclick={() => transfer.cancel()}>Cancel</button>
      </div>
    </div>
  {:else if xfer.phase === 'done'}
    <div class="step">
      <p class="lbl">Done — imported to {xfer.report?.dest ?? 'your library'}</p>
      <div class="report">
        <span class="stat ok mono">✓ {xfer.report?.matched ?? 0} matched</span>
        <span class="stat warn mono">⚠ {xfer.report?.failed ?? 0} unmatched</span>
        {#if (xfer.report?.skipped ?? 0) > 0}
          <span class="stat mono">↷ {xfer.report?.skipped} skipped</span>
        {/if}
      </div>
      {#if xfer.report && xfer.report.unmatched.length > 0}
        <details class="unmatched">
          <summary>Couldn't match {xfer.report.unmatched.length}</summary>
          <ul>
            {#each xfer.report.unmatched as u (u)}
              <li>{u}</li>
            {/each}
          </ul>
        </details>
      {/if}
      <div class="frow">
        <button class="btn" onclick={() => transfer.reset()}>Import more</button>
        <button class="btn primary" onclick={close}>Done</button>
      </div>
    </div>
  {:else}
    <div class="step center">
      <p class="hint err">{xfer.error ?? 'The import failed.'}</p>
      <button class="btn" onclick={() => transfer.listSpotify()}>Try again</button>
    </div>
  {/if}
</Modal>

<style>
  .step {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .center {
    align-items: center;
    text-align: center;
    padding: var(--space-4) 0;
  }
  .lbl {
    margin: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--role-accent);
  }
  .hint {
    margin: 0;
    font-size: 13px;
    color: var(--role-text-muted);
    line-height: 1.55;
  }
  .hint.err {
    color: var(--role-error);
  }
  .sources {
    display: flex;
    flex-direction: column;
    max-height: 200px;
    overflow-y: auto;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-m);
  }
  .src {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--role-border-muted);
    font-size: 13px;
  }
  .src:last-child {
    border-bottom: none;
  }
  .sname {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .scount {
    font-size: 11px;
    color: var(--role-text-subtle);
  }
  .dest {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .dopt {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 13px;
  }
  .dopt span {
    min-width: 128px;
  }
  .note {
    margin: 0;
    font-size: 11.5px;
    color: var(--role-text-subtle);
  }
  .bar {
    height: 8px;
    border-radius: var(--radius-pill);
    background: var(--surface-2);
    overflow: hidden;
  }
  .fill {
    display: block;
    height: 100%;
    background: var(--role-accent);
    transition: width 160ms ease;
  }
  .prog {
    margin: 0;
    font-size: 11.5px;
    color: var(--role-text-subtle);
  }
  .report {
    display: flex;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .stat {
    font-size: 12px;
    color: var(--role-text-muted);
  }
  .stat.ok {
    color: var(--role-success);
  }
  .stat.warn {
    color: var(--role-warning);
  }
  .unmatched {
    font-size: 12px;
    color: var(--role-text-muted);
  }
  .unmatched summary {
    cursor: pointer;
  }
  .unmatched ul {
    margin: var(--space-2) 0 0;
    padding-left: var(--space-5);
  }
  .frow {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-2);
  }
  .btn {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .btn:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .btn:disabled {
    opacity: 0.4;
  }
  .btn.primary {
    border-color: var(--role-accent);
    color: var(--role-accent);
  }
  .btn.danger {
    border-color: var(--role-error);
    color: var(--role-error);
  }
  .ti {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-s);
    background: var(--surface-0);
    color: var(--role-text-primary);
    font-size: 12px;
  }
  .mono {
    font-family: var(--font-mono);
  }
</style>
