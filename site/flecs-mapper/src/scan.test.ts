import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { run } from './run.ts';
import { CODE, scan } from './scan.ts';

const wasm = path.resolve(import.meta.dirname, '../../flecs/flecs-doc.wasm');

for (const [rel, tgt, relName, tgtName] of [
	['Requires', 'site.target', 'Requires', 'target'],
	['Requires', '"site.target-name"', 'Requires', 'target-name'],
	['"rel-name"', 'site.target', 'rel-name', 'target'],
	['"rel-name"', '"site.target-name"', 'rel-name', 'target-name'],
]) {
	test(`pair (${rel}, ${tgt}) agrees with Flecs and retains its source spans`, async () => {
		const source = `${JSON.stringify(relName)} {}\nsite {\n ${JSON.stringify(tgtName)} {}\n page {\n  (${rel}, ${tgt})\n }\n}\n`;
		const found = scan(source);
		assert.deepEqual(found.diags, []);
		const pairs = found.items.filter((item) => item.k === 'pair');
		assert.equal(pairs.length, 1);
		assert.equal(pairs[0].owner, 'site.page');
		assert.equal(pairs[0].rel.v, relName);
		assert.equal(pairs[0].tgt.v, `site.${tgtName}`);
		assert.equal(source.slice(pairs[0].rel.s, pairs[0].rel.e), rel);
		assert.equal(source.slice(pairs[0].tgt.s, pairs[0].tgt.e), tgt);
		const result = await run(wasm, [source], ['site']);
		assert.equal(result.ok, true, JSON.stringify(result));
		if (result.ok) {
			assert.equal(result.entities.find((entity) => entity.parent === 'site' && entity.name === 'page')?.pairs?.[relName], `site.${tgtName}`);
		}
	});
}

test('quoted names do not admit malformed pairs', () => {
	for (const pair of [
		'(Requires "site.target")',
		'(Requires, "site.target", Other)',
		'(Requires,)',
		'(42, "site.target")',
		'(Requires, 42)',
	]) {
		assert.ok(scan(`page {\n ${pair}\n}\n`).diags.some((diag) => diag.code === CODE.syntax), pair);
	}
});

test('a quoted target still has to exist in Flecs', async () => {
	const source = 'Requires {}\nsite {\n page {\n  (Requires, "site.missing-target")\n }\n}\n';
	assert.deepEqual(scan(source).diags, []);
	const result = await run(wasm, [source], ['site']);
	assert.equal(result.ok, false);
	if (!result.ok) assert.ok(result.errors.some((error) => error.message.includes("unresolved reference 'site.missing-target'")));
});

test('the const-macro-trees page retains its quoted prerequisite', () => {
	const source = readFileSync(path.resolve(import.meta.dirname, '../../content/updates/const-macro-trees.flecs'), 'utf8');
	const found = scan(source);
	assert.deepEqual(found.diags, []);
	const prerequisite = found.items.find((item) => item.k === 'pair' && item.rel.v === 'Requires');
	assert.equal(prerequisite?.k, 'pair');
	if (prerequisite?.k === 'pair') {
		assert.equal(prerequisite.owner, 'site.updates.const-macro-trees');
		assert.equal(prerequisite.tgt.v, 'site.updates.const-macros');
		assert.equal(source.slice(prerequisite.tgt.s, prerequisite.tgt.e), '"site.updates.const-macros"');
	}
});
