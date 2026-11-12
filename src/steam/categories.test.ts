import { describe, it, expect } from 'vitest';
import { findCategory, parseCategories } from './categories.js';

function entry(key: string, value: object | null, extra: object = {}): [string, object] {
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

const STRATEGY = [
  255710, 4570, 4580, 8930, 9310, 9350, 9420, 9450, 15620, 20570, 21970, 40100, 56400,
  105430, 105450, 221380, 241930,
];

const fixture = JSON.stringify([
  entry('GameReleased', { apps: [] }),
  entry('showcases.2', { nShowcaseId: 2 }),
  entry('user-collections.favorite', {
    id: 'favorite',
    name: 'Favorites',
    added: [440, 230410, 271590, 238370, 553850, 646570, 2379780],
    removed: [],
  }),
  entry('user-collections.from-tag-Strategy', {
    id: 'from-tag-Strategy',
    name: 'Strategy',
    added: STRATEGY,
    removed: [],
  }),
  entry('user-collections.hidden', {
    id: 'hidden',
    name: 'Hidden',
    added: [1385510, 99830, 654310, 261570, 8980, 383080, 1151640, 1083500],
    removed: [],
  }),
  entry('user-collections.from-tag-Nintendo DS', {
    id: 'from-tag-Nintendo DS',
    name: 'Nintendo DS',
    added: [],
    removed: [],
  }),
  entry('user-collections.uc-DQIQvQZKuBvY', {
    id: 'uc-DQIQvQZKuBvY',
    name: 'Garfield + Masterhuck',
    added: [],
    removed: [],
    filterSpec: {
      nFormatVersion: 2,
      strSearchText: '',
      filterGroups: [
        { rgOptions: [8], bAcceptUnion: true },
        { rgOptions: [33554147, 31376102], bAcceptUnion: false },
      ],
      setSuggestions: {},
    },
  }),
  entry('user-collections.uc-gn4OC6hGDS0B', {
    id: 'uc-gn4OC6hGDS0B',
    name: 'Gold+',
    added: [],
    removed: [],
    filterSpec: {
      nFormatVersion: 2,
      strSearchText: '',
      filterGroups: [{ rgOptions: [], bAcceptUnion: true }, { rgOptions: [0, 1], bAcceptUnion: true }],
    },
  }),
  entry('user-collections.from-tag-Humble Bundle 8', null, { is_deleted: true }),
  entry('user-collections.uc-llVuLcjuUCgW', { id: 'uc-llVuLcjuUCgW', name: 'Gone', added: [1], removed: [] }, { is_deleted: true }),
]);

describe('parseCategories', () => {
  const categories = parseCategories(fixture);

  it('returns only live user collections, skipping tombstones and other keys', () => {
    expect(categories.map((c) => c.name)).toEqual([
      'Favorites',
      'Strategy',
      'Hidden',
      'Nintendo DS',
      'Garfield + Masterhuck',
      'Gold+',
    ]);
  });

  it('includes tombstones on request', () => {
    expect(parseCategories(fixture, true)).toHaveLength(8);
  });

  it('resolves explicit membership', () => {
    const favorites = findCategory(categories, 'favorites');
    expect(favorites?.kind).toBe('explicit');
    expect(favorites?.appids).toHaveLength(7);
    expect(favorites?.appids).toContain(553850);
    expect(favorites?.appids).toContain(2379780);

    const strategy = findCategory(categories, 'Strategy');
    expect(strategy?.kind).toBe('tag');
    expect(strategy?.appids).toHaveLength(17);
    expect(strategy?.appids).toContain(241930);

    const hidden = findCategory(categories, 'Hidden');
    expect(hidden?.appids).toHaveLength(8);
    expect(hidden?.appids).toContain(1385510);
    expect(hidden?.appids).toContain(1083500);
  });

  it('keeps raw filterSpec for filter collections without appids', () => {
    const gold = findCategory(categories, 'gold+');
    expect(gold?.kind).toBe('filter');
    expect(gold?.appids).toEqual([]);
    const groups = gold?.filterSpec?.filterGroups ?? [];
    expect(groups[groups.length - 1]).toEqual({ rgOptions: [0, 1], bAcceptUnion: true });

    const gm = findCategory(categories, 'Garfield + Masterhuck');
    expect(gm?.filterSpec?.filterGroups).toContainEqual({
      rgOptions: [33554147, 31376102],
      bAcceptUnion: false,
    });
    expect(gm?.filterSpec?.filterGroups).toContainEqual({ rgOptions: [8], bAcceptUnion: true });
  });

  it('subtracts removed from added', () => {
    const raw = JSON.stringify([
      entry('user-collections.uc-x', { name: 'X', added: [1, 2, 3], removed: [2] }),
    ]);
    expect(parseCategories(raw)[0]?.appids).toEqual([1, 3]);
  });

  it('is defensive about malformed input', () => {
    expect(parseCategories('not json')).toEqual([]);
    expect(parseCategories('{}')).toEqual([]);
    expect(parseCategories('[["user-collections.a",{"value":"{bad"}]]')).toEqual([]);
  });
});
