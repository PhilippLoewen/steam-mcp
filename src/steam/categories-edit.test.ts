import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, beforeEach, describe, it, expect } from 'vitest';
import {
  addApps,
  CategoryError,
  createCategory,
  deleteCategory,
  findCategory,
  parseCategories,
  removeApps,
  renameCategory,
  writeCategoriesFile,
} from './categories.js';

const NOW = 1800000000;

type Pair = [string, Record<string, unknown>];

function entry(key: string, value: object | null, extra: object = {}): Pair {
  return [
    key,
    {
      key,
      timestamp: 1787813434,
      ...(value ? { value: JSON.stringify(value) } : {}),
      version: '2810',
      ...extra,
    },
  ];
}

const fixture = JSON.stringify([
  entry('showcases.2', { nShowcaseId: 2 }),
  entry(
    'user-collections.favorite',
    { id: 'favorite', name: 'Favorites', added: [440, 730], removed: [] },
    { conflictResolutionMethod: 'custom', strMethodId: 'union-collections' },
  ),
  entry('user-collections.uc-abc', { id: 'uc-abc', name: 'RPG', added: [10], removed: [20] }),
  entry('user-collections.uc-dyn', {
    id: 'uc-dyn',
    name: 'Dynamic',
    added: [],
    removed: [],
    filterSpec: { nFormatVersion: 2, filterGroups: [] },
  }),
  entry('user-collections.uc-gone', null, { is_deleted: true }),
]);

function pairs(raw: string): Pair[] {
  return JSON.parse(raw) as Pair[];
}

function pairFor(raw: string, key: string): Record<string, unknown> | undefined {
  return pairs(raw).find((p) => p[0] === key)?.[1];
}

function valueFor(raw: string, key: string): Record<string, unknown> {
  return JSON.parse(pairFor(raw, key)?.['value'] as string) as Record<string, unknown>;
}

describe('addApps', () => {
  it('appends new appids, reports existing ones and bumps the timestamp only', () => {
    const edit = addApps(fixture, 'favorites', [730, 570, 570], NOW);
    expect(edit.key).toBe('user-collections.favorite');
    expect(edit.changed).toBe(true);
    expect(edit.affected).toEqual([570]);
    expect(edit.skipped).toEqual([730]);

    const meta = pairFor(edit.raw, edit.key);
    expect(meta).toMatchObject({
      timestamp: NOW,
      version: '2810',
      conflictResolutionMethod: 'custom',
      strMethodId: 'union-collections',
    });
    expect(valueFor(edit.raw, edit.key)).toEqual({
      id: 'favorite',
      name: 'Favorites',
      added: [440, 730, 570],
      removed: [],
    });
  });

  it('clears re-added appids from removed', () => {
    const edit = addApps(fixture, 'RPG', [20], NOW);
    expect(valueFor(edit.raw, edit.key)).toMatchObject({ added: [10, 20], removed: [] });
    expect(findCategory(parseCategories(edit.raw), 'RPG')?.appids).toEqual([10, 20]);
  });

  it('leaves the file untouched when nothing changes', () => {
    const edit = addApps(fixture, 'Favorites', [440], NOW);
    expect(edit.changed).toBe(false);
    expect(edit.raw).toBe(fixture);
  });

  it('keeps other entries and their order', () => {
    const edit = addApps(fixture, 'RPG', [1], NOW);
    const before = pairs(fixture);
    const after = pairs(edit.raw);
    expect(after.map((p) => p[0])).toEqual(before.map((p) => p[0]));
    expect(after[0]).toEqual(before[0]);
    expect(after[4]).toEqual(before[4]);
  });

  it('creates Hidden on demand with its special id', () => {
    const edit = addApps(fixture, 'Hidden', [99], NOW);
    expect(edit.key).toBe('user-collections.hidden');
    expect(pairFor(edit.raw, edit.key)).toEqual({
      key: 'user-collections.hidden',
      timestamp: NOW,
      value: JSON.stringify({ id: 'hidden', name: 'Hidden', added: [99], removed: [] }),
      conflictResolutionMethod: 'custom',
      strMethodId: 'union-collections',
    });
  });

  it('refuses dynamic collections and unknown names', () => {
    expect(() => addApps(fixture, 'Dynamic', [1], NOW)).toThrow(CategoryError);
    expect(() => addApps(fixture, 'Nope', [1], NOW)).toThrow(/not found/);
  });

  it('refuses filter collections that also list appids', () => {
    const raw = JSON.stringify([
      entry('user-collections.uc-mix', { name: 'Mix', added: [1], removed: [], filterSpec: {} }),
    ]);
    expect(() => addApps(raw, 'Mix', [2], NOW)).toThrow(/dynamic/);
  });

  it('revives a deleted Favorites tombstone in place', () => {
    const raw = JSON.stringify([
      entry('showcases.2', { nShowcaseId: 2 }),
      entry('user-collections.favorite', null, { is_deleted: true }),
    ]);
    const edit = addApps(raw, 'Favorites', [440], NOW);
    expect(pairs(edit.raw).map((p) => p[0])).toEqual(['showcases.2', 'user-collections.favorite']);
    expect(pairFor(edit.raw, edit.key)?.['is_deleted']).toBeUndefined();
    expect(pairFor(edit.raw, edit.key)?.['version']).toBe('2810');
    expect(valueFor(edit.raw, edit.key)).toMatchObject({ id: 'favorite', added: [440] });
  });
});

describe('removeApps', () => {
  it('drops appids from added and reports missing ones', () => {
    const edit = removeApps(fixture, 'Favorites', [730, 999], NOW);
    expect(edit.affected).toEqual([730]);
    expect(edit.skipped).toEqual([999]);
    expect(valueFor(edit.raw, edit.key)).toMatchObject({ added: [440], removed: [] });
    expect(pairFor(edit.raw, edit.key)?.['timestamp']).toBe(NOW);
  });

  it('also clears the appid from removed, like Steam', () => {
    const edit = removeApps(fixture, 'RPG', [10, 20], NOW);
    expect(edit.affected).toEqual([10]);
    expect(valueFor(edit.raw, edit.key)).toMatchObject({ added: [], removed: [] });
  });

  it('refuses dynamic collections', () => {
    expect(() => removeApps(fixture, 'Dynamic', [1], NOW)).toThrow(CategoryError);
  });
});

describe('createCategory', () => {
  it('appends a new uc- collection', () => {
    const edit = createCategory(fixture, '  Co-op  ', NOW, 'AbCdEfGh1234');
    expect(edit.key).toBe('user-collections.uc-AbCdEfGh1234');
    expect(pairs(edit.raw).at(-1)).toEqual([
      edit.key,
      {
        key: edit.key,
        timestamp: NOW,
        value: JSON.stringify({ id: 'uc-AbCdEfGh1234', name: 'Co-op', added: [], removed: [] }),
        conflictResolutionMethod: 'custom',
        strMethodId: 'union-collections',
      },
    ]);
  });

  it('generates a 12-character base62 id by default', () => {
    const { key } = createCategory(fixture, 'New', NOW);
    expect(key).toMatch(/^user-collections\.uc-[A-Za-z0-9]{12}$/);
  });

  it('refuses duplicate and empty names', () => {
    expect(() => createCategory(fixture, 'rpg', NOW)).toThrow(/already exists/);
    expect(() => createCategory(fixture, 'hidden', NOW)).toThrow(/reserved/);
    expect(() => createCategory(fixture, '   ', NOW)).toThrow(CategoryError);
  });
});

describe('renameCategory', () => {
  it('changes only the name', () => {
    const edit = renameCategory(fixture, 'RPG', 'Role-playing', NOW);
    expect(valueFor(edit.raw, edit.key)).toEqual({
      id: 'uc-abc',
      name: 'Role-playing',
      added: [10],
      removed: [20],
    });
  });

  it('refuses collisions and the special collections', () => {
    expect(() => renameCategory(fixture, 'RPG', 'dynamic', NOW)).toThrow(/already exists/);
    expect(() => renameCategory(fixture, 'RPG', 'favorites', NOW)).toThrow(/reserved/);
    expect(() => renameCategory(fixture, 'Favorites', 'Faves', NOW)).toThrow(CategoryError);
  });
});

describe('deleteCategory', () => {
  it('writes a tombstone in the shape Steam uses', () => {
    const edit = deleteCategory(fixture, 'RPG', NOW);
    expect(pairFor(edit.raw, edit.key)).toEqual({
      key: 'user-collections.uc-abc',
      timestamp: NOW,
      is_deleted: true,
      version: '2810',
    });
    expect(findCategory(parseCategories(edit.raw), 'RPG')).toBeUndefined();
  });

  it('refuses the special collections', () => {
    expect(() => deleteCategory(fixture, 'Favorites', NOW)).toThrow(CategoryError);
  });
});

describe('writeCategoriesFile', () => {
  let dir: string;
  let file: string;
  let modified: string;

  beforeEach(() => {
    dir = fs.mkdtempSync(path.join(os.tmpdir(), 'steam-mcp-categories-'));
    file = path.join(dir, 'cloud-storage-namespace-1.json');
    modified = path.join(dir, 'cloud-storage-namespace-1.modified.json');
    fs.writeFileSync(file, fixture);
  });

  afterEach(() => {
    fs.rmSync(dir, { recursive: true, force: true });
  });

  it('backs up, writes and marks the key as modified', () => {
    fs.writeFileSync(modified, '["showcases.2"]');
    writeCategoriesFile(file, '[]', 'user-collections.favorite');

    expect(fs.readFileSync(file, 'utf-8')).toBe('[]');
    expect(fs.readFileSync(`${file}.bak`, 'utf-8')).toBe(fixture);
    expect(JSON.parse(fs.readFileSync(modified, 'utf-8'))).toEqual([
      'showcases.2',
      'user-collections.favorite',
    ]);
    expect(fs.readdirSync(dir).sort()).toEqual([
      'cloud-storage-namespace-1.json',
      'cloud-storage-namespace-1.json.bak',
      'cloud-storage-namespace-1.modified.json',
    ]);
  });

  it('creates modified.json and does not duplicate keys', () => {
    writeCategoriesFile(file, '[]', 'user-collections.favorite');
    writeCategoriesFile(file, '[]', 'user-collections.favorite');
    expect(JSON.parse(fs.readFileSync(modified, 'utf-8'))).toEqual(['user-collections.favorite']);
  });

  it('writes nothing when modified.json is malformed', () => {
    fs.writeFileSync(modified, '{oops');
    expect(() => writeCategoriesFile(file, '[]', 'k')).toThrow(/modified\.json/);
    expect(fs.readFileSync(file, 'utf-8')).toBe(fixture);
    expect(fs.existsSync(`${file}.bak`)).toBe(false);
  });
});
