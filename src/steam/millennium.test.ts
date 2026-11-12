import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { afterEach, beforeEach, describe, it, expect } from 'vitest';
import { CategoryError } from './categories.js';
import {
  BRIDGE_PLUGIN,
  bridgeCategory,
  type BridgeRequest,
  encodeFrame,
  FrameReader,
  BridgeTimeoutError,
  MepClient,
  runBridgeEdit,
  verifyBridgeEdit,
} from './millennium.js';

describe('MEP framing', () => {
  it('round-trips messages split across chunks', () => {
    const a = encodeFrame({ id: '1', method: 'plugin.list', params: {} });
    const b = encodeFrame({ id: '2', result: [1, 2, 3], error: null });
    const bytes = Buffer.concat([a, b]);
    const reader = new FrameReader();

    expect(reader.push(bytes.subarray(0, 3))).toEqual([]);
    expect(reader.push(bytes.subarray(3, a.length + 5))).toEqual([{ id: '1', method: 'plugin.list', params: {} }]);
    expect(reader.push(bytes.subarray(a.length + 5))).toEqual([{ id: '2', result: [1, 2, 3], error: null }]);
  });

  it('prefixes the payload with its little-endian length', () => {
    const frame = encodeFrame('x');
    expect(frame.readUInt32LE(0)).toBe(frame.length - 4);
  });
});

/**
 * Minimal Millennium MEP server: answers plugin.status and plugin.config.*,
 * and plays the bridge plugin by answering request:<id> keys via `plugin`.
 */
function fakeMillennium(
  socketPath: string,
  options: {
    status?: { running: boolean; enabled: boolean } | null;
    plugin?: (request: BridgeRequest) => unknown;
  },
) {
  const config = new Map<string, unknown>();
  const calls: string[] = [];
  const server = net.createServer((socket) => {
    const reader = new FrameReader();
    socket.on('data', (chunk) => {
      for (const message of reader.push(chunk)) {
        const { id, method, params } = message as { id: string; method: string; params: Record<string, unknown> };
        calls.push(method);
        const reply = (result: unknown, error: string | null = null) =>
          socket.write(encodeFrame({ id, result, error }));

        if (params.name !== BRIDGE_PLUGIN) {
          reply(null, `plugin not found: ${String(params.name)}`);
          continue;
        }
        const key = params.key as string;
        switch (method) {
          case 'plugin.status':
            if (options.status === null) reply(null, `plugin not found: ${BRIDGE_PLUGIN}`);
            else reply({ name: BRIDGE_PLUGIN, ...(options.status ?? { running: true, enabled: true }) });
            break;
          case 'plugin.config.set': {
            config.set(key, params.value);
            const requestId = key.startsWith('request:') ? key.slice('request:'.length) : null;
            if (requestId && options.plugin) {
              config.delete(key);
              const answer = options.plugin(JSON.parse(params.value as string) as BridgeRequest);
              if (answer !== undefined) config.set(`response:${requestId}`, answer);
            }
            reply({ success: true });
            break;
          }
          case 'plugin.config.get':
            reply({ value: config.get(key) ?? null });
            break;
          case 'plugin.config.delete':
            config.delete(key);
            reply({ success: true });
            break;
          default:
            reply(null, `unknown method ${method}`);
        }
      }
    });
  });
  return {
    config,
    calls,
    listen: () => new Promise<void>((resolve) => server.listen(socketPath, resolve)),
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

describe('runBridgeEdit', () => {
  let dir: string;
  let socketPath: string;
  let server: ReturnType<typeof fakeMillennium> | undefined;

  beforeEach(() => {
    dir = fs.mkdtempSync(path.join(os.tmpdir(), 'steam-mcp-mep-'));
    socketPath = path.join(dir, 'mep.sock');
  });

  afterEach(async () => {
    await server?.close();
    server = undefined;
    fs.rmSync(dir, { recursive: true, force: true });
  });

  async function start(options: Parameters<typeof fakeMillennium>[1]) {
    server = fakeMillennium(socketPath, options);
    await server.listen();
    return server;
  }

  it('reports a missing Millennium socket as unavailable', async () => {
    expect(await runBridgeEdit({ action: 'create', name: 'X' }, { socketPath })).toEqual({
      unavailable: expect.stringMatching(/Millennium is not running/),
    });
  });

  it('reports a missing, disabled or stopped plugin as unavailable', async () => {
    const edit = () => runBridgeEdit({ action: 'create', name: 'X' }, { socketPath });

    await start({ status: null });
    expect(await edit()).toEqual({ unavailable: expect.stringMatching(/not installed/) });
    await server!.close();

    await start({ status: { running: false, enabled: false } });
    expect(await edit()).toEqual({ unavailable: expect.stringMatching(/installed but disabled/) });
    await server!.close();

    await start({ status: { running: false, enabled: true } });
    expect(await edit()).toEqual({ unavailable: expect.stringMatching(/installed but not running/) });
  });

  it('sends the request and returns the plugin result, cleaning up the response key', async () => {
    const seen: BridgeRequest[] = [];
    const fake = await start({
      plugin: (request) => {
        seen.push(request);
        return {
          ok: true,
          result: {
            changed: true,
            id: 'uc-abc',
            collection: { id: 'uc-abc', name: 'RPG', dynamic: false, appids: [10, 440] },
            affected: [440],
            skipped: [],
          },
        };
      },
    });

    const result = await runBridgeEdit({ action: 'add', name: 'RPG', appids: [440] }, { socketPath, pollMs: 5 });

    expect(seen).toEqual([{ action: 'add', name: 'RPG', appids: [440] }]);
    expect(result).toEqual({
      changed: true,
      id: 'uc-abc',
      collection: { id: 'uc-abc', name: 'RPG', dynamic: false, appids: [10, 440] },
      affected: [440],
      skipped: [],
    });
    expect(fake.config.size).toBe(0);
  });

  it('throws the plugin error as a CategoryError', async () => {
    await start({ plugin: () => ({ ok: false, error: 'Category "Nope" not found. Available: RPG' }) });

    const edit = runBridgeEdit({ action: 'delete', name: 'Nope' }, { socketPath, pollMs: 5 });
    await expect(edit).rejects.toThrow(CategoryError);
    await expect(edit).rejects.toThrow(/not found/);
  });

  it('times out and withdraws the request and any late response when the plugin never answers', async () => {
    const fake = await start({});

    await expect(
      runBridgeEdit({ action: 'create', name: 'X' }, { socketPath, timeoutMs: 50, pollMs: 5 }),
    ).rejects.toThrow(BridgeTimeoutError);
    expect(fake.config.size).toBe(0);
    expect(fake.calls.slice(-2)).toEqual(['plugin.config.delete', 'plugin.config.delete']);
  });

  it('fails a call Millennium never answers instead of hanging', async () => {
    const silent = net.createServer((socket) => socket.resume()); // reads, never answers
    await new Promise<void>((resolve) => silent.listen(socketPath, resolve));
    try {
      const client = await MepClient.connect(socketPath);
      await expect(client.call('plugin.status', { name: BRIDGE_PLUGIN }, 30)).rejects.toThrow(/did not answer/);
      client.close();
    } finally {
      await new Promise<void>((resolve) => silent.close(() => resolve()));
    }
  });

  it('turns a dropped connection after sending the request into a CategoryError', async () => {
    const dropping = net.createServer((socket) => {
      const reader = new FrameReader();
      socket.on('data', (chunk) => {
        for (const message of reader.push(chunk)) {
          const { id, method } = message as { id: string; method: string };
          if (method === 'plugin.status') socket.write(encodeFrame({ id, result: { running: true }, error: null }));
          else socket.destroy();
        }
      });
    });
    await new Promise<void>((resolve) => dropping.listen(socketPath, resolve));
    try {
      const edit = runBridgeEdit({ action: 'create', name: 'X' }, { socketPath, pollMs: 5 });
      await expect(edit).rejects.toThrow(CategoryError);
      await expect(edit).rejects.toThrow(/may or may not have been applied/);
    } finally {
      await new Promise<void>((resolve) => dropping.close(() => resolve()));
    }
  });
});

describe('verifyBridgeEdit', () => {
  const rpg = bridgeCategory({ id: 'uc-abc', name: 'RPG', dynamic: false, appids: [10, 440] });

  it('checks membership for add and remove', () => {
    expect(verifyBridgeEdit({ action: 'add', name: 'rpg', appids: [440] }, [rpg])).toEqual({ applied: true, category: rpg });
    expect(verifyBridgeEdit({ action: 'add', name: 'RPG', appids: [440, 730] }, [rpg]).applied).toBe(false);
    expect(verifyBridgeEdit({ action: 'remove', name: 'RPG', appids: [730] }, [rpg]).applied).toBe(true);
    expect(verifyBridgeEdit({ action: 'remove', name: 'RPG', appids: [440] }, [rpg]).applied).toBe(false);
    expect(verifyBridgeEdit({ action: 'add', name: 'Nope', appids: [440] }, [rpg]).applied).toBe(false);
  });

  it('checks existence for create, rename and delete', () => {
    expect(verifyBridgeEdit({ action: 'create', name: 'RPG' }, [rpg]).applied).toBe(true);
    expect(verifyBridgeEdit({ action: 'create', name: 'New' }, [rpg]).applied).toBe(false);
    expect(verifyBridgeEdit({ action: 'rename', name: 'Old', newName: 'RPG' }, [rpg]).applied).toBe(true);
    expect(verifyBridgeEdit({ action: 'delete', name: 'RPG' }, [rpg]).applied).toBe(false);
    expect(verifyBridgeEdit({ action: 'delete', name: 'RPG' }, []).applied).toBe(true);
  });
});

describe('bridgeCategory', () => {
  it('maps collection ids to file keys and kinds', () => {
    expect(bridgeCategory({ id: 'favorite', name: 'Favorites', dynamic: false, appids: [1, 2] })).toMatchObject({
      key: 'user-collections.favorite',
      kind: 'explicit',
      appids: [1, 2],
      added: 2,
      hasFilter: false,
    });
    expect(bridgeCategory({ id: 'uc-x', name: 'Dyn', dynamic: true, appids: [] }).kind).toBe('filter');
    expect(bridgeCategory({ id: 'from-tag-RPG', name: 'RPG', dynamic: false, appids: [] }).kind).toBe('tag');
  });
});
