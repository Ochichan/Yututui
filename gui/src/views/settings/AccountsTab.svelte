<script lang="ts">
  // Settings → Accounts (docs/gui/07 §11, docs/gui/02 §13.4). Connect flows push `accounts`
  // events and the GUI opens the browser (win:openUrl); the transfer wizard rides the
  // `transfer` topic. Wired via stores/accounts.svelte.ts + stores/transfer.svelte.ts —
  // was the settings.accounts + transfer.wizard patch-bay seams.
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';
  import SpotifyImport from '../wizards/SpotifyImport.svelte';
  import { t } from '../../lib/i18n.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores are reactive
  const { accounts, transfer } = ctx;

  const acc = $derived(accounts.model);

  let wizardOpen = $state(false);
  function openWizard() {
    transfer.reset();
    transfer.listSpotify();
    wizardOpen = true;
  }
</script>

<SettingSection title="Last.fm">
  <SettingRow label={t('settings.accounts.scrobbling')}>
    <Toggle
      checked={acc.lastfm.scrobbling}
      disabled={!acc.lastfm.connected}
      onchange={(v) => accounts.setLastfmScrobbling(v)}
    />
  </SettingRow>
  <SettingRow
    label={t('settings.accounts.account')}
    hint={t('settings.accounts.lastfmAccountHint')}
  >
    {#if acc.lastfm.connected}
      <span class="pill on"
        >{t('settings.accounts.connected')}{acc.lastfm.user ? ` · ${acc.lastfm.user}` : ''}</span
      >
    {:else}
      <span class="pill off">{t('settings.accounts.notConnected')}</span>
      <button class="connect" onclick={() => accounts.connectLastfm()}
        >{t('settings.accounts.connect')}</button
      >
    {/if}
  </SettingRow>
  <SettingRow label={t('settings.accounts.loveSync')} hint={t('settings.accounts.loveSyncHint')}>
    <Toggle
      checked={acc.lastfm.love_sync}
      disabled={!acc.lastfm.connected}
      onchange={(v) => accounts.setLastfmLoveSync(v)}
    />
  </SettingRow>
</SettingSection>

<SettingSection title="ListenBrainz">
  <SettingRow label={t('settings.accounts.submitListens')}>
    <Toggle
      checked={acc.listenbrainz.submit}
      onchange={(v) => accounts.configureListenBrainz({ submit: v })}
    />
  </SettingRow>
  <SettingRow label={t('settings.accounts.token')} hint={t('settings.accounts.tokenHint')}>
    <input
      class="ti"
      type="password"
      placeholder={acc.listenbrainz.has_token
        ? t('settings.accounts.tokenSet')
        : t('settings.accounts.tokenPlaceholder')}
      size="18"
      onchange={(e) => accounts.configureListenBrainz({ token: e.currentTarget.value })}
    />
  </SettingRow>
  <SettingRow label={t('settings.accounts.customUrl')} hint={t('settings.accounts.customUrlHint')}>
    <input
      class="ti"
      placeholder="https://api.listenbrainz.org"
      size="24"
      value={acc.listenbrainz.custom_url ?? ''}
      onchange={(e) => accounts.configureListenBrainz({ custom_url: e.currentTarget.value })}
    />
  </SettingRow>
</SettingSection>

<SettingSection title="Spotify">
  <SettingRow label={t('settings.accounts.clientId')} hint={t('settings.accounts.clientIdHint')}>
    <input
      class="ti"
      placeholder={t('settings.accounts.clientIdPlaceholder')}
      size="20"
      value={acc.spotify.client_id ?? ''}
      onchange={(e) => accounts.setSpotifyClientId(e.currentTarget.value)}
    />
  </SettingRow>
  <SettingRow label={t('settings.accounts.redirectPort')}>
    <input
      class="ti"
      type="number"
      placeholder="8888"
      size="6"
      value={acc.spotify.redirect_port ?? ''}
      onchange={(e) => accounts.setSpotifyRedirectPort(Number(e.currentTarget.value))}
    />
  </SettingRow>
  <SettingRow label={t('settings.accounts.account')}>
    {#if acc.spotify.connected}
      <span class="pill on"
        >{t('settings.accounts.connected')}{acc.spotify.user ? ` · ${acc.spotify.user}` : ''}</span
      >
    {:else}
      <span class="pill off">{t('settings.accounts.notConnected')}</span>
      <button class="connect" onclick={() => accounts.connectSpotify()}
        >{t('settings.accounts.connect')}</button
      >
    {/if}
  </SettingRow>
  <SettingRow
    label={t('settings.accounts.importPlaylists')}
    hint={t('settings.accounts.importPlaylistsHint')}
  >
    <button class="connect" onclick={openWizard}>{t('settings.accounts.import')}</button>
  </SettingRow>
</SettingSection>

<SettingSection title={t('settings.accounts.scrobbleScope')}>
  <SettingRow label={t('settings.accounts.scrobbleLocalFiles')}>
    <Toggle checked={acc.scrobble_local} onchange={(v) => accounts.setScrobbleLocal(v)} />
  </SettingRow>
</SettingSection>

{#if wizardOpen}
  <SpotifyImport {ctx} onclose={() => (wizardOpen = false)} />
{/if}

<style>
  .pill {
    padding: 2px 10px;
    border-radius: var(--radius-pill);
    font-size: 11px;
    font-weight: 600;
  }
  .pill.off {
    background: var(--surface-2);
    color: var(--role-text-subtle);
  }
  .pill.on {
    background: color-mix(in oklab, var(--role-success) 22%, transparent);
    color: var(--role-success);
  }
  .connect {
    padding: var(--space-1) var(--space-4);
    border: 1px solid var(--role-border-primary);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-primary);
    font-size: 12px;
  }
  .connect:hover {
    background: var(--surface-2);
  }
</style>
