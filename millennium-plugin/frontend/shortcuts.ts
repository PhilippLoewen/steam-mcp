/**
 * Non-Steam shortcuts listed, added and removed through the Steam client's own
 * SteamClient.Apps calls. Steam keeps shortcuts in memory and rewrites
 * shortcuts.vdf from it, so an edit made here sticks while Steam runs.
 */

import {
	type AppDetails,
	DETAILS_TIMEOUT_MS,
	requireAppid,
	requireString,
	serialize,
	steamApps,
	waitForDetails,
	withTimeout,
} from './steam-apps';

export type Request =
	| { action: 'add_shortcut'; name: string; exe: string; startDir: string; launchOptions: string }
	| { action: 'remove_shortcut'; appid: number }
	| { action: 'list_shortcuts' };

export interface Shortcut {
	appid: number;
	name: string;
	exe: string;
	startDir: string;
	launchOptions: string;
}

export interface ListedShortcut extends Shortcut {
	/** Unix seconds; 0 when never played. */
	lastPlayed: number;
}

export const ACTIONS: readonly string[] = ['add_shortcut', 'remove_shortcut', 'list_shortcuts'];

interface AppOverview {
	appid: number;
	display_name?: string;
	rt_last_time_played?: number;
	app_type?: number;
	BIsShortcut?(): boolean;
}

interface AppStore {
	GetAppOverviewByAppID?(appid: number): unknown;
	allApps?: AppOverview[];
}

/** EAppType.Shortcut, for overviews without BIsShortcut. */
const SHORTCUT_APP_TYPE = 1073741824;

function describe(appid: number, details: AppDetails): Shortcut {
	return {
		appid,
		name: details.strDisplayName ?? '',
		exe: details.strShortcutExe ?? '',
		startDir: details.strShortcutStartDir ?? '',
		launchOptions: details.strShortcutLaunchOptions ?? '',
	};
}

/** Without one pair of surrounding double quotes, which Steam may add or drop. Only for confirming a write. */
const unquote = (value: string | undefined) => (value ?? '').replace(/^"(.*)"$/s, '$1');

const sameShortcut = (want: Omit<Shortcut, 'appid'>) => (details: AppDetails) =>
	details.strDisplayName === want.name &&
	unquote(details.strShortcutExe) === unquote(want.exe) &&
	unquote(details.strShortcutStartDir) === unquote(want.startDir) &&
	(details.strShortcutLaunchOptions ?? '') === want.launchOptions;

/**
 * Add a shortcut, then set each field: AddShortcut doesn't reliably take the
 * name, start folder and launch options it is given. Steam picks the appid.
 */
async function addShortcut(want: Omit<Shortcut, 'appid'>): Promise<Shortcut> {
	const apps = steamApps(
		'RegisterForAppDetails',
		'AddShortcut',
		'SetShortcutName',
		'SetShortcutExe',
		'SetShortcutStartDir',
		'SetShortcutLaunchOptions',
	);
	const appid = await withTimeout(
		apps.AddShortcut(want.name, want.exe, want.startDir, want.launchOptions),
		`Steam did not return an appid for the new shortcut within ${DETAILS_TIMEOUT_MS / 1000}s; it may still have been added, check the library before retrying`,
	);
	if (!Number.isSafeInteger(appid) || appid <= 0) {
		throw new Error(`Steam did not return an appid for the new shortcut (got ${JSON.stringify(appid)})`);
	}
	let confirmation;
	try {
		// Register before writing, so the confirming update can't be missed.
		const confirmed = waitForDetails(appid, sameShortcut(want));
		apps.SetShortcutName(appid, want.name);
		apps.SetShortcutExe(appid, want.exe);
		apps.SetShortcutStartDir(appid, want.startDir);
		apps.SetShortcutLaunchOptions(appid, want.launchOptions);
		confirmation = await confirmed;
	} catch (error) {
		const why = error instanceof Error ? error.message : String(error);
		throw new Error(
			`Steam added shortcut appid ${appid}, but setting it up failed (${why}); check it in the library instead of adding it again`,
		);
	}
	const { details, matched } = confirmation;
	if (!matched) {
		const seen = details ? ` (Steam last reported ${JSON.stringify(describe(appid, details))})` : '';
		throw new Error(
			`Steam added shortcut appid ${appid} but did not confirm its settings within ${DETAILS_TIMEOUT_MS / 1000}s${seen}; check it in the library instead of adding it again`,
		);
	}
	return describe(appid, details as AppDetails);
}

function overviewOf(appid: number): unknown {
	const appStore = (window as unknown as { appStore?: AppStore }).appStore;
	if (typeof appStore?.GetAppOverviewByAppID !== 'function') {
		throw new Error("Steam's appStore is not available, so a removal couldn't be confirmed; close Steam and remove again");
	}
	return appStore.GetAppOverviewByAppID(appid);
}

/** Remove a shortcut, refusing apps that aren't one; answers with what it was. */
async function removeShortcut(appid: number): Promise<Shortcut> {
	const apps = steamApps('RegisterForAppDetails', 'RemoveShortcut');
	overviewOf(appid);
	const { details } = await waitForDetails(appid, () => true);
	if (!details?.strShortcutExe) {
		throw new Error(`Steam knows no non-Steam shortcut with appid ${appid}`);
	}
	const removed = describe(appid, details);
	apps.RemoveShortcut(appid);
	for (let waited = 0; overviewOf(appid); waited += 100) {
		if (waited >= DETAILS_TIMEOUT_MS) {
			throw new Error(
				`Steam did not confirm removing shortcut appid ${appid} within ${DETAILS_TIMEOUT_MS / 1000}s; check it before retrying`,
			);
		}
		await new Promise((resolve) => setTimeout(resolve, 100));
	}
	return removed;
}

/**
 * The shortcuts Steam holds now. shortcuts.vdf trails them by a few seconds
 * after a change, so steam-mcp reads them here while Steam runs.
 */
async function listShortcuts(): Promise<{ shortcuts: ListedShortcut[] }> {
	steamApps('RegisterForAppDetails');
	const allApps = (window as unknown as { appStore?: AppStore }).appStore?.allApps;
	if (!Array.isArray(allApps)) {
		throw new Error("Steam's appStore has no app list to read shortcuts from");
	}
	const isShortcut = (app: AppOverview) =>
		typeof app.BIsShortcut === 'function' ? app.BIsShortcut() : app.app_type === SHORTCUT_APP_TYPE;
	// Details for all of them at once: each wait ends within DETAILS_TIMEOUT_MS.
	const listed = await Promise.all(
		allApps.filter(isShortcut).map(async (app): Promise<ListedShortcut | null> => {
			const { details } = await waitForDetails(app.appid, () => true);
			if (!details) {
				// Removed while listing: leave it out. Still there: its program and
				// folder are unknown, and listing them empty would be wrong.
				const appStore = (window as unknown as { appStore?: AppStore }).appStore;
				const lookup = appStore?.GetAppOverviewByAppID;
				const stillThere = typeof lookup === 'function' ? Boolean(lookup.call(appStore, app.appid)) : true;
				if (!stillThere) return null;
				throw new Error(
					`Steam sent no details for shortcut appid ${app.appid} within ${DETAILS_TIMEOUT_MS / 1000}s; try listing again`,
				);
			}
			const shortcut = describe(app.appid, details);
			return {
				...shortcut,
				name: shortcut.name || app.display_name || '',
				lastPlayed: app.rt_last_time_played ?? 0,
			};
		}),
	);
	return { shortcuts: listed.filter((s): s is ListedShortcut => s !== null) };
}

/**
 * Check a shortcut request. Only its shape: which programs and launch options
 * are acceptable is steam-mcp's check, and a program that can reach
 * Millennium's socket could write shortcuts.vdf just as well.
 */
export function parse(fields: Record<string, unknown>): Request {
	if (fields.action === 'list_shortcuts') return { action: 'list_shortcuts' };
	if (fields.action === 'remove_shortcut') return { action: 'remove_shortcut', appid: requireAppid(fields.appid) };
	if (fields.action !== 'add_shortcut') throw new Error(`Unknown action ${JSON.stringify(fields.action)}`);
	return {
		action: 'add_shortcut',
		name: requireString(fields.name, 'name', true),
		exe: requireString(fields.exe, 'exe', true),
		startDir: requireString(fields.startDir, 'startDir'),
		launchOptions: requireString(fields.launchOptions, 'launchOptions'),
	};
}

export function handle(request: Request): Promise<Shortcut | { shortcuts: ListedShortcut[] }> {
	switch (request.action) {
		case 'add_shortcut':
			return serialize(() => addShortcut(request));
		case 'remove_shortcut':
			return serialize(() => removeShortcut(request.appid));
		case 'list_shortcuts':
			// A read: it doesn't wait for queued writes.
			return listShortcuts();
	}
}
