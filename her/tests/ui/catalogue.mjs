import assert from 'node:assert/strict';

export function validateCatalogue({ inventory, functions }, walks, unavailablePages) {
  const sources = new Map();
  for (const [id, definition] of Object.entries(functions)) {
    assert(definition.area && definition.section && definition.label, `Missing placement for ${id}`);
    for (const source of definition.sources) {
      assert(!sources.has(source), `Duplicate catalogue source: ${source}`);
      assert(Object.hasOwn(inventory, source.split('.')[0]), `Unknown catalogue source: ${source}`);
      sources.set(source, id);
    }
  }
  for (const [page, expected] of Object.entries(inventory)) {
    const declared = [...sources.keys()].filter(source => source.startsWith(page + '.'));
    assert.equal(declared.length, expected, `Inventory count for ${page}`);
    if (Object.hasOwn(unavailablePages, page)) {
      assert(!Object.hasOwn(walks, page), `Walk defined for unavailable page: ${page}`);
    } else {
      assert.deepEqual(Object.keys(walks[page] ?? {}).map(key => `${page}.${key}`).sort(),
        declared.sort(), `Walk coverage for ${page}`);
    }
  }
  for (const page of Object.keys(walks)) {
    assert(Object.hasOwn(inventory, page), `Unknown walk page: ${page}`);
  }
}
