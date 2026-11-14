/**
 * What the launch options and shortcut edits share: the Steam client's
 * SteamClient.Apps calls, waiting for an app's details, one write at a time,
 * and the field checks for requests.
 */

export interface AppDetails {
	strDisplayName?: string;
	strLaunchOptions?: string;
	strShortcutExe?: string;
	strShortcutStartDir?: string;
	strShortcutLaunchOptions?: string;
}

export interface Apps {
	RegisterForAppDetails(appid: number, callback: (details: AppDetails) => void): { unregister(): void };
	SetAppLaunchOptions(appid: number, options: string): void;
	AddShortcut(name: string, exe: string, startDir: string, launchOptions: string): Promise<number>;
	RemoveShortcut(appid: number): void;
	SetShortcutName(appid: number, name: string): void;
	SetShortcutExe(appid: number, exe: string): void;
	SetShortcutStartDir(appid: number, startDir: string): void;
	SetShortcutLaunchOptions(appid: number, options: string): void;
}

/** How long Steam gets to send an app's details, and to confirm a write. */
export const DETAILS_TIMEOUT_MS = 2000;

/** SteamClient.Apps, refusing a client that lacks any of `names`. */
export function steamApps<K extends keyof Apps>(...names: K[]): Pick<Apps, K> {
	const client = (window as unknown as { SteamClient?: { Apps?: Partial<Apps> } }).SteamClient?.Apps;
	for (const name of names) {
		if (typeof client?.[name] !== 'function') {
			throw new Error(`This Steam client has no SteamClient.Apps.${name}; close Steam and edit again`);
		}
	}
	return client as Pick<Apps, K>;
}

/**
 * Wait for the app's details to satisfy `done`, for at most DETAILS_TIMEOUT_MS.
 * Resolves with the last details Steam sent (null when it sent none) and
 * whether they satisfied `done`.
 */
export function waitForDetails(
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
			registration = steamApps('RegisterForAppDetails').RegisterForAppDetails(appid, (details) => {
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

/**
 * `promise`, or a rejection with `message` after DETAILS_TIMEOUT_MS: a Steam
 * call that never settles must not hold up the writes queued behind it.
 */
export function withTimeout<T>(promise: Promise<T>, message: string): Promise<T> {
	let timer: ReturnType<typeof setTimeout> | undefined;
	const timeout = new Promise<never>((_, reject) => {
		timer = setTimeout(() => reject(new Error(message)), DETAILS_TIMEOUT_MS);
	});
	return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** The write in progress; the next one waits for it. */
let writes: Promise<unknown> = Promise.resolve();

/**
 * Run writes one at a time, so a check a write makes before changing Steam
 * still holds when it changes it. Edits made in Steam's own dialogs at the
 * same moment can still slip in between.
 */
export function serialize<T>(write: () => Promise<T>): Promise<T> {
	const run = writes.then(write);
	writes = run.catch(() => undefined);
	return run;
}

export function requireAppid(value: unknown): number {
	if (!Number.isSafeInteger(value) || (value as number) <= 0 || (value as number) > 0xffffffff) {
		throw new Error('"appid" must be a positive whole number of at most 32 bits');
	}
	return value as number;
}

/** Longest string field a request may carry. */
export const MAX_FIELD_LENGTH = 4096;

export function requireString(value: unknown, field: string, nonEmpty = false): string {
	if (typeof value !== 'string' || value.length > MAX_FIELD_LENGTH || (nonEmpty && !value.trim())) {
		const what = nonEmpty ? 'a non-empty string' : 'a string';
		throw new Error(`"${field}" must be ${what} of at most ${MAX_FIELD_LENGTH} characters`);
	}
	return value;
}
