/**
 * Launch options reads and writes through the Steam client's own SteamClient.Apps
 * calls. Steam keeps launch options in memory and rewrites localconfig.vdf from
 * it, so a write made here sticks while Steam runs.
 */

interface AppDetails {
	strLaunchOptions?: string;
	strShortcutExe?: string;
}

interface Apps {
	RegisterForAppDetails(appid: number, callback: (details: AppDetails) => void): { unregister(): void };
	SetAppLaunchOptions(appid: number, options: string): void;
}

export type Request =
	| { action: 'get_launch_options'; appid: number }
	| { action: 'set_launch_options'; appid: number; options: string; expected?: string };

export interface LaunchOptionsResult {
	appid: number;
	launchOptions: string;
	changed: boolean;
}

export const ACTIONS: readonly string[] = ['get_launch_options', 'set_launch_options'];

/** Longest launch options a request may carry. */
const MAX_OPTIONS_LENGTH = 4096;
/** How long Steam gets to send an app's details, and to confirm a write. */
const DETAILS_TIMEOUT_MS = 2000;

function apps(): Apps {
	const client = (window as unknown as { SteamClient?: { Apps?: Partial<Apps> } }).SteamClient?.Apps;
	for (const name of ['RegisterForAppDetails', 'SetAppLaunchOptions'] as const) {
		if (typeof client?.[name] !== 'function') {
			throw new Error(`This Steam client has no SteamClient.Apps.${name}; close Steam and edit again`);
		}
	}
	return client as Apps;
}

/**
 * Wait for the app's details to satisfy `done`, for at most DETAILS_TIMEOUT_MS.
 * Resolves with the last details Steam sent (null when it sent none) and
 * whether they satisfied `done`.
 */
function waitForDetails(
	appid: number,
	done: (details: AppDetails) => boolean,
): Promise<{ details: AppDetails | null; matched: boolean }> {
	return new Promise((resolve, reject) => {
		let last: AppDetails | null = null;
		let settled = false;
		let registration: { unregister(): void } | undefined;
		const finish = (matched: boolean) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			registration?.unregister();
			resolve({ details: last, matched });
		};
		const timer = setTimeout(() => finish(false), DETAILS_TIMEOUT_MS);
		try {
			registration = apps().RegisterForAppDetails(appid, (details) => {
				// Steam answers an appid it doesn't know with an empty object.
				last = details && Object.keys(details).length > 0 ? details : null;
				if (last && done(last)) finish(true);
			});
		} catch (error) {
			settled = true;
			clearTimeout(timer);
			reject(error);
			return;
		}
		// The callback may have fired during registration, before it could be unregistered.
		if (settled) registration?.unregister();
	});
}

/** The app's details, refusing apps Steam doesn't know and non-Steam shortcuts. */
async function steamApp(appid: number): Promise<AppDetails> {
	const { details } = await waitForDetails(appid, () => true);
	if (!details) throw new Error(`Steam knows no app with appid ${appid} in this library`);
	if (details.strShortcutExe) {
		throw new Error(`appid ${appid} is a non-Steam shortcut; its launch options are edited with the shortcuts tool`);
	}
	return details;
}

async function getLaunchOptions(appid: number): Promise<LaunchOptionsResult> {
	const details = await steamApp(appid);
	return { appid, launchOptions: details.strLaunchOptions ?? '', changed: false };
}

/** The write in progress; the next one waits for it. */
let writes: Promise<unknown> = Promise.resolve();

/**
 * Writes run one at a time, so two requests can't both pass the same
 * `expected` check. An edit made in Steam's own dialog at the same moment can
 * still slip in between the check and the write.
 */
function setLaunchOptions(appid: number, options: string, expected?: string): Promise<LaunchOptionsResult> {
	const write = writes.then(() => writeLaunchOptions(appid, options, expected));
	writes = write.catch(() => undefined);
	return write;
}

async function writeLaunchOptions(appid: number, options: string, expected?: string): Promise<LaunchOptionsResult> {
	const current = (await steamApp(appid)).strLaunchOptions ?? '';
	if (expected !== undefined && current !== expected) {
		throw new Error(
			`The launch options of appid ${appid} changed in Steam since steam-mcp read them; they were NOT modified, try again`,
		);
	}
	if (current === options) return { appid, launchOptions: options, changed: false };

	// Register before writing, so the confirming update can't be missed.
	const confirmed = waitForDetails(appid, (details) => (details.strLaunchOptions ?? '') === options);
	apps().SetAppLaunchOptions(appid, options);
	const { details, matched } = await confirmed;
	if (!matched) {
		const seen = details ? ` (Steam last reported ${JSON.stringify(details.strLaunchOptions ?? '')})` : '';
		throw new Error(
			`Steam did not confirm the new launch options of appid ${appid} within ${DETAILS_TIMEOUT_MS / 1000}s${seen}; check them before retrying`,
		);
	}
	return { appid, launchOptions: options, changed: true };
}

function requireAppid(value: unknown): number {
	if (!Number.isSafeInteger(value) || (value as number) <= 0 || (value as number) > 0xffffffff) {
		throw new Error('"appid" must be a positive whole number of at most 32 bits');
	}
	return value as number;
}

function requireOptions(value: unknown, field: string): string {
	if (typeof value !== 'string' || value.length > MAX_OPTIONS_LENGTH) {
		throw new Error(`"${field}" must be a string of at most ${MAX_OPTIONS_LENGTH} characters`);
	}
	return value;
}

/**
 * Check a launch options request. Only its shape: which launch options are
 * acceptable is steam-mcp's check, and a program that can reach Millennium's
 * socket could write localconfig.vdf just as well.
 */
export function parse(fields: Record<string, unknown>): Request {
	const appid = requireAppid(fields.appid);
	if (fields.action === 'get_launch_options') return { action: 'get_launch_options', appid };
	if (fields.action !== 'set_launch_options') throw new Error(`Unknown action ${JSON.stringify(fields.action)}`);
	const request: Request = { action: 'set_launch_options', appid, options: requireOptions(fields.options, 'options') };
	if (fields.expected !== undefined) request.expected = requireOptions(fields.expected, 'expected');
	return request;
}

export function handle(request: Request): Promise<LaunchOptionsResult> {
	return request.action === 'get_launch_options'
		? getLaunchOptions(request.appid)
		: setLaunchOptions(request.appid, request.options, request.expected);
}
