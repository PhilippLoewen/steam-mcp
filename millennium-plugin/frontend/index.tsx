import { definePlugin, Field, pluginConfig } from 'millennium';
import { handle, parseRequest } from './requests';

async function respond(id: string, request: string): Promise<void> {
	let response;
	try {
		response = { ok: true, result: await handle(parseRequest(request)) };
	} catch (error) {
		response = { ok: false, error: error instanceof Error ? error.message : String(error) };
	}
	// No plugin name: starlight injects it as the first argument of every
	// pluginConfig call, so passing it here would shift key and value. The
	// typings narrow the value of this two-argument form to a string.
	await pluginConfig.set(`response:${id}`, response as unknown as string);
}

// Called by the backend for each request. Answers asynchronously through the
// plugin config, since collection saves and app details are async.
/** @ffi */
export function handleRequest(id: string, request: string): boolean {
	void respond(id, request);
	return true;
}

export default definePlugin(() => ({
	title: 'Steam MCP Bridge',
	icon: <span />,
	content: <Field label="Lets steam-mcp edit library categories, launch options and shortcuts while Steam is running." />,
}));
