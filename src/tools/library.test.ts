import { beforeEach, describe, it, expect, vi } from 'vitest';
import type { CategoryEdit } from '../steam/categories.js';

const steam = vi.hoisted(() => ({ running: false }));

vi.mock('../steam/paths.js', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../steam/paths.js')>()),
  isSteamRunning: () => steam.running,
}));

// Keep app-name lookups for reported categories off disk and network.
vi.mock('../steam/manifests.js', () => ({ readAllManifests: async () => [] }));
vi.mock('../steam/api.js', () => ({
  hasApiKey: () => false,
  steamApiRequest: vi.fn(),
  storeApiRequest: () => Promise.reject(new Error('offline')),
}));

vi.mock('../steam/millennium.js', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../steam/millennium.js')>()),
  runBridgeEdit: vi.fn(),
}));

vi.mock('../steam/categories.js', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../steam/categories.js')>()),
  editCategories: vi.fn(),
  readCategories: vi.fn(),
}));

const { runCategoryEdit } = await import('./library.js');
const { runBridgeEdit, BridgeTimeoutError } = await import('../steam/millennium.js');
const { editCategories, readCategories } = await import('../steam/categories.js');

const request = { action: 'create', name: 'RPG' } as const;
const fileEdit = (raw: string): CategoryEdit => ({ raw, key: 'user-collections.uc-abc', changed: true });

function output(result: { content: { text: string }[] }) {
  return result.content[0].text;
}

describe('runCategoryEdit route choice', () => {
  beforeEach(() => {
    vi.mocked(runBridgeEdit).mockReset();
    vi.mocked(editCategories).mockReset();
  });

  it('edits through the bridge while Steam runs', async () => {
    steam.running = true;
    vi.mocked(runBridgeEdit).mockResolvedValue({
      changed: true,
      id: 'uc-abc',
      collection: { id: 'uc-abc', name: 'RPG', dynamic: false, appids: [] },
    });

    const result = await runCategoryEdit('category_create', request, fileEdit);

    expect(runBridgeEdit).toHaveBeenCalledWith(request);
    expect(editCategories).not.toHaveBeenCalled();
    expect(JSON.parse(output(result))).toMatchObject({
      changed: true,
      via: 'millennium',
      category: { name: 'RPG', key: 'user-collections.uc-abc', kind: 'explicit' },
    });
  });

  it('refuses with the bridge status while Steam runs without a usable bridge', async () => {
    steam.running = true;
    vi.mocked(runBridgeEdit).mockResolvedValue({ unavailable: 'steam-mcp-bridge is installed but disabled' });

    const result = await runCategoryEdit('category_create', request, fileEdit);

    expect(result).toMatchObject({ isError: true });
    expect(output(result)).toMatch(/Steam is currently running/);
    expect(output(result)).toMatch(/Millennium bridge: steam-mcp-bridge is installed but disabled\./);
    expect(editCategories).not.toHaveBeenCalled();
  });

  it('edits the file while Steam is closed, without touching the bridge', async () => {
    steam.running = false;
    vi.mocked(editCategories).mockReturnValue({ raw: '[]', key: 'user-collections.uc-abc', changed: true });

    const result = await runCategoryEdit('category_create', request, fileEdit);

    expect(runBridgeEdit).not.toHaveBeenCalled();
    expect(editCategories).toHaveBeenCalledWith(fileEdit);
    expect(JSON.parse(output(result))).toEqual({ changed: true, key: 'user-collections.uc-abc' });
  });

  it('names the membership fields on both routes', async () => {
    const fields = { affected: 'added', skipped: 'alreadyPresent' };

    steam.running = true;
    vi.mocked(runBridgeEdit).mockResolvedValue({ changed: true, id: 'favorite', affected: [440], skipped: [730] });
    expect(JSON.parse(output(await runCategoryEdit('category_add', request, fileEdit, fields)))).toMatchObject({
      added: [440],
      alreadyPresent: [730],
    });

    steam.running = false;
    vi.mocked(editCategories).mockReturnValue({
      raw: '[]',
      key: 'user-collections.favorite',
      changed: false,
      affected: [],
      skipped: [440],
    } as CategoryEdit);
    expect(JSON.parse(output(await runCategoryEdit('category_add', request, fileEdit, fields)))).toMatchObject({
      added: [],
      alreadyPresent: [440],
    });
  });
});

describe('runCategoryEdit after a bridge timeout', () => {
  const add = { action: 'add', name: 'RPG', appids: [440] } as const;
  const rpg = (appids: number[]) => ({
    name: 'RPG',
    key: 'user-collections.uc-abc',
    kind: 'explicit' as const,
    version: null,
    timestamp: null,
    appids,
    added: appids.length,
    removed: 0,
    hasFilter: false,
    isDeleted: false,
  });

  beforeEach(() => {
    steam.running = true;
    vi.mocked(runBridgeEdit).mockReset();
    vi.mocked(readCategories).mockReset();
    vi.mocked(runBridgeEdit).mockRejectedValue(new BridgeTimeoutError('The plugin did not answer within 15s.'));
  });

  it('reports the edit as applied when the categories file shows it', async () => {
    vi.mocked(readCategories).mockReturnValue([rpg([10, 440])]);

    const result = await runCategoryEdit('category_add', add, fileEdit);

    expect(result).not.toHaveProperty('isError');
    expect(JSON.parse(output(result))).toMatchObject({
      via: 'millennium',
      verifiedByReadback: true,
      category: { key: 'user-collections.uc-abc', appids: [{ appid: 10 }, { appid: 440 }] },
    });
  });

  it('stays an error when the categories file does not show it', async () => {
    vi.mocked(readCategories).mockReturnValue([rpg([10])]);

    const result = await runCategoryEdit('category_add', add, fileEdit);

    expect(result).toMatchObject({ isError: true });
    expect(output(result)).toMatch(/did not answer.*doesn't show it yet/);
  });
});
