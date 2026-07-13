<script lang="ts">
  // Spotify import wizard (docs/gui/07 §14): a Modal-hosted, phase-driven flow — list the
  // user's Spotify playlists, pick sources + a destination, then watch a coalesced-progress
  // import end in a match report. Driven entirely by the transfer store's pushed state
  // machine (stores/transfer.svelte.ts). The destination offers an existing YTM playlist
  // (from ctx.playlists.list) as the mainline path, since dev-mode Spotify apps 403 on
  // creation. Wired — was the transfer.wizard patch-bay seam.
  import type { AppCtx } from '../../lib/ctx';
  import type { TransferDest, TransferReport } from '../../lib/stores/transfer.svelte';
  import Modal from '../../lib/components/Modal.svelte';
  import { t } from '../../lib/i18n.svelte';

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

  interface FollowUpStep {
    label: string;
    command: string;
  }

  function followUpSteps(report: TransferReport | null): FollowUpStep[] {
    if (!report) return [];
    return [
      report.review_command && {
        label: t('transfer.reviewCommand'),
        command: report.review_command,
      },
      report.report_command && {
        label: t('transfer.reportCommand'),
        command: report.report_command,
      },
      report.download_preview_command && {
        label: t('transfer.downloadPreviewCommand'),
        command: report.download_preview_command,
      },
      report.organize_preview_command && {
        label: t('transfer.organizePreviewCommand'),
        command: report.organize_preview_command,
      },
    ].filter((step): step is FollowUpStep => Boolean(step));
  }

  const followUps = $derived(followUpSteps(xfer.report));
</script>

<Modal title={t('transfer.title')} width="560px" onclose={close}>
  {#if xfer.phase === 'idle' || xfer.phase === 'listing'}
    <div class="step center">
      {#if xfer.phase === 'listing'}
        <p class="hint">{t('transfer.fetching')}</p>
      {:else}
        <p class="hint">{t('transfer.connectHint')}</p>
        <button class="btn primary" onclick={() => transfer.listSpotify()}
          >{t('transfer.connectSpotify')}</button
        >
      {/if}
    </div>
  {:else if xfer.phase === 'ready'}
    <div class="step">
      <p class="lbl">1 · {t('transfer.step1')}</p>
      <div class="sources" role="group" aria-label={t('transfer.sourcesLabel')}>
        {#each xfer.sources as s (s.id)}
          <label class="src">
            <input type="checkbox" checked={picked.has(s.id)} onchange={() => toggle(s.id)} />
            <span class="sname">{s.name}</span>
            <span class="scount mono">{s.count}</span>
          </label>
        {/each}
      </div>

      <p class="lbl">2 · {t('transfer.step2')}</p>
      <div class="dest">
        <label class="dopt">
          <input type="radio" name="dest" value="new" bind:group={destKind} />
          <span>{t('transfer.newPlaylist')}</span>
          <input
            class="ti"
            bind:value={newName}
            disabled={destKind !== 'new'}
            placeholder={t('transfer.namePlaceholder')}
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
          <span>{t('transfer.appendExisting')}</span>
          <select class="ti" bind:value={existingId} disabled={destKind !== 'existing'}>
            <option value="" disabled>{t('transfer.choose')}</option>
            {#each playlists.list as p (p.id)}
              <option value={p.id}>{p.name} ({p.count})</option>
            {/each}
          </select>
        </label>
        {#if playlists.list.length === 0}
          <p class="note">{t('transfer.noLocalPlaylists')}</p>
        {/if}
      </div>

      <div class="frow">
        <button class="btn" onclick={close}>{t('common.cancel')}</button>
        <button class="btn primary" disabled={!canStart} onclick={start}
          >{t('transfer.import')}</button
        >
      </div>
    </div>
  {:else if xfer.phase === 'running'}
    <div class="step">
      <p class="lbl">{t('transfer.importing')}</p>
      <div class="bar"><span class="fill" style:width="{pct}%"></span></div>
      <p class="prog mono">
        {t('transfer.progress', {
          done: xfer.job?.done ?? 0,
          total: xfer.job?.total ?? 0,
          matched: xfer.job?.matched ?? 0,
          failed: xfer.job?.failed ?? 0,
        })}
      </p>
      <div class="frow">
        <button class="btn danger" onclick={() => transfer.cancel()}>{t('common.cancel')}</button>
      </div>
    </div>
  {:else if xfer.phase === 'done'}
    <div class="step">
      <p class="lbl">
        {t('transfer.doneImportedTo', { dest: xfer.report?.dest ?? t('transfer.yourLibrary') })}
      </p>
      <div class="report">
        <span class="stat ok mono"
          >✓ {t('transfer.matched', { count: xfer.report?.matched ?? 0 })}</span
        >
        <span class="stat warn mono"
          >⚠ {t('transfer.unmatched', { count: xfer.report?.failed ?? 0 })}</span
        >
        {#if (xfer.report?.skipped ?? 0) > 0}
          <span class="stat mono"
            >↷ {t('transfer.skipped', { count: xfer.report?.skipped ?? 0 })}</span
          >
        {/if}
      </div>
      {#if xfer.report && xfer.report.unmatched.length > 0}
        <details class="unmatched">
          <summary>{t('transfer.couldntMatch', { count: xfer.report.unmatched.length })}</summary>
          <ul>
            {#each xfer.report.unmatched as u (u)}
              <li>{u}</li>
            {/each}
          </ul>
        </details>
      {/if}
      {#if followUps.length > 0 || xfer.report?.local_deck_hint}
        <div class="follow">
          <p class="lbl">3 · {t('transfer.nextSteps')}</p>
          {#if followUps.length > 0}
            <ul>
              {#each followUps as step (step.command)}
                <li>
                  <span>{step.label}</span>
                  <code>{step.command}</code>
                </li>
              {/each}
            </ul>
          {/if}
          {#if xfer.report?.local_deck_hint}
            <p class="note">{xfer.report.local_deck_hint}</p>
          {/if}
        </div>
      {/if}
      <div class="frow">
        <button class="btn" onclick={() => transfer.reset()}>{t('transfer.importMore')}</button>
        <button class="btn primary" onclick={close}>{t('transfer.done')}</button>
      </div>
    </div>
  {:else}
    <div class="step center">
      <p class="hint err">{xfer.error ?? t('transfer.failed')}</p>
      <button class="btn" onclick={() => transfer.listSpotify()}>{t('common.retry')}</button>
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
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    font-size: 13px;
  }
  .dopt span {
    flex: none;
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
  .follow {
    border-top: 1px solid var(--role-border-muted);
    padding-top: var(--space-3);
  }
  .follow ul {
    display: grid;
    gap: var(--space-2);
    margin: var(--space-2) 0;
    padding: 0;
    list-style: none;
  }
  .follow li {
    display: grid;
    grid-template-columns: minmax(120px, max-content) minmax(0, 1fr);
    gap: var(--space-2);
    align-items: baseline;
    font-size: 12px;
    color: var(--role-text-muted);
  }
  .follow code {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-s);
    padding: 2px 6px;
    background: var(--surface-2);
    color: var(--role-text-primary);
    white-space: nowrap;
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
