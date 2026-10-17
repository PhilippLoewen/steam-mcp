import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';
import { getLibraries } from '../steam/library.js';
import { readAllManifests } from '../steam/manifests.js';
import { getAllPlaytimes } from '../steam/userdata.js';
import { steamApiRequest, storeApiRequest, hasApiKey } from '../steam/api.js';
import type { OwnedGamesResponse } from '../steam/api-types.js';
import { getUserConfig } from '../steam/paths.js';
import { findCategory, readCategories } from '../steam/categories.js';
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

export function registerLibraryTools(server: McpServer): void {
  server.tool(
    'library',
    'List library folders, get aggregate stats, or read library categories (Steam collections: Favorites, Hidden, tag/custom shelves)',
    {
      action: z.enum(['list', 'stats', 'categories', 'category']).default('list'),
      name: z.string().optional().describe('Category name or key (for action "category")'),
      includeDeleted: z.boolean().optional().describe('Include deleted categories (for "categories")'),
    },
    async (params) => {
      switch (params.action) {
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
            const names = await resolveAppNames(category.appids);
            return textResult({
              name: category.name,
              key: category.key,
              kind: category.kind,
              version: category.version,
              timestamp: category.timestamp,
              appids: category.appids.map((appid) => ({ appid, name: names.get(appid) ?? null })),
              ...(category.filterSpec ? { filterSpec: category.filterSpec } : {}),
            });
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
