import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { decode, encode } from '@msgpack/msgpack';
import { type Category, CategoryError, categoryKind, findCategory, KEY_PREFIX } from './categories.js';

/**
 * Live category edits through the steam-mcp-bridge Millennium plugin
 * (millennium-plugin/ in this repo), so Steam doesn't have to be closed.
 *
 * Millennium serves a local socket (MEP) whose plugin.config.set fires the
 * plugin's Lua config.on_change. A request goes to config key request:<id>;
 * the plugin runs it against Steam's collectionStore and answers in
 * response:<id>, which is polled here.
 */

export const BRIDGE_PLUGIN = 'steam-mcp-bridge';

const MAX_FRAME = 64 * 1024 * 1024;

export function mepSocketPath(): string {
  if (process.env.MILLENNIUM_MEP_SOCKET) return process.env.MILLENNIUM_MEP_SOCKET;
  if (process.platform === 'win32') {
    return path.join(process.env.TEMP ?? 'C:\\Windows\\Temp', 'millennium-mep.sock');
  }
  return '/tmp/millennium-mep.sock';
}

/** MEP frame: 4-byte little-endian length, then a msgpack payload. */
export function encodeFrame(message: unknown): Buffer {
  const payload = encode(message);
  const frame = Buffer.alloc(4 + payload.length);
  frame.writeUInt32LE(payload.length, 0);
  frame.set(payload, 4);
  return frame;
}

/** Splits a byte stream into decoded MEP frames. */
export class FrameReader {
  private buffer = Buffer.alloc(0);

  push(chunk: Buffer): unknown[] {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    const messages: unknown[] = [];
    while (this.buffer.length >= 4) {
      const length = this.buffer.readUInt32LE(0);
      if (length === 0 || length > MAX_FRAME) throw new Error(`Invalid MEP frame length ${length}`);
      if (this.buffer.length < 4 + length) break;
      messages.push(decode(this.buffer.subarray(4, 4 + length)));
      this.buffer = this.buffer.subarray(4 + length);
    }
    return messages;
  }
}

interface MepResponse {
  id?: string;
  result?: unknown;
  error?: string | null;
}

export class MepClient {
  private nextId = 0;
  private readonly pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

  private constructor(private readonly socket: net.Socket) {
    const reader = new FrameReader();
    socket.on('data', (chunk) => {
      let messages: unknown[];
      try {
        messages = reader.push(chunk);
      } catch (error) {
        socket.destroy(error as Error);
        return;
      }
      for (const message of messages) {
        const { id, result, error } = (message ?? {}) as MepResponse;
        if (id === undefined) continue;
        const waiter = this.pending.get(id);
        if (!waiter) continue; // subscription events or answers past their timeout
        this.pending.delete(id);
        if (error) waiter.reject(new Error(error));
        else waiter.resolve(result);
      }
    });
    socket.on('close', () => this.failAll(new Error('Millennium closed the connection')));
    socket.on('error', (error) => this.failAll(error));
  }

  static connect(socketPath: string, timeoutMs = 2000): Promise<MepClient> {
    return new Promise((resolve, reject) => {
      const socket = net.createConnection(socketPath);
      const timer = setTimeout(() => {
        socket.destroy();
        reject(new Error(`Timed out connecting to ${socketPath}`));
      }, timeoutMs);
      socket.once('connect', () => {
        clearTimeout(timer);
        socket.removeAllListeners('error');
        resolve(new MepClient(socket));
      });
      socket.once('error', (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
  }

  call(method: string, params: Record<string, unknown> = {}, timeoutMs = 5000): Promise<unknown> {
    const id = String(++this.nextId);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Millennium did not answer ${method} within ${timeoutMs / 1000}s`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.socket.write(encodeFrame({ id, method, params }));
    });
  }

  close(): void {
    this.socket.end();
  }

  private failAll(error: Error): void {
    for (const waiter of this.pending.values()) waiter.reject(error);
    this.pending.clear();
  }
}

export type BridgeRequest =
  | { action: 'add' | 'remove'; name: string; appids: number[] }
  | { action: 'create' | 'delete'; name: string }
  | { action: 'rename'; name: string; newName: string };

export interface BridgeCollection {
  /** Collection id as Steam's collectionStore knows it, e.g. "favorite" or "uc-AbC123". */
  id: string;
  name: string;
  dynamic: boolean;
  appids: number[];
}

export interface BridgeResult {
  changed: boolean;
  id: string;
  /** Absent once deleted. */
  collection?: BridgeCollection;
  affected?: number[];
  skipped?: number[];
}

type BridgeResponse = { ok: true; result: BridgeResult } | { ok: false; error: string };

export interface BridgeOptions {
  socketPath?: string;
  timeoutMs?: number;
  pollMs?: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** The request reached Millennium but no answer came back in time. */
export class BridgeTimeoutError extends CategoryError {}

/**
 * Whether the categories file shows `request` as done, for when the plugin
 * didn't answer. Steam saves edits made in the client to that file.
 */
export function verifyBridgeEdit(request: BridgeRequest, categories: Category[]): { applied: boolean; category?: Category } {
  switch (request.action) {
    case 'add':
    case 'remove': {
      const category = findCategory(categories, request.name);
      const wanted = request.action === 'add';
      const applied = !!category && request.appids.every((appid) => category.appids.includes(appid) === wanted);
      return { applied, category };
    }
    case 'create': {
      const category = findCategory(categories, request.name);
      return { applied: !!category, category };
    }
    case 'rename': {
      const category = findCategory(categories, request.newName);
      return { applied: !!category, category };
    }
    case 'delete':
      return { applied: !findCategory(categories, request.name) };
  }
}

/** Why the bridge couldn't be used; nothing was sent to Steam. */
export interface BridgeUnavailable {
  unavailable: string;
}

/**
 * Run an edit through the bridge plugin. Resolves to BridgeUnavailable when
 * the bridge can't be reached (no Millennium socket, plugin missing or not
 * running), so the caller can fall back. Once the request is sent, every
 * failure throws CategoryError.
 */
export async function runBridgeEdit(
  request: BridgeRequest,
  options: BridgeOptions = {},
): Promise<BridgeResult | BridgeUnavailable> {
  const { socketPath = mepSocketPath(), timeoutMs = 15000, pollMs = 100 } = options;
  if (!fs.existsSync(socketPath)) return { unavailable: `Millennium is not running (no socket at ${socketPath})` };

  let client: MepClient;
  try {
    client = await MepClient.connect(socketPath);
  } catch (error) {
    return { unavailable: `can't connect to Millennium: ${(error as Error).message}` };
  }

  try {
    let status: { running?: boolean; enabled?: boolean };
    try {
      status = (await client.call('plugin.status', { name: BRIDGE_PLUGIN })) as typeof status;
    } catch (error) {
      return { unavailable: `${BRIDGE_PLUGIN} is not installed (${(error as Error).message})` };
    }
    if (!status?.running) {
      return { unavailable: `${BRIDGE_PLUGIN} is installed but ${status?.enabled ? 'not running' : 'disabled'}` };
    }

    const id = randomUUID();
    const requestKey = `request:${id}`;
    const responseKey = `response:${id}`;
    const forget = (key: string) => client.call('plugin.config.delete', { name: BRIDGE_PLUGIN, key }).catch(() => {});

    let response: BridgeResponse | null = null;
    try {
      await client.call('plugin.config.set', { name: BRIDGE_PLUGIN, key: requestKey, value: JSON.stringify(request) });
      const deadline = Date.now() + timeoutMs;
      while (!response && Date.now() < deadline) {
        const { value } = (await client.call('plugin.config.get', { name: BRIDGE_PLUGIN, key: responseKey })) as {
          value: BridgeResponse | null;
        };
        if (value) response = value;
        else await sleep(pollMs);
      }
    } catch (error) {
      throw new CategoryError(
        `Lost the connection to Millennium (${(error as Error).message}); the edit may or may not have been applied.`,
      );
    }

    if (!response) {
      await Promise.all([forget(requestKey), forget(responseKey)]);
      throw new BridgeTimeoutError(
        `The ${BRIDGE_PLUGIN} Millennium plugin did not answer within ${timeoutMs / 1000}s; the edit may or may not have been applied.`,
      );
    }
    await forget(responseKey);
    if (!response.ok) throw new CategoryError(response.error);
    return response.result;
  } finally {
    client.close();
  }
}

/** Shape a bridge collection like a parsed file entry, for shared reporting. */
export function bridgeCategory(collection: BridgeCollection): Category {
  const key = KEY_PREFIX + collection.id;
  return {
    name: collection.name,
    key,
    kind: categoryKind(collection.id, collection.dynamic),
    version: null,
    timestamp: null,
    appids: collection.appids,
    added: collection.appids.length,
    removed: 0,
    hasFilter: collection.dynamic,
    isDeleted: false,
  };
}
