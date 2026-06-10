// Minimal HTTP routing on top of node:http. The stubs have a dozen routes
// each; a framework would be more dependency than code.

import { createServer } from 'node:http';
import type { IncomingMessage, Server, ServerResponse } from 'node:http';

export interface RouteContext {
  req: IncomingMessage;
  res: ServerResponse;
  url: URL;
  params: Record<string, string>;
  body: unknown;
}

export type RouteHandler = (ctx: RouteContext) => void | Promise<void>;

export interface Route {
  method: string;
  /** Path pattern with :name segments, e.g. /api/v0/searches/:id */
  pattern: string;
  handler: RouteHandler;
}

export function sendJson(res: ServerResponse, status: number, payload: unknown): void {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(body),
  });
  res.end(body);
}

export function sendText(
  res: ServerResponse,
  status: number,
  body: string,
  contentType = 'text/plain; charset=utf-8',
): void {
  res.writeHead(status, {
    'content-type': contentType,
    'content-length': Buffer.byteLength(body),
  });
  res.end(body);
}

export function sendEmpty(res: ServerResponse, status: number): void {
  res.writeHead(status);
  res.end();
}

function matchPattern(pattern: string, path: string): Record<string, string> | null {
  const patternParts = pattern.split('/');
  const pathParts = path.split('/');
  if (patternParts.length !== pathParts.length) {
    return null;
  }
  const params: Record<string, string> = {};
  for (let i = 0; i < patternParts.length; i++) {
    const expected = patternParts[i] as string;
    const actual = pathParts[i] as string;
    if (expected.startsWith(':')) {
      params[expected.slice(1)] = decodeURIComponent(actual);
    } else if (expected !== actual) {
      return null;
    }
  }
  return params;
}

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(chunk as Buffer);
  }
  if (chunks.length === 0) {
    return undefined;
  }
  const raw = Buffer.concat(chunks).toString('utf8');
  if (raw.length === 0) {
    return undefined;
  }
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

export interface StubServerOptions {
  name: string;
  routes: Route[];
  /** Runs before routing; return true when the request was already handled. */
  intercept?: (ctx: Omit<RouteContext, 'params' | 'body'>) => boolean;
}

export function createStubServer(options: StubServerOptions): Server {
  return createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', 'http://localhost');
    try {
      if (options.intercept?.({ req, res, url })) {
        return;
      }
      for (const route of options.routes) {
        if (route.method !== req.method) {
          continue;
        }
        const params = matchPattern(route.pattern, url.pathname);
        if (params) {
          const body = await readBody(req);
          await route.handler({ req, res, url, params, body });
          return;
        }
      }
      console.warn(`[${options.name}] unmatched ${req.method} ${url.pathname}${url.search}`);
      sendJson(res, 404, { error: 'not found' });
    } catch (err) {
      console.error(`[${options.name}] handler error for ${req.method} ${url.pathname}:`, err);
      if (!res.headersSent) {
        sendJson(res, 500, { error: String(err) });
      }
    }
  });
}
