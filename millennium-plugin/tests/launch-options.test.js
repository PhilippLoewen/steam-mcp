// Run with `bun test` in millennium-plugin/. Stands in for the Steam client's
// SteamClient.Apps, which answers RegisterForAppDetails like Steam does: once
// right away, then on every change.
import { beforeEach, expect, test } from 'bun:test';

globalThis.window = {};
const { handle } = await import('../frontend/launch-options.ts');

let apps;
let sets;

function fakeSteam(known) {
	const listeners = new Map();
	sets = [];
	apps = {
		RegisterForAppDetails(appid, callback) {
			const list = listeners.get(appid) ?? [];
			list.push(callback);
			listeners.set(appid, list);
			callback(known.get(appid) ?? {});
			return { unregister: () => list.splice(list.indexOf(callback), 1) };
		},
		SetAppLaunchOptions(appid, options) {
			sets.push([appid, options]);
			const details = { ...known.get(appid), strLaunchOptions: options };
			known.set(appid, details);
			for (const callback of [...(listeners.get(appid) ?? [])]) callback(details);
		},
	};
	window.SteamClient = { Apps: apps };
}

beforeEach(() => fakeSteam(new Map([[440, { strLaunchOptions: '-high' }], [999, { strShortcutExe: '/bin/x' }]])));

test('reads the launch options Steam holds', async () => {
	expect(await handle({ action: 'get_launch_options', appid: 440 })).toEqual({
		appid: 440,
		launchOptions: '-high',
		changed: false,
	});
});

test('sets launch options and waits for Steam to confirm them', async () => {
	const result = await handle({ action: 'set_launch_options', appid: 440, options: '-novid', expected: '-high' });
	expect(result).toEqual({ appid: 440, launchOptions: '-novid', changed: true });
	expect(sets).toEqual([[440, '-novid']]);
});

test('leaves equal launch options alone', async () => {
	const result = await handle({ action: 'set_launch_options', appid: 440, options: '-high' });
	expect(result.changed).toBe(false);
	expect(sets).toEqual([]);
});

test('refuses when Steam no longer holds the expected launch options', async () => {
	const edit = handle({ action: 'set_launch_options', appid: 440, options: '-novid', expected: '' });
	await expect(edit).rejects.toThrow('changed in Steam');
	expect(sets).toEqual([]);
});

test('runs overlapping writes one at a time, so only one passes the same expected check', async () => {
	const results = await Promise.allSettled([
		handle({ action: 'set_launch_options', appid: 440, options: '-a', expected: '-high' }),
		handle({ action: 'set_launch_options', appid: 440, options: '-b', expected: '-high' }),
	]);
	expect(results.map((r) => r.status)).toEqual(['fulfilled', 'rejected']);
	expect(results[1].reason.message).toContain('changed in Steam');
	expect(sets).toEqual([[440, '-a']]);
});

test('a failed write does not block the next one', async () => {
	await expect(handle({ action: 'set_launch_options', appid: 999, options: '-x' })).rejects.toThrow();
	expect((await handle({ action: 'set_launch_options', appid: 440, options: '-c' })).changed).toBe(true);
});

test('refuses non-Steam shortcuts', async () => {
	await expect(handle({ action: 'set_launch_options', appid: 999, options: '-x' })).rejects.toThrow('shortcut');
	expect(sets).toEqual([]);
});

test('refuses apps Steam does not know', async () => {
	await expect(handle({ action: 'get_launch_options', appid: 12345 })).rejects.toThrow('knows no app');
});

test('reports a Steam that does not confirm the write', async () => {
	apps.SetAppLaunchOptions = (appid, options) => sets.push([appid, options]);
	const edit = handle({ action: 'set_launch_options', appid: 440, options: '-novid' });
	await expect(edit).rejects.toThrow('did not confirm the new launch options of appid 440 within 2s (Steam last reported "-high")');
});

test('reports a Steam client without the calls', async () => {
	delete apps.SetAppLaunchOptions;
	await expect(handle({ action: 'get_launch_options', appid: 440 })).rejects.toThrow('SetAppLaunchOptions');
});
