<script lang="ts">
  // About (docs/gui/07 §18): identity, versions, license, links (OS browser via win op),
  // plus the M0 IPC self-test relocated here as a diagnostics line.
  import type { AppCtx } from '../../lib/ctx';
  import Modal from '../../lib/components/Modal.svelte';
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ui, boot, connection, client, demo } = ctx;

  let ping = $state<'idle' | 'pending' | string>('idle');
  async function selfTest() {
    ping = 'pending';
    try {
      ping = String(await client.req<string>('ping'));
    } catch (e) {
      ping = `error: ${(e as Error).message}`;
    }
  }

  function openRepo() {
    const url = 'https://github.com/Ochichan/Yututui';
    if (demo) window.open(url, '_blank');
    else client.win('openUrl', { url });
  }
</script>

<Modal onclose={() => (ui.aboutOpen = false)} width="420px">
  <div class="about">
    <p class="kaomoji mono" aria-hidden="true">=^..^=</p>
    <h2>YuTuTui!</h2>
    <p class="tag">{t('about.tagline')}</p>
    <dl class="mono">
      <dt>{t('about.desktop')}</dt>
      <dd>v{boot.version} ({boot.platform})</dd>
      <dt>{t('about.core')}</dt>
      <dd>{connection.info.coreVersion ?? '—'}</dd>
      <dt>{t('about.protocol')}</dt>
      <dd>v{connection.info.protocolVersion ?? boot.protocolVersion}</dd>
      <dt>{t('about.owner')}</dt>
      <dd>{connection.info.ownerMode ?? '—'}</dd>
      <dt>{t('about.ipc')}</dt>
      <dd>
        <button class="ping" onclick={selfTest}
          >{ping === 'idle' ? t('about.selfTest') : ping}</button
        >
      </dd>
    </dl>
    <div class="links">
      <button class="link" onclick={openRepo}>GitHub</button>
      <span class="lic">{t('about.license')}</span>
    </div>
  </div>
</Modal>

<style>
  .about {
    text-align: center;
  }
  .kaomoji {
    margin: 0;
    font-size: 30px;
    color: var(--role-accent);
  }
  h2 {
    margin: var(--space-2) 0 var(--space-1);
    font-size: 20px;
  }
  .tag {
    margin: 0 0 var(--space-6);
    font-size: 12px;
    color: var(--role-text-muted);
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: var(--space-1) var(--space-4);
    margin: 0 auto var(--space-6);
    max-width: 280px;
    font-size: 11.5px;
    text-align: left;
  }
  dt {
    color: var(--role-text-subtle);
  }
  dd {
    margin: 0;
    color: var(--role-text-primary);
  }
  .mono {
    font-family: var(--font-mono);
  }
  .ping {
    border: none;
    background: transparent;
    color: var(--role-accent);
    font-family: var(--font-mono);
    font-size: 11.5px;
    padding: 0;
    text-decoration: underline dotted;
  }
  .links {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: var(--space-4);
  }
  .link {
    border: none;
    background: transparent;
    color: var(--role-accent);
    text-decoration: underline;
    font-size: 12.5px;
    padding: 0;
  }
  .lic {
    font-size: 11.5px;
    color: var(--role-text-subtle);
  }
</style>
