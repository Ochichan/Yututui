// Boot: read the injected payload, pick a transport (real shell vs the in-page demo
// core), apply the theme, assemble the store bundle, and mount App (docs/gui/05 §2).

import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';
import { createAppCtx } from './lib/app';
import { readBoot } from './lib/ipc/boot';
import { WryTransport, type Transport } from './lib/ipc/transport';
import { DemoCoreTransport } from './lib/dev/democore';

const boot = readBoot();

// Real shell when wry injected window.ipc; otherwise the demo core keeps the whole UI
// alive and interactive in a plain browser (docs/gui/05 §4.3).
const transport: Transport = window.ipc ? new WryTransport() : new DemoCoreTransport();
const ctx = createAppCtx(boot, transport);

const app = mount(App, {
  target: document.getElementById('app')!,
  props: { ctx },
});

requestAnimationFrame(() => ctx.ui.restoreDocument(boot.uiState));

export default app;
