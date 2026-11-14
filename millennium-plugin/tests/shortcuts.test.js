// Run with `bun test` in millennium-plugin/. Stands in for the Steam client's
// SteamClient.Apps and appStore: details are sent once on registration and
// again on every change, and AddShortcut ignores everything but the exe, as
// Steam's sometimes does.
import { beforeEach, expect, test } from 'bun:test';

globalThis.window = {};
const { handle } = await import('../frontend/shortcuts.ts');
const { parseRequest } = await import('../frontend/requests.ts');

let apps;
let known;
let calls;

function notify(listeners, appid) {
	for (const callback of [...(listeners.get(appid) ?? [])]) callback(known.get(appid) ?? {});
}

beforeEach(() => {
	const listeners = new Map();
	known = new Map([
		[440, { strDisplayName: 'Team Fortress 2', strLaunchOptions: '' }],
		[3000000001, { strDisplayName: 'Old', strShortcutExe: '"/opt/old/old"', strShortcutStartDir: '"/opt/old"', strShortcutLaunchOptions: '' }],
	]);
	calls = [];
	const set = (field) => (appid, value) => {
		calls.push([field, appid, value]);
		known.set(appid, { ...known.get(appid), [field]: value });
		notify(listeners, appid);
	};
	apps = {
		RegisterForAppDetails(appid, callback) {
			const list = listeners.get(appid) ?? [];
			list.push(callback);
			listeners.set(appid, list);
			callback(known.get(appid) ?? {});
			return { unregister: () => list.splice(list.indexOf(callback), 1) };
		},
		async AddShortcut(name, exe) {
			calls.push(['AddShortcut', name, exe]);
			known.set(3000000002, { strDisplayName: 'old-name', strShortcutExe: exe, strShortcutStartDir: '', strShortcutLaunchOptions: '' });
			return 3000000002;
		},
		RemoveShortcut(appid) {
			calls.push(['RemoveShortcut', appid]);
			known.delete(appid);
			notify(listeners, appid);
		},
		SetShortcutName: set('strDisplayName'),
		SetShortcutExe: set('strShortcutExe'),
		SetShortcutStartDir: set('strShortcutStartDir'),
		SetShortcutLaunchOptions: set('strShortcutLaunchOptions'),
	};
	window.SteamClient = { Apps: apps };
	window.appStore = { GetAppOverviewByAppID: (appid) => (known.has(appid) ? { appid } : null) };
});

const game = { name: 'My Game', exe: '"/opt/game/run"', startDir: '"/opt/game"', launchOptions: '-novid' };

test('adds a shortcut and sets every field, whatever AddShortcut kept', async () => {
	const added = await handle({ action: 'add_shortcut', ...game });
	expect(added).toEqual({ appid: 3000000002, ...game });
	expect(calls.map((c) => c[0])).toEqual([
		'AddShortcut',
		'strDisplayName',
		'strShortcutExe',
		'strShortcutStartDir',
		'strShortcutLaunchOptions',
	]);
});

test('reports a shortcut Steam added but did not confirm', async () => {
	apps.SetShortcutLaunchOptions = () => {};
	await expect(handle({ action: 'add_shortcut', ...game })).rejects.toThrow(
		'Steam added shortcut appid 3000000002 but did not confirm its settings',
	);
});

test('accepts a Steam that stores the program and folder without quotes', async () => {
	const unquoting = (field) => (appid, value) => {
		known.set(appid, { ...known.get(appid), [field]: value.replace(/^"|"$/g, '') });
	};
	apps.SetShortcutExe = unquoting('strShortcutExe');
	apps.SetShortcutStartDir = unquoting('strShortcutStartDir');
	// The last setter's notification carries the unquoted fields.
	const added = await handle({ action: 'add_shortcut', ...game });
	expect(added.exe).toBe('/opt/game/run');
});

test('a Steam call that never settles does not hold up later writes', async () => {
	apps.AddShortcut = () => new Promise(() => {});
	const stuck = handle({ action: 'add_shortcut', ...game });
	const next = handle({ action: 'remove_shortcut', appid: 3000000001 });
	await expect(stuck).rejects.toThrow('did not return an appid for the new shortcut within 2s; it may still have been added');
	expect((await next).appid).toBe(3000000001);
});

test('a setter that throws after the add names the shortcut Steam already made', async () => {
	apps.SetShortcutStartDir = () => {
		throw new Error('boom');
	};
	await expect(handle({ action: 'add_shortcut', ...game })).rejects.toThrow(
		'Steam added shortcut appid 3000000002, but setting it up failed (boom); check it in the library instead of adding it again',
	);
});

test('reports a Steam that returns no appid', async () => {
	apps.AddShortcut = async () => undefined;
	await expect(handle({ action: 'add_shortcut', ...game })).rejects.toThrow('did not return an appid');
});

test('removes a shortcut and answers with what it was', async () => {
	const removed = await handle({ action: 'remove_shortcut', appid: 3000000001 });
	expect(removed).toEqual({ appid: 3000000001, name: 'Old', exe: '"/opt/old/old"', startDir: '"/opt/old"', launchOptions: '' });
	expect(calls).toEqual([['RemoveShortcut', 3000000001]]);
});

test('refuses to remove a Steam game or an unknown appid', async () => {
	await expect(handle({ action: 'remove_shortcut', appid: 440 })).rejects.toThrow('no non-Steam shortcut');
	await expect(handle({ action: 'remove_shortcut', appid: 12345 })).rejects.toThrow('no non-Steam shortcut');
	expect(calls).toEqual([]);
});

test('reports a removal Steam does not confirm', async () => {
	apps.RemoveShortcut = () => {};
	await expect(handle({ action: 'remove_shortcut', appid: 3000000001 })).rejects.toThrow('did not confirm removing');
});

test('refuses to remove without the appStore to confirm it', async () => {
	delete window.appStore;
	await expect(handle({ action: 'remove_shortcut', appid: 3000000001 })).rejects.toThrow('appStore');
	expect(calls).toEqual([]);
});

test('parses shortcut requests and refuses malformed ones', () => {
	expect(parseRequest(JSON.stringify({ action: 'add_shortcut', ...game }))).toEqual({ action: 'add_shortcut', ...game });
	expect(parseRequest('{"action":"remove_shortcut","appid":3000000001}')).toEqual({
		action: 'remove_shortcut',
		appid: 3000000001,
	});
	for (const fields of [
		{ action: 'add_shortcut', ...game, name: '  ' },
		{ action: 'add_shortcut', ...game, exe: '' },
		{ action: 'add_shortcut', ...game, startDir: 7 },
		{ action: 'add_shortcut', name: 'x', exe: 'y' },
		{ action: 'add_shortcut', ...game, launchOptions: 'x'.repeat(4097) },
		{ action: 'remove_shortcut' },
		{ action: 'remove_shortcut', appid: 2 ** 32 },
	]) {
		expect(() => parseRequest(JSON.stringify(fields))).toThrow();
	}
});
