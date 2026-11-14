import * as collections from './collections';
import * as launchOptions from './launch-options';
import * as shortcuts from './shortcuts';

export type Request = collections.Request | launchOptions.Request | shortcuts.Request;

/**
 * Parse and check a request from the config key. Any local program that can
 * reach Millennium's socket can write one, so nothing is trusted: it must be a
 * JSON object with a known action, and each module checks its own fields.
 */
export function parseRequest(raw: string): Request {
	let data: unknown;
	try {
		data = JSON.parse(raw);
	} catch {
		throw new Error('The request is not valid JSON');
	}
	if (typeof data !== 'object' || data === null || Array.isArray(data)) {
		throw new Error('The request must be a JSON object');
	}
	const fields = data as Record<string, unknown>;
	const action = fields.action as string;
	if (launchOptions.ACTIONS.includes(action)) return launchOptions.parse(fields);
	if (shortcuts.ACTIONS.includes(action)) return shortcuts.parse(fields);
	return collections.parse(fields);
}

export function handle(request: Request): Promise<unknown> {
	switch (request.action) {
		case 'get_launch_options':
		case 'set_launch_options':
			return launchOptions.handle(request);
		case 'add_shortcut':
		case 'remove_shortcut':
			return shortcuts.handle(request);
		default:
			return collections.handle(request);
	}
}
