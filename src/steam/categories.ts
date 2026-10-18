import fs from 'node:fs';
import path from 'node:path';
import { randomInt } from 'node:crypto';
import { getSteamDir, getUserConfig, isSteamRunning } from './paths.js';

/**
 * Library categories ("user collections") as stored by the Steam client in
 * userdata/<accountid>/config/cloudstorage/cloud-storage-namespace-1.json.
 *
 * That file is a cloud-synced cache owned by the Steam client. Edits are only
 * written while Steam is closed: the changed key is listed in
 * cloud-storage-namespace-1.modified.json so Steam uploads it on its next start.
 * While Steam runs, edits go through the Millennium bridge (millennium.ts).
 */

export const KEY_PREFIX = 'user-collections.';

export interface FilterGroup {
  rgOptions?: unknown[];
  bAcceptUnion?: boolean;
  [key: string]: unknown;
}

export interface FilterSpec {
  nFormatVersion?: number;
  strSearchText?: string;
  filterGroups?: FilterGroup[];
  [key: string]: unknown;
}

export type CategoryKind = 'explicit' | 'filter' | 'tag';

export interface Category {
  name: string;
  key: string;
  kind: CategoryKind;
  version: string | null;
  timestamp: number | null;
  /** Explicit membership: added minus removed, deduplicated. */
  appids: number[];
  added: number;
  removed: number;
  hasFilter: boolean;
  filterSpec?: FilterSpec;
  isDeleted: boolean;
}

/** `id` is the key without the user-collections. prefix. */
export function categoryKind(id: string, filterOnly: boolean): CategoryKind {
  if (id.startsWith('from-tag-')) return 'tag';
  return filterOnly ? 'filter' : 'explicit';
}

function toAppids(value: unknown): number[] {
  return Array.isArray(value)
    ? value.filter((v): v is number => typeof v === 'number' && Number.isFinite(v))
    : [];
}

/**
 * Parse the raw text of cloud-storage-namespace-1.json into categories.
 * Tombstones are skipped unless includeDeleted is set. Malformed entries are
 * ignored; a malformed file yields an empty list.
 */
export function parseCategories(raw: string, includeDeleted = false): Category[] {
  let top: unknown;
  try {
    top = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(top)) return [];

  const result: Category[] = [];
  for (const pair of top) {
    if (!Array.isArray(pair) || typeof pair[0] !== 'string') continue;
    const key = pair[0];
    if (!key.startsWith(KEY_PREFIX)) continue;
    const entry = pair[1] as Record<string, unknown> | null;
    if (!entry || typeof entry !== 'object') continue;

    let value: Record<string, unknown> = {};
    if (typeof entry['value'] === 'string') {
      try {
        const parsed: unknown = JSON.parse(entry['value']);
        if (parsed && typeof parsed === 'object') value = parsed as Record<string, unknown>;
      } catch {
        continue;
      }
    }

    const isDeleted = entry['is_deleted'] === true || value['is_deleted'] === true;
    if (isDeleted && !includeDeleted) continue;

    const suffix = key.slice(KEY_PREFIX.length);
    const added = toAppids(value['added']);
    const removedList = toAppids(value['removed']);
    const removedSet = new Set(removedList);
    const appids = [...new Set(added)].filter((id) => !removedSet.has(id));

    const filterSpec =
      value['filterSpec'] && typeof value['filterSpec'] === 'object'
        ? (value['filterSpec'] as FilterSpec)
        : undefined;

    result.push({
      name: typeof value['name'] === 'string' ? value['name'] : suffix,
      key,
      kind: categoryKind(suffix, filterSpec !== undefined && added.length === 0),
      version: typeof entry['version'] === 'string' ? entry['version'] : null,
      timestamp: typeof entry['timestamp'] === 'number' ? entry['timestamp'] : null,
      appids,
      added: added.length,
      removed: removedList.length,
      hasFilter: filterSpec !== undefined,
      ...(filterSpec ? { filterSpec } : {}),
      isDeleted,
    });
  }
  return result;
}

/** Find a category by name (case-insensitive) or exact key. */
export function findCategory(categories: Category[], query: string): Category | undefined {
  const q = query.trim().toLowerCase();
  return (
    categories.find((c) => c.name.toLowerCase() === q) ??
    categories.find((c) => c.key.toLowerCase() === q)
  );
}

/**
 * Locate cloud-storage-namespace-1.json for the active account, falling back
 * to the first account that has one.
 */
export function findCategoriesFile(): string | null {
  const userdata = path.join(getSteamDir(), 'userdata');
  const rel = path.join('config', 'cloudstorage', 'cloud-storage-namespace-1.json');

  try {
    const active = path.join(userdata, String(getUserConfig().steamId32), rel);
    if (fs.existsSync(active)) return active;
  } catch {
    /* fall through to scanning */
  }

  let accounts: string[];
  try {
    accounts = fs.readdirSync(userdata).filter((d) => /^\d+$/.test(d));
  } catch {
    return null;
  }
  for (const account of accounts) {
    const candidate = path.join(userdata, account, rel);
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
}

/** Read categories from disk. Returns null when no file exists. */
export function readCategories(includeDeleted = false): Category[] | null {
  const file = findCategoriesFile();
  if (!file) return null;
  return parseCategories(fs.readFileSync(file, 'utf-8'), includeDeleted);
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/** An edit refused for a reason the caller should show as-is. */
export class CategoryError extends Error {}

export interface CategoryEdit {
  /** New file text; identical to the input when nothing changed. */
  raw: string;
  key: string;
  changed: boolean;
}

export interface MembershipEdit extends CategoryEdit {
  /** Appids actually added or removed. */
  affected: number[];
  /** Appids that were already members (add) or not members (remove). */
  skipped: number[];
}

type Pair = [string, Record<string, unknown>];

/** Favorites and Hidden have fixed ids and can't be renamed or deleted. */
const SPECIAL: Record<string, { id: string; name: string }> = {
  favorite: { id: 'favorite', name: 'Favorites' },
  favorites: { id: 'favorite', name: 'Favorites' },
  hidden: { id: 'hidden', name: 'Hidden' },
};
const SPECIAL_KEYS = new Set([`${KEY_PREFIX}favorite`, `${KEY_PREFIX}hidden`]);

const ID_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function randomCollectionId(): string {
  let id = '';
  for (let i = 0; i < 12; i++) id += ID_ALPHABET[randomInt(ID_ALPHABET.length)];
  return id;
}

/** Strict counterpart of parseCategories: a file we can't parse must not be rewritten. */
function loadPairs(raw: string): Pair[] {
  let top: unknown;
  try {
    top = JSON.parse(raw);
  } catch {
    throw new CategoryError('cloud-storage-namespace-1.json is not valid JSON; nothing was changed');
  }
  if (!Array.isArray(top)) {
    throw new CategoryError('cloud-storage-namespace-1.json is not a JSON array; nothing was changed');
  }
  return top as Pair[];
}

function pairIndex(pairs: Pair[], key: string): number {
  return pairs.findIndex((p) => Array.isArray(p) && p[0] === key);
}

function newEntry(key: string, value: object, now: number): Record<string, unknown> {
  return {
    key,
    timestamp: now,
    value: JSON.stringify(value),
    conflictResolutionMethod: 'custom',
    strMethodId: 'union-collections',
  };
}

function resolveCategory(raw: string, query: string): Category {
  const categories = parseCategories(raw);
  const category = findCategory(categories, query);
  if (!category) {
    throw new CategoryError(
      `Category "${query}" not found. Available: ${categories.map((c) => c.name).join(', ') || '(none)'}`,
    );
  }
  return category;
}

function assertNameFree(raw: string, name: string, exceptKey?: string): void {
  const lower = name.toLowerCase();
  if (SPECIAL[lower]) throw new CategoryError(`"${name}" is reserved for the built-in collection`);
  const clash = parseCategories(raw).find((c) => c.key !== exceptKey && c.name.toLowerCase() === lower);
  if (clash) throw new CategoryError(`A category named "${clash.name}" already exists`);
}

function assertNotSpecial(category: Category, verb: string): void {
  if (SPECIAL_KEYS.has(category.key)) {
    throw new CategoryError(`"${category.name}" is a built-in collection and can't be ${verb}`);
  }
}

function cleanName(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) throw new CategoryError('Category name must not be empty');
  return trimmed;
}

/**
 * Apply `mutate` to the parsed value of the category's entry and bump its
 * timestamp. Every other field and entry is kept as-is.
 */
function updateValue(
  raw: string,
  key: string,
  now: number,
  mutate: (value: Record<string, unknown>) => void,
): CategoryEdit {
  const pairs = loadPairs(raw);
  const meta = pairs[pairIndex(pairs, key)][1];
  const before = meta['value'];
  if (typeof before !== 'string') throw new CategoryError(`Entry ${key} has no value to edit`);
  const value = JSON.parse(before) as Record<string, unknown>;
  mutate(value);
  const after = JSON.stringify(value);
  if (after === before) return { raw, key, changed: false };
  meta['value'] = after;
  meta['timestamp'] = now;
  return { raw: JSON.stringify(pairs), key, changed: true };
}

function editMembership(
  raw: string,
  query: string,
  appids: number[],
  now: number,
  mode: 'add' | 'remove',
): MembershipEdit {
  const category = resolveCategory(raw, query);
  if (category.hasFilter) {
    throw new CategoryError(
      `"${category.name}" is a dynamic collection; its games come from filters and can't be ${mode === 'add' ? 'added' : 'removed'} by hand`,
    );
  }

  const members = new Set(category.appids);
  const affected: number[] = [];
  const skipped: number[] = [];
  for (const id of new Set(appids)) {
    (members.has(id) === (mode === 'remove') ? affected : skipped).push(id);
  }

  const targets = new Set(appids);
  const edit = updateValue(raw, category.key, now, (value) => {
    let added = Array.isArray(value['added']) ? (value['added'] as unknown[]) : [];
    if (mode === 'add') {
      added = [...added, ...affected.filter((id) => !added.includes(id))];
    } else {
      added = added.filter((id) => !targets.has(id as number));
    }
    value['added'] = added;
    if (Array.isArray(value['removed'])) {
      value['removed'] = (value['removed'] as unknown[]).filter((id) => !targets.has(id as number));
    }
  });
  return { ...edit, affected, skipped };
}

/**
 * Add appids to a category. Favorites and Hidden are created when missing,
 * as Steam only stores them once they have members.
 */
export function addApps(raw: string, query: string, appids: number[], now = nowSeconds()): MembershipEdit {
  const q = query.trim().toLowerCase();
  const special = SPECIAL[q.startsWith(KEY_PREFIX) ? q.slice(KEY_PREFIX.length) : q];
  if (special && !findCategory(parseCategories(raw), query)) {
    const pairs = loadPairs(raw);
    const key = KEY_PREFIX + special.id;
    const added = [...new Set(appids)];
    const entry = newEntry(key, { id: special.id, name: special.name, added, removed: [] }, now);
    const index = pairIndex(pairs, key);
    if (index === -1) {
      pairs.push([key, entry]);
    } else {
      // Revive the tombstone, keeping its server version.
      const version = pairs[index][1]['version'];
      pairs[index] = [key, version !== undefined ? { ...entry, version } : entry];
    }
    return { raw: JSON.stringify(pairs), key, changed: true, affected: added, skipped: [] };
  }
  return editMembership(raw, query, appids, now, 'add');
}

/** Remove appids from a category the way Steam does: drop them from `added`. */
export function removeApps(raw: string, query: string, appids: number[], now = nowSeconds()): MembershipEdit {
  return editMembership(raw, query, appids, now, 'remove');
}

/** Append a new, empty custom collection. */
export function createCategory(
  raw: string,
  name: string,
  now = nowSeconds(),
  id = randomCollectionId(),
): CategoryEdit {
  const cleaned = cleanName(name);
  assertNameFree(raw, cleaned);
  const pairs = loadPairs(raw);
  const collectionId = `uc-${id}`;
  const key = KEY_PREFIX + collectionId;
  if (pairIndex(pairs, key) !== -1) throw new CategoryError(`Key ${key} is already taken`);
  pairs.push([key, newEntry(key, { id: collectionId, name: cleaned, added: [], removed: [] }, now)]);
  return { raw: JSON.stringify(pairs), key, changed: true };
}

export function renameCategory(raw: string, query: string, newName: string, now = nowSeconds()): CategoryEdit {
  const cleaned = cleanName(newName);
  const category = resolveCategory(raw, query);
  assertNotSpecial(category, 'renamed');
  assertNameFree(raw, cleaned, category.key);
  return updateValue(raw, category.key, now, (value) => {
    value['name'] = cleaned;
  });
}

/** Replace the entry with a tombstone, the shape Steam itself writes. */
export function deleteCategory(raw: string, query: string, now = nowSeconds()): CategoryEdit {
  const category = resolveCategory(raw, query);
  assertNotSpecial(category, 'deleted');
  const pairs = loadPairs(raw);
  const index = pairIndex(pairs, category.key);
  const version = pairs[index][1]['version'];
  pairs[index] = [
    category.key,
    { key: category.key, timestamp: now, is_deleted: true, ...(version !== undefined ? { version } : {}) },
  ];
  return { raw: JSON.stringify(pairs), key: category.key, changed: true };
}

function writeAtomic(file: string, content: string): void {
  const tmp = `${file}.tmp-${process.pid}`;
  try {
    fs.writeFileSync(tmp, content, 'utf-8');
    fs.renameSync(tmp, file);
  } catch (error) {
    fs.rmSync(tmp, { force: true });
    throw error;
  }
}

/**
 * Write new namespace text: back up the old file to .bak, replace it
 * atomically and list `key` in the .modified.json sibling so Steam uploads it.
 */
export function writeCategoriesFile(file: string, raw: string, key: string): void {
  const modifiedPath = file.replace(/\.json$/, '.modified.json');
  let modified: unknown = [];
  if (fs.existsSync(modifiedPath)) {
    try {
      modified = JSON.parse(fs.readFileSync(modifiedPath, 'utf-8'));
    } catch {
      modified = null;
    }
    if (!Array.isArray(modified)) {
      throw new CategoryError(`${path.basename(modifiedPath)} is not a JSON array; nothing was written`);
    }
  }
  const keys = modified as unknown[];

  // Mark first: a marked but unchanged key is harmless, an unmarked edit may be
  // replaced by the cloud copy.
  if (!keys.includes(key)) writeAtomic(modifiedPath, JSON.stringify([...keys, key]));
  fs.copyFileSync(file, `${file}.bak`);
  writeAtomic(file, raw);
}

/**
 * The refusal for file edits under a running Steam. `bridgeStatus` says why
 * the Millennium bridge couldn't take the edit instead, when known.
 */
export function steamRunningError(bridgeStatus?: string): CategoryError {
  return new CategoryError(
    [
      'WARNING: Steam is currently running!',
      'Steam keeps categories in memory and overwrites cloud-storage-namespace-1.json when it exits.',
      'Please close Steam first, then try again, or install the steam-mcp-bridge Millennium plugin',
      '(millennium-plugin/ in the steam-mcp repo) to edit categories while Steam runs.',
      ...(bridgeStatus ? [`Millennium bridge: ${bridgeStatus}.`] : []),
      '',
      'Categories were NOT modified.',
    ].join('\n'),
  );
}

/**
 * Run an edit against the active account's categories file and write the
 * result. Refuses while Steam is running: it would overwrite the file from
 * memory on exit.
 */
export function editCategories<T extends CategoryEdit>(edit: (raw: string) => T): T {
  const file = findCategoriesFile();
  if (!file) {
    throw new CategoryError(
      'No cloud-storage-namespace-1.json found. Open the Steam library once so Steam creates it.',
    );
  }
  if (isSteamRunning()) throw steamRunningError();
  const result = edit(fs.readFileSync(file, 'utf-8'));
  if (result.changed) writeCategoriesFile(file, result.raw, result.key);
  return result;
}
