// Boot: read the injected payload, pick a transport (real shell vs plain browser), apply the
// theme, wire the client + stores, and mount App (docs/gui/05 §2).

import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';
import { readBoot } from './lib/ipc/boot';
import { WryTransport, FakeTransport, type Transport } from './lib/ipc/transport';
import { Client } from './lib/ipc/client';
import { ConnectionStore } from './lib/stores/connection.svelte';
import { UiStore } from './lib/stores/ui.svelte';
import { ThemeStore } from './lib/stores/theme.svelte';

const boot = readBoot();

const transport: Transport = window.ipc ? new WryTransport() : new FakeTransport();
const client = new Client(transport);

// Apply the theme before mount so the first paint is themed; falls back to the app.css
// role defaults when the boot payload carries no theme (M0 injects a static default).
new ThemeStore().apply(boot.theme);

const connection = new ConnectionStore(client);
const ui = new UiStore();

const app = mount(App, {
  target: document.getElementById('app')!,
  props: { boot, client, connection, ui },
});

export default app;
