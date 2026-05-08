import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { PassThrough } from 'node:stream';
import { FrameReader, writeFrame } from '../src/framing.ts';

describe('framing', () => {
  it('round-trips multiple frames', async () => {
    const stream = new PassThrough();
    const reader = new FrameReader(stream);

    await writeFrame(stream, Buffer.from('hello'));
    await writeFrame(stream, Buffer.from('world'));
    await writeFrame(stream, Buffer.from(''));
    stream.end();

    const a = await reader.read();
    const b = await reader.read();
    const c = await reader.read();
    const d = await reader.read();
    assert.equal(a?.toString(), 'hello');
    assert.equal(b?.toString(), 'world');
    assert.equal(c?.length, 0);
    assert.equal(d, null);
  });

  it('handles fragmented input', async () => {
    const stream = new PassThrough();
    const reader = new FrameReader(stream);
    const body = Buffer.from('split');
    const header = Buffer.alloc(4);
    header.writeUInt32BE(body.length, 0);

    stream.write(header.subarray(0, 2));
    setTimeout(() => stream.write(header.subarray(2)), 5);
    setTimeout(() => stream.write(body.subarray(0, 2)), 10);
    setTimeout(() => stream.write(body.subarray(2)), 15);
    setTimeout(() => stream.end(), 20);

    const got = await reader.read();
    assert.equal(got?.toString(), 'split');
  });
});
