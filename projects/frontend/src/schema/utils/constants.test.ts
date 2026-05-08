import { describe, it, expect } from 'vitest';
import {
  TABLE_WIDTH, ROW_HEIGHT, HEADER_HEIGHT, MAX_VISIBLE_COLS, TABLE_GAP,
  DOMAIN_PADDING, DOMAIN_GAP, DOMAIN_HEADER_HEIGHT,
  GROUP_PADDING, GROUP_HEADER_HEIGHT, GROUP_SUB_GAP,
} from './constants';

describe('layout constants', () => {
  it('TABLE_WIDTH is 280', () => expect(TABLE_WIDTH).toBe(280));
  it('ROW_HEIGHT is 22', () => expect(ROW_HEIGHT).toBe(22));
  it('HEADER_HEIGHT is 36', () => expect(HEADER_HEIGHT).toBe(36));
  it('MAX_VISIBLE_COLS is 25', () => expect(MAX_VISIBLE_COLS).toBe(25));
  it('TABLE_GAP is 60', () => expect(TABLE_GAP).toBe(60));
  it('DOMAIN_PADDING is 80', () => expect(DOMAIN_PADDING).toBe(80));
  it('DOMAIN_GAP is 180', () => expect(DOMAIN_GAP).toBe(180));
  it('DOMAIN_HEADER_HEIGHT is 40', () => expect(DOMAIN_HEADER_HEIGHT).toBe(40));
  it('GROUP_PADDING is 50', () => expect(GROUP_PADDING).toBe(50));
  it('GROUP_HEADER_HEIGHT is 44', () => expect(GROUP_HEADER_HEIGHT).toBe(44));
  it('GROUP_SUB_GAP is 60', () => expect(GROUP_SUB_GAP).toBe(60));
  it('all values are positive numbers', () => {
    const all = [TABLE_WIDTH, ROW_HEIGHT, HEADER_HEIGHT, MAX_VISIBLE_COLS, TABLE_GAP,
      DOMAIN_PADDING, DOMAIN_GAP, DOMAIN_HEADER_HEIGHT, GROUP_PADDING, GROUP_HEADER_HEIGHT, GROUP_SUB_GAP];
    all.forEach(v => expect(v).toBeGreaterThan(0));
  });
});
