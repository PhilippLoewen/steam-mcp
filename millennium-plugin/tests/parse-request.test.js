// Run with `bun test` in millennium-plugin/. collections.ts reads `window`
// when it loads, so stand one in before importing it.
import { expect, test } from 'bun:test';

globalThis.window = {};
const { parseRequest } = await import('../frontend/collections.ts');

test('accepts what steam-mcp sends', () => {
	expect(parseRequest('{"action":"add","name":"RPG","appids":[440,730]}')).toEqual({
		action: 'add',
		name: 'RPG',
		appids: [440, 730],
	});
	expect(parseRequest('{"action":"remove","name":"RPG","appids":[440]}').action).toBe('remove');
	expect(parseRequest('{"action":"create","name":"RPG"}')).toEqual({ action: 'create', name: 'RPG' });
	expect(parseRequest('{"action":"delete","name":"RPG"}')).toEqual({ action: 'delete', name: 'RPG' });
	expect(parseRequest('{"action":"rename","name":"RPG","newName":"Role-playing"}')).toEqual({
		action: 'rename',
		name: 'RPG',
		newName: 'Role-playing',
	});
});

test('refuses anything else', () => {
	for (const raw of [
		'not json',
		'[]',
		'null',
		'"add"',
		'{"action":"wipe","name":"RPG"}',
		'{"name":"RPG"}',
		'{"action":"create"}',
		'{"action":"create","name":"   "}',
		'{"action":"create","name":42}',
		`{"action":"create","name":"${'x'.repeat(257)}"}`,
		'{"action":"add","name":"RPG"}',
		'{"action":"add","name":"RPG","appids":"abc"}',
		'{"action":"add","name":"RPG","appids":[]}',
		'{"action":"add","name":"RPG","appids":[440,"730"]}',
		'{"action":"add","name":"RPG","appids":[1.5]}',
		'{"action":"add","name":"RPG","appids":[-1]}',
		`{"action":"add","name":"RPG","appids":[${Array(10001).fill(1).join(',')}]}`,
		'{"action":"rename","name":"RPG"}',
	]) {
		expect(() => parseRequest(raw)).toThrow();
	}
});
