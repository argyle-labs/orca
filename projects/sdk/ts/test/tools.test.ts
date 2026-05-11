import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import {
  toolErrorCodes,
  ToolHandlerError,
  type ToolDeclaration,
  type ToolHandler,
  TOOLS_CALL_METHOD,
  TOOLS_DECLARE_METHOD,
} from '../src/tools.ts';

test('declaration roundtrips through JSON', () => {
  const d: ToolDeclaration = {
    name: 'stack.list',
    description: 'List stacks',
    input_schema: { type: 'object' },
    sensitivity: 'general',
  };
  const back = JSON.parse(JSON.stringify(d)) as ToolDeclaration;
  assert.equal(back.name, 'stack.list');
  assert.equal(back.sensitivity, 'general');
  assert.deepEqual(back.input_schema, { type: 'object' });
});

test('ToolHandlerError carries data', () => {
  const e = new ToolHandlerError('upstream rejected', { status: 500 });
  assert.equal(e.message, 'upstream rejected');
  assert.deepEqual(e.data, { status: 500 });
  assert.equal(e.name, 'ToolHandlerError');
  assert.ok(e instanceof Error);
});

test('handler signature accepts an async closure', async () => {
  const h: ToolHandler = async (args: unknown) => {
    const obj = args as { value: string };
    return { echoed: obj.value };
  };
  const out = (await h({ value: 'ping' })) as { echoed: string };
  assert.equal(out.echoed, 'ping');
});

test('method names and error codes match the locked contract', () => {
  assert.equal(TOOLS_DECLARE_METHOD, 'orca/tools.declare');
  assert.equal(TOOLS_CALL_METHOD, 'orca/tools.call');
  assert.equal(toolErrorCodes.UNKNOWN_TOOL, -32001);
  assert.equal(toolErrorCodes.SCHEMA_VIOLATION, -32002);
  assert.equal(toolErrorCodes.HANDLER_ERROR, -32003);
});
