// Mirrors the client's connection state as reactive rune state (docs/gui/05 §5).

import type { Client, ConnInfo } from '../ipc/client';

export class ConnectionStore {
  info = $state<ConnInfo>({
    state: 'connecting',
    coreVersion: null,
    protocolVersion: null,
    capabilities: [],
    ownerMode: null,
    reason: null,
  });

  constructor(client: Client) {
    client.onConn((info) => {
      this.info = { ...info };
    });
  }

  get online(): boolean {
    return this.info.state === 'online';
  }

  /** Commands are allowed while online or degraded; disabled offline/connecting. */
  get usable(): boolean {
    return this.info.state === 'online' || this.info.state === 'degraded';
  }

  has(capability: string): boolean {
    return this.info.capabilities.includes(capability);
  }
}
