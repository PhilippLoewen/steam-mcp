import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';
import { getLibraries } from '../steam/library.js';
import { readAllManifests } from '../steam/manifests.js';
import { getAllPlaytimes } from '../steam/userdata.js';
import { steamApiRequest, storeApiRequest, hasApiKey } from '../steam/api.js';
import type { OwnedGamesResponse } from '../steam/api-types.js';
import { getUserConfig, isSteamRunning } from '../steam/paths.js';
import {
  addApps,
  type Category,
  type CategoryEdit,
  CategoryError,
  createCategory,
  deleteCategory,
  editCategories,
  findCategory,
  KEY_PREFIX,
  type MembershipEdit,
  parseCategories,
  readCategories,
  removeApps,
  renameCategory,
  steamRunningError,
} from '../steam/categories.js';
import {
  bridgeCategory,
  type BridgeRequest,
  BridgeTimeoutError,
  runBridgeEdit,
  verifyBridgeEdit,
} from '../steam/millennium.js';
import { formatBytes, formatPlaytime, formatTimestamp } from '../util/format.js';

/**
 * Best-effort appid -> name: local manifests, then owned games (API key),
 * then the public store API. Unresolvable appids are simply absent.
 */
async function resolveAppNames(appids: number[]): Promise<Map<number, string>> {
  const names = new Map<number, string>();
  if (appids.length === 0) return names;

  try {
    for (const m of await readAllManifests()) names.set(m.appid, m.name);
  } catch { /* no local manifests */ }

  if (appids.some((id) => !names.has(id)) && hasApiKey()) {
    try {
      const owned = await steamApiRequest<OwnedGamesResponse>(
        'IPlayerService',
        'GetOwnedGames',
        'v1',
        { steamid: getUserConfig().steamId64, include_appinfo: 1, include_played_free_games: 1 },
      );
      for (const g of owned.response.games ?? []) {
        if (!names.has(g.appid)) names.set(g.appid, g.name);
      }
    } catch { /* fall through to store API */ }
  }

  const missing = appids.filter((id) => !names.has(id));
  await Promise.allSettled(
    missing.map(async (appid) => {
      const data = await storeApiRequest<
        Record<string, { success: boolean; data?: { name?: string } }>
      >('/api/appdetails', { appids: appid, filters: 'basic' });
      const entry = data[String(appid)];
      if (entry?.success && entry.data?.name) names.set(appid, entry.data.name);
    }),
  );
  return names;
}

function textResult(value: unknown) {
  return { content: [{ type: 'text' as const, text: JSON.stringify(value, null, 2) }] };
}

function errorResult(message: string) {
  return { content: [{ type: 'text' as const, text: message }], isError: true };
}

async function describeCategory(category: Category) {
  const names = await resolveAppNames(category.appids);
  return {
    name: category.name,
    key: category.key,
    kind: category.kind,
    version: category.version,
    timestamp: category.timestamp,
    appids: category.appids.map((appid) => ({ appid, name: names.get(appid) ?? null })),
    ...(category.filterSpec ? { filterSpec: category.filterSpec } : {}),
  };
}

/** Result field names for the appids a membership edit changed and skipped. */
interface MembershipFields {
  affected: string;
  skipped: string;
}

/**
 * Report a bridge edit the plugin never acknowledged by what the categories
 * file shows. Still an error when the file doesn't show it done: Steam may
 * not have saved it yet, or the edit never ran.
 */
async function readBackBridgeEdit(request: BridgeRequest, timeout: BridgeTimeoutError) {
  const { applied, category } = verifyBridgeEdit(request, readCategories() ?? []);
  if (!applied) {
    return errorResult(`${timeout.message} The categories file doesn't show it yet; check again before retrying.`);
  }
  return textResult({
    changed: null,
    via: 'millennium',
    verifiedByReadback: true,
    note: `${timeout.message} The categories file shows it applied.`,
    ...(category ? { category: await describeCategory(category) } : {}),
  });
}

/**
 * Run a category edit and report the edited category (or just its key once
 * deleted). While Steam runs, `request` goes through the Millennium bridge;
 * otherwise `fileEdit` rewrites the categories file.
 */
export async function runCategoryEdit(
  action: string,
  request: BridgeRequest,
  fileEdit: (raw: string) => CategoryEdit & Partial<Pick<MembershipEdit, 'affected' | 'skipped'>>,
  membershipFields?: MembershipFields,
) {
  const membership = (r: { affected?: number[]; skipped?: number[] }) =>
    membershipFields
      ? { [membershipFields.affected]: r.affected ?? [], [membershipFields.skipped]: r.skipped ?? [] }
      : {};

  try {
    if (isSteamRunning()) {
      let bridged: Awaited<ReturnType<typeof runBridgeEdit>>;
      try {
        bridged = await runBridgeEdit(request);
      } catch (error) {
        if (!(error instanceof BridgeTimeoutError)) throw error;
        return readBackBridgeEdit(request, error);
      }
      if ('unavailable' in bridged) throw steamRunningError(bridged.unavailable);
      return textResult({
        changed: bridged.changed,
        via: 'millennium',
        ...membership(bridged),
        ...(bridged.collection
          ? { category: await describeCategory(bridgeCategory(bridged.collection)) }
          : { key: KEY_PREFIX + bridged.id }),
      });
    }

    const result = editCategories(fileEdit);
    const category = parseCategories(result.raw).find((c) => c.key === result.key);
    return textResult({
      changed: result.changed,
      ...membership(result),
      ...(category ? { category: await describeCategory(category) } : { key: result.key }),
    });
  } catch (error) {
    if (error instanceof CategoryError) return errorResult(error.message);
    const msg = error instanceof Error ? error.message : String(error);
    return errorResult(`Error in ${action}: ${msg}`);
  }
}

function requiredParam(names: string, action: string) {
  return errorResult(`Action "${action}" requires ${names}`);
}

export function registerLibraryTools(server: McpServer): void {
  server.tool(
    'library',
    'List library folders, get aggregate stats, or read and edit library categories (Steam collections: Favorites, Hidden, tag/custom shelves). Edits need Steam closed, or the steam-mcp-bridge Millennium plugin while it runs.',
    {
      action: z
        .enum([
          'list',
          'stats',
          'categories',
          'category',
          'category_add',
          'category_remove',
          'category_create',
          'category_rename',
          'category_delete',
        ])
        .default('list'),
      name: z.string().optional().describe('Category name or key (for "category" and "category_*")'),
      appids: z.array(z.number().int()).optional().describe('Appids to add or remove (for "category_add"/"category_remove")'),
      newName: z.string().optional().describe('New name (for "category_rename")'),
      includeDeleted: z.boolean().optional().describe('Include deleted categories (for "categories")'),
    },
    async (params) => {
      switch (params.action) {
        case 'category_add': {
          const { name, appids } = params;
          if (!name || !appids?.length) return requiredParam('"name" and "appids"', params.action);
          return runCategoryEdit(
            params.action,
            { action: 'add', name, appids },
            (raw) => addApps(raw, name, appids),
            { affected: 'added', skipped: 'alreadyPresent' },
          );
        }

        case 'category_remove': {
          const { name, appids } = params;
          if (!name || !appids?.length) return requiredParam('"name" and "appids"', params.action);
          return runCategoryEdit(
            params.action,
            { action: 'remove', name, appids },
            (raw) => removeApps(raw, name, appids),
            { affected: 'removed', skipped: 'notPresent' },
          );
        }

        case 'category_create': {
          const { name } = params;
          if (!name) return requiredParam('"name"', params.action);
          return runCategoryEdit(params.action, { action: 'create', name }, (raw) => createCategory(raw, name));
        }

        case 'category_rename': {
          const { name, newName } = params;
          if (!name || !newName) return requiredParam('"name" and "newName"', params.action);
          return runCategoryEdit(params.action, { action: 'rename', name, newName }, (raw) =>
            renameCategory(raw, name, newName),
          );
        }

        case 'category_delete': {
          const { name } = params;
          if (!name) return requiredParam('"name"', params.action);
          return runCategoryEdit(params.action, { action: 'delete', name }, (raw) => deleteCategory(raw, name));
        }

        case 'categories': {
          try {
            const categories = readCategories(params.includeDeleted ?? false);
            if (!categories || categories.length === 0) {
              return textResult({
                collections: [],
                note: 'No cloud-storage-namespace-1.json found or it contains no categories',
              });
            }
            return textResult({
              collections: categories.map((c) => ({
                name: c.name,
                key: c.key,
                kind: c.kind,
                added: c.added,
                removed: c.removed,
                hasFilter: c.hasFilter,
                ...(params.includeDeleted ? { isDeleted: c.isDeleted } : {}),
              })),
            });
          } catch (error) {
            const msg = error instanceof Error ? error.message : String(error);
            return errorResult(`Error listing categories: ${msg}`);
          }
        }

        case 'category': {
          if (!params.name) return errorResult('The "name" parameter is required for action "category"');
          try {
            const categories = readCategories(params.includeDeleted ?? false) ?? [];
            const category = findCategory(categories, params.name);
            if (!category) {
              return errorResult(
                `Category "${params.name}" not found. Available: ${categories.map((c) => c.name).join(', ') || '(none)'}`,
              );
            }
            return textResult(await describeCategory(category));
          } catch (error) {
            const msg = error instanceof Error ? error.message : String(error);
            return errorResult(`Error reading category: ${msg}`);
          }
        }

        case 'list': {
          try {
            const libraries = getLibraries();

            const results = libraries.map((lib) => ({
              path: lib.path,
              label: lib.label,
              totalSize: formatBytes(lib.totalSize),
              freeSpace: formatBytes(lib.freeSpace),
              gameCount: lib.appids.length,
              mounted: lib.mounted,
            }));

            return {
              content: [{ type: 'text' as const, text: JSON.stringify(results, null, 2) }],
            };
          } catch (error) {
            const msg = error instanceof Error ? error.message : String(error);
            return {
              content: [{ type: 'text' as const, text: `Error listing libraries: ${msg}` }],
              isError: true,
            };
          }
        }

        case 'stats': {
          try {
            const libraries = getLibraries();
            const manifests = await readAllManifests();

            // Total stats
            const totalGames = manifests.length;
            const totalSize = manifests.reduce((sum, m) => sum + m.sizeOnDisk, 0);

            // Games per library
            const gamesPerLibrary = libraries.map((lib) => ({
              path: lib.path,
              label: lib.label,
              gameCount: lib.appids.length,
              totalSize: formatBytes(
                manifests
                  .filter((m) => m.libraryPath === lib.path)
                  .reduce((sum, m) => sum + m.sizeOnDisk, 0),
              ),
            }));

            // Top 10 by size
            const sortedBySize = [...manifests].sort(
              (a, b) => b.sizeOnDisk - a.sizeOnDisk,
            );
            const top10BySize = sortedBySize.slice(0, 10).map((m) => ({
              appid: m.appid,
              name: m.name,
              size: formatBytes(m.sizeOnDisk),
            }));

            // Top 10 recently played
            const sortedByPlayed = [...manifests]
              .filter((m) => m.lastPlayed > 0)
              .sort((a, b) => b.lastPlayed - a.lastPlayed);
            const top10RecentlyPlayed = sortedByPlayed.slice(0, 10).map((m) => ({
              appid: m.appid,
              name: m.name,
              lastPlayed: formatTimestamp(m.lastPlayed),
            }));

            // Pile of shame: installed but never played (lastPlayed === 0)
            const pileOfShame = manifests
              .filter((m) => m.lastPlayed === 0)
              .sort((a, b) => a.name.localeCompare(b.name))
              .map((m) => ({
                appid: m.appid,
                name: m.name,
                size: formatBytes(m.sizeOnDisk),
              }));

            // Total playtime (best effort) — parse localconfig.vdf once
            const allPlaytimes = getAllPlaytimes();
            const playtimeMap = new Map(allPlaytimes.map(p => [p.appid, p.playtime]));
            let totalPlaytimeMinutes = 0;
            for (const m of manifests) {
              totalPlaytimeMinutes += playtimeMap.get(m.appid) ?? 0;
            }

            const output = {
              totalGames,
              totalSize: formatBytes(totalSize),
              totalPlaytime: formatPlaytime(totalPlaytimeMinutes),
              gamesPerLibrary,
              top10BySize,
              top10RecentlyPlayed,
              pileOfShame: {
                count: pileOfShame.length,
                games: pileOfShame,
              },
            };

            return {
              content: [{ type: 'text' as const, text: JSON.stringify(output, null, 2) }],
            };
          } catch (error) {
            const msg = error instanceof Error ? error.message : String(error);
            return {
              content: [{ type: 'text' as const, text: `Error getting library stats: ${msg}` }],
              isError: true,
            };
          }
        }
      }
    },
  );
}
