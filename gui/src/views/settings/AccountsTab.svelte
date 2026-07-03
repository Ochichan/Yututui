<script lang="ts">
  // Settings → Accounts (docs/gui/07 §11). Connect flows push `accounts` events and the
  // GUI opens the browser (win:openUrl) — all gated through the patch bay until M4.
  //
  // TODO(wire:M4/settings.accounts): lastfm_connect / spotify_connect / listen_brainz_configure.
  import type { AppCtx } from '../../lib/ctx';
  import SettingSection from './SettingSection.svelte';
  import SettingRow from './SettingRow.svelte';
  import Toggle from '../../lib/components/Toggle.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip } = ctx;

  const stub = () => wip.gate('settings.accounts');
</script>

<SettingSection title="Last.fm">
  <SettingRow label="Scrobbling">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow label="Account" hint="Browser approval flow; status updates push live">
    <span class="pill off">Not connected</span>
    <button class="connect" onclick={stub}>Connect…</button>
  </SettingRow>
  <SettingRow label="Love sync" hint="♥ in the library also loves the track on Last.fm">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
</SettingSection>

<SettingSection title="ListenBrainz">
  <SettingRow label="Submit listens">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
  <SettingRow label="Token" hint="Write-only, like every secret on the wire">
    <input class="ti" type="password" placeholder="token" size="18" onchange={stub} />
  </SettingRow>
  <SettingRow label="Custom URL" hint="Self-hosted instances">
    <input class="ti" placeholder="https://api.listenbrainz.org" size="24" onchange={stub} />
  </SettingRow>
</SettingSection>

<SettingSection title="Spotify">
  <SettingRow label="Client ID" hint="Your own Spotify app (dev mode) — PKCE, no secret">
    <input class="ti" placeholder="client id" size="20" onchange={stub} />
  </SettingRow>
  <SettingRow label="Redirect port">
    <input class="ti" placeholder="8888" size="6" onchange={stub} />
  </SettingRow>
  <SettingRow label="Account">
    <span class="pill off">Not connected</span>
    <button class="connect" onclick={stub}>Connect…</button>
  </SettingRow>
  <SettingRow
    label="Import playlists"
    hint="The transfer wizard: pick playlists, choose a destination, watch the match report"
  >
    <button class="connect" onclick={() => wip.gate('transfer.wizard')}>Import…</button>
  </SettingRow>
</SettingSection>

<SettingSection title="Scrobble scope">
  <SettingRow label="Scrobble local files">
    <Toggle checked={false} onchange={stub} />
  </SettingRow>
</SettingSection>

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
