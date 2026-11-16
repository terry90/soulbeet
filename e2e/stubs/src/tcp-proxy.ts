// A TCP forwarder the suite can switch off: "Navidrome down" for the app, which
// dials the proxy while the real Navidrome keeps running behind it. Down closes
// the listener and destroys every open socket, so clients get ECONNREFUSED.

import { createServer, connect } from 'node:net';
import type { Server, Socket } from 'node:net';

export class TcpProxy {
  private server: Server | null = null;
  private readonly sockets = new Set<Socket>();

  constructor(
    private readonly listenPort: number,
    private readonly upstreamHost: string,
    private readonly upstreamPort: number,
  ) {}

  /** Listens; resolves once listening. */
  async start(): Promise<void> {
    if (this.server) {
      return;
    }
    const server = createServer((client) => this.forward(client));
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(this.listenPort, '0.0.0.0', () => {
        server.off('error', reject);
        resolve();
      });
    });
    this.server = server;
  }

  /** Down: stops listening and destroys every open socket. Up: listens again. */
  async setDown(down: boolean): Promise<void> {
    if (!down) {
      await this.start();
      return;
    }
    const server = this.server;
    this.server = null;
    for (const socket of this.sockets) {
      socket.destroy();
    }
    this.sockets.clear();
    if (server) {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  }

  private forward(client: Socket): void {
    const upstream = connect(this.upstreamPort, this.upstreamHost);
    for (const socket of [client, upstream]) {
      this.sockets.add(socket);
      socket.on('close', () => this.sockets.delete(socket));
      socket.on('error', () => socket.destroy());
    }
    client.on('close', () => upstream.destroy());
    upstream.on('close', () => client.destroy());
    client.pipe(upstream);
    upstream.pipe(client);
  }
}

/** NAVIDROME_PROXY_PORT and NAVIDROME_UPSTREAM ("host:port") together, or neither (null). One alone throws. */
export function proxyFromEnv(env: NodeJS.ProcessEnv): TcpProxy | null {
  const port = env.NAVIDROME_PROXY_PORT;
  const upstream = env.NAVIDROME_UPSTREAM;
  if (port === undefined && upstream === undefined) {
    return null;
  }
  if (port === undefined || upstream === undefined) {
    throw new Error('NAVIDROME_PROXY_PORT and NAVIDROME_UPSTREAM must be set together');
  }
  const split = upstream.lastIndexOf(':');
  const upstreamPort = Number(upstream.slice(split + 1));
  if (split <= 0 || !Number.isInteger(upstreamPort) || !Number.isInteger(Number(port))) {
    throw new Error(`NAVIDROME_UPSTREAM must be host:port and NAVIDROME_PROXY_PORT a port, got "${upstream}" and "${port}"`);
  }
  return new TcpProxy(Number(port), upstream.slice(0, split), upstreamPort);
}
