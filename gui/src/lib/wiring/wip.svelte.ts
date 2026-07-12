// The gate in front of every not-yet-wired feature. Views call `wip.open(id)` (tagged
// TODO(wire:…) at the call site); App.svelte hosts the WipModal that renders the active
// entry. When the connected core advertises the feature's capability the gate reports
// `wired` and callers take the real path instead — stubs dissolve without a release.

import { WIRING, type FeatureId } from './registry';
import type { ConnectionStore } from '../stores/connection.svelte';

export class WipStore {
  active = $state<FeatureId | null>(null);
  readonly #connection: ConnectionStore;
  readonly #demo: boolean;

  constructor(connection: ConnectionStore, demo = false) {
    this.#connection = connection;
    this.#demo = demo;
  }

  /**
   * True once the core advertises the feature's capability (auto-dissolve). The in-page
   * demo core genuinely implements every gated surface, so demo mode is always wired.
   */
  wired(id: FeatureId): boolean {
    if (this.#demo) return true;
    const cap = WIRING[id].capability;
    return cap != null && this.#connection.has(cap);
  }

  /**
   * The standard gate: returns true when the caller should run the real path, otherwise
   * shows the not-wired-yet modal and returns false.
   */
  gate(id: FeatureId): boolean {
    if (this.wired(id)) return true;
    this.active = id;
    return false;
  }

  open(id: FeatureId): void {
    this.active = id;
  }

  close(): void {
    this.active = null;
  }
}
