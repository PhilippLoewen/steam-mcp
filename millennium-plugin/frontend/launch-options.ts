/**
 * Launch options reads and writes through the Steam client's own SteamClient.Apps
 * calls. Steam keeps launch options in memory and rewrites localconfig.vdf from
 * it, so a write made here sticks while Steam runs.
 */

import {
	type AppDetails,
	DETAILS_TIMEOUT_MS,
	requireAppid,
	requireString,
	serialize,
	steamApps,
	waitForDetails,
} from './steam-apps';

export type Request =
	| { action: 'get_launch_options'; appid: number }
	| { action: 'set_launch_options'; appid: number; options: string; expected?: string };

export interface LaunchOptionsResult {
	appid: number;
	launchOptions: string;
	changed: boolean;
}

export const ACTIONS: readonly string[] = ['get_launch_options', 'set_launch_options'];

const apps = () => steamApps('RegisterForAppDetails', 'SetAppLaunchOptions');

/** The app's details, refusing apps Steam doesn't know and non-Steam shortcuts. */
async function steamApp(appid: number): Promise<AppDetails> {
	apps();
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

/** One write at a time (see serialize), so two requests can't both pass the same `expected` check. */
async function setLaunchOptions(appid: number, options: string, expected?: string): Promise<LaunchOptionsResult> {
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

/**
 * Check a launch options request. Only its shape: which launch options are
 * acceptable is steam-mcp's check, and a program that can reach Millennium's
 * socket could write localconfig.vdf just as well.
 */
export function parse(fields: Record<string, unknown>): Request {
	const appid = requireAppid(fields.appid);
	if (fields.action === 'get_launch_options') return { action: 'get_launch_options', appid };
	if (fields.action !== 'set_launch_options') throw new Error(`Unknown action ${JSON.stringify(fields.action)}`);
	const request: Request = { action: 'set_launch_options', appid, options: requireString(fields.options, 'options') };
	if (fields.expected !== undefined) request.expected = requireString(fields.expected, 'expected');
	return request;
}

export function handle(request: Request): Promise<LaunchOptionsResult> {
	return request.action === 'get_launch_options'
		? getLaunchOptions(request.appid)
		: serialize(() => setLaunchOptions(request.appid, request.options, request.expected));
}
