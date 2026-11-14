import * as collections from './collections';
import * as launchOptions from './launch-options';

export type Request = collections.Request | launchOptions.Request;

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
	return launchOptions.ACTIONS.includes(fields.action as string)
		? launchOptions.parse(fields)
		: collections.parse(fields);
}

export function handle(request: Request): Promise<unknown> {
	switch (request.action) {
		case 'get_launch_options':
		case 'set_launch_options':
			return launchOptions.handle(request);
		default:
			return collections.handle(request);
	}
}
