import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { parseString } from '../src/manifest.ts';

const CANONICAL = `
[plugin]
id               = "alpha"
version          = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./bin/alpha"
mode   = "process"
eager  = false

[surfaces]
mcp = true

[[capabilities]]
name        = "context.publish"
sensitivity = "general"

[[capabilities]]
name        = "atlassian.read"
sensitivity = "sensitive"
`;

describe('manifest', () => {
  it('parses canonical fixture', () => {
    const m = parseString(CANONICAL);
    assert.equal(m.plugin.id, 'alpha');
    assert.equal(m.runtime.binary, './bin/alpha');
    assert.equal(m.runtime.mode, 'process');
    assert.equal(m.surfaces.mcp, true);
    assert.equal(m.capabilities.length, 2);
    assert.equal(m.capabilities[1]!.sensitivity, 'sensitive');
  });

  it('rejects unknown top-level field', () => {
    assert.throws(() => parseString(CANONICAL + '\n[bogus]\nx = 1\n'), /bogus/);
  });

  it('rejects both binary and image', () => {
    assert.throws(
      () =>
        parseString(`
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
image  = "ghcr.io/x:1"
`),
      /mutually exclusive/,
    );
  });

  it('rejects neither binary nor image', () => {
    assert.throws(
      () =>
        parseString(`
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
`),
      /binary/,
    );
  });

  it('rejects duplicate capability', () => {
    assert.throws(
      () =>
        parseString(`
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"

[[capabilities]]
name = "thing"
sensitivity = "general"

[[capabilities]]
name = "thing"
sensitivity = "sensitive"
`),
      /duplicate/,
    );
  });

  it('rejects pre-release semver', () => {
    assert.throws(
      () =>
        parseString(`
[plugin]
id = "x"
version = "0.1.0-rc1"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
`),
      /pre-release/,
    );
  });

  it('rejects bad plugin id', () => {
    for (const bad of ['', 'has space', 'has/slash']) {
      assert.throws(
        () =>
          parseString(`
[plugin]
id = "${bad}"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
`),
        /plugin\.id/,
      );
    }
  });
});
