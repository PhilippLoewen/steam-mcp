/**
 * Category edits against the Steam client's in-memory collectionStore. Steam
 * saves and cloud-syncs these itself, so it can stay open.
 *
 * The rules mirror the file-based edits in steam-mcp's src/steam/categories.ts.
 */

interface AppOverview {
	appid: number;
}

interface Collection {
	id: string;
	displayName: string;
	bIsDynamic: boolean;
	bAllowsDragAndDrop: boolean;
	apps: { has(appid: number): boolean; keys(): IterableIterator<number> };
	AsDragDropCollection(): { AddApps(overviews: AppOverview[]): void; RemoveApps(overviews: AppOverview[]): void };
	Save(): Promise<void>;
	Delete(): Promise<void>;
	/** Internal field behind displayName; no public setter is known. */
	m_strName?: string;
}

interface CollectionStore {
	userCollections: Collection[];
	GetCollection(id: string): Collection | undefined;
	NewUnsavedCollection(name: string, filter: unknown, overviews: AppOverview[]): Collection | undefined;
	SetAppsAsHidden(appids: number[], hide: boolean): unknown;
}

interface AppStore {
	GetAppOverviewByAppID?(appid: number): AppOverview | null | undefined;
	m_mapApps?: { get(appid: number): AppOverview | undefined };
}

export type Request =
	| { action: 'add' | 'remove'; name: string; appids: number[] }
	| { action: 'create' | 'delete'; name: string }
	| { action: 'rename'; name: string; newName: string };

export interface BridgeCollection {
	id: string;
	name: string;
	dynamic: boolean;
	appids: number[];
}

export interface BridgeResult {
	changed: boolean;
	id: string;
	collection?: BridgeCollection;
	affected?: number[];
	skipped?: number[];
}

const KEY_PREFIX = 'user-collections.';
const SPECIAL: Record<string, string> = { favorite: 'favorite', favorites: 'favorite', hidden: 'hidden' };

const steam = window as unknown as { collectionStore?: CollectionStore; appStore?: AppStore };

function store(): CollectionStore {
	if (!steam.collectionStore) throw new Error('Steam collectionStore is not available yet');
	return steam.collectionStore;
}

function describe(collection: Collection): BridgeCollection {
	return {
		id: collection.id,
		name: collection.displayName,
		dynamic: collection.bIsDynamic,
		appids: [...collection.apps.keys()],
	};
}

/** By name (case-insensitive) or full user-collections.<id> key, like the file route's findCategory. */
function resolve(query: string): Collection {
	const trimmed = query.trim();
	const q = trimmed.toLowerCase();
	const collections = store().userCollections;
	const keyId = q.startsWith(KEY_PREFIX) ? trimmed.slice(KEY_PREFIX.length) : undefined;
	// Favorites and Hidden also by their English names; Steam may show them localized.
	const special = SPECIAL[keyId?.toLowerCase() ?? q];
	const found =
		collections.find((c) => c.displayName.toLowerCase() === q) ??
		(keyId !== undefined ? store().GetCollection(keyId) : undefined) ??
		(special ? store().GetCollection(special) : undefined);
	if (!found) {
		const available = collections.map((c) => c.displayName).join(', ') || '(none)';
		throw new Error(`Category "${query}" not found. Available: ${available}`);
	}
	return found;
}

function cleanName(name: string): string {
	const trimmed = name.trim();
	if (!trimmed) throw new Error('Category name must not be empty');
	return trimmed;
}

function assertNameFree(name: string, exceptId?: string): void {
	const lower = name.toLowerCase();
	if (SPECIAL[lower]) throw new Error(`"${name}" is reserved for the built-in collection`);
	const clash = store().userCollections.find((c) => c.id !== exceptId && c.displayName.toLowerCase() === lower);
	if (clash) throw new Error(`A category named "${clash.displayName}" already exists`);
}

function assertNotSpecial(collection: Collection, verb: string): void {
	if (collection.id === 'favorite' || collection.id === 'hidden') {
		throw new Error(`"${collection.displayName}" is a built-in collection and can't be ${verb}`);
	}
}

function overviews(appids: number[]): AppOverview[] {
	const appStore = steam.appStore;
	const missing: number[] = [];
	const found: AppOverview[] = [];
	for (const appid of appids) {
		const overview = appStore?.GetAppOverviewByAppID?.(appid) ?? appStore?.m_mapApps?.get(appid);
		if (overview) found.push(overview);
		else missing.push(appid);
	}
	if (missing.length) throw new Error(`Steam knows no app with appid ${missing.join(', ')} in this library`);
	return found;
}

async function editMembership(query: string, appids: number[], mode: 'add' | 'remove'): Promise<BridgeResult> {
	const collection = resolve(query);
	if (collection.bIsDynamic) {
		throw new Error(
			`"${collection.displayName}" is a dynamic collection; its games come from filters and can't be ${mode === 'add' ? 'added' : 'removed'} by hand`,
		);
	}

	const affected: number[] = [];
	const skipped: number[] = [];
	for (const appid of new Set(appids)) {
		(collection.apps.has(appid) === (mode === 'remove') ? affected : skipped).push(appid);
	}

	if (affected.length) {
		if (collection.id === 'hidden') {
			await store().SetAppsAsHidden(affected, mode === 'add');
		} else {
			const drag = collection.AsDragDropCollection();
			if (mode === 'add') drag.AddApps(overviews(affected));
			else drag.RemoveApps(overviews(affected));
			await collection.Save();
		}
	}

	const updated = store().GetCollection(collection.id) ?? collection;
	return { changed: affected.length > 0, id: updated.id, collection: describe(updated), affected, skipped };
}

async function create(name: string): Promise<BridgeResult> {
	const cleaned = cleanName(name);
	assertNameFree(cleaned);
	const created = store().NewUnsavedCollection(cleaned, undefined, []);
	if (!created) throw new Error(`Steam refused to create category "${cleaned}"`);
	await created.Save();
	const saved = store().userCollections.find((c) => c.displayName === cleaned) ?? created;
	return { changed: true, id: saved.id, collection: describe(saved) };
}

async function rename(query: string, newName: string): Promise<BridgeResult> {
	const cleaned = cleanName(newName);
	const collection = resolve(query);
	assertNotSpecial(collection, 'renamed');
	assertNameFree(cleaned, collection.id);
	if (collection.displayName === cleaned) {
		return { changed: false, id: collection.id, collection: describe(collection) };
	}
	if (typeof collection.m_strName !== 'string') {
		throw new Error('Renaming is not supported by this Steam client version; close Steam and rename again');
	}
	collection.m_strName = cleaned;
	await collection.Save();
	if (collection.displayName !== cleaned) {
		throw new Error('Steam did not accept the new name; close Steam and rename again');
	}
	return { changed: true, id: collection.id, collection: describe(collection) };
}

async function deleteCollection(query: string): Promise<BridgeResult> {
	const collection = resolve(query);
	assertNotSpecial(collection, 'deleted');
	await collection.Delete();
	return { changed: true, id: collection.id };
}

export function handle(request: Request): Promise<BridgeResult> {
	switch (request.action) {
		case 'add':
		case 'remove':
			return editMembership(request.name, request.appids, request.action);
		case 'create':
			return create(request.name);
		case 'rename':
			return rename(request.name, request.newName);
		case 'delete':
			return deleteCollection(request.name);
		default:
			return Promise.reject(new Error(`Unknown action ${(request as { action: string }).action}`));
	}
}
