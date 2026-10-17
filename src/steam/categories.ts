import fs from 'node:fs';
import path from 'node:path';
import { getSteamDir, getUserConfig } from './paths.js';

/**
 * Library categories ("user collections") as stored by the Steam client in
 * userdata/<accountid>/config/cloudstorage/cloud-storage-namespace-1.json.
 *
 * That file is a cloud-synced cache owned by the Steam client: this module
 * only ever reads it.
 */

const KEY_PREFIX = 'user-collections.';

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

    const kind: CategoryKind = suffix.startsWith('from-tag-')
      ? 'tag'
      : filterSpec && added.length === 0
        ? 'filter'
        : 'explicit';

    result.push({
      name: typeof value['name'] === 'string' ? value['name'] : suffix,
      key,
      kind,
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

/** Read categories from disk (read-only). Returns null when no file exists. */
export function readCategories(includeDeleted = false): Category[] | null {
  const file = findCategoriesFile();
  if (!file) return null;
  return parseCategories(fs.readFileSync(file, 'utf-8'), includeDeleted);
}
