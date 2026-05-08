import { test, expect } from '@playwright/test';

test.describe('Theme switching', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await page.waitForLoadState('networkidle');
  });

  test('document has data-theme and data-mode attributes on load', async ({ page }) => {
    const theme = await page.evaluate(() => document.documentElement.dataset.theme);
    const mode  = await page.evaluate(() => document.documentElement.dataset.mode);
    expect(theme).toBeTruthy();
    expect(['violet', 'ocean', 'tide', 'forest', 'copper', 'ice-age']).toContain(theme);
    expect(['dark', 'light']).toContain(mode);
  });

  test('semantic color tokens are defined on :root', async ({ page }) => {
    const tokens = await page.evaluate(() => {
      const style = getComputedStyle(document.documentElement);
      return {
        danger:  style.getPropertyValue('--color-danger').trim(),
        warning: style.getPropertyValue('--color-warning').trim(),
        success: style.getPropertyValue('--color-success').trim(),
        info:    style.getPropertyValue('--color-info').trim(),
        special: style.getPropertyValue('--color-special').trim(),
      };
    });
    for (const [name, value] of Object.entries(tokens)) {
      expect(value, `--color-${name} should be defined`).not.toBe('');
    }
  });

  test('mode persists to localStorage', async ({ page }) => {
    const initialMode = await page.evaluate(() => document.documentElement.dataset.mode);
    const newMode = initialMode === 'dark' ? 'light' : 'dark';

    // Directly set mode via localStorage and reload to verify persistence
    await page.evaluate((mode) => localStorage.setItem('orca-mode', mode), newMode);
    await page.reload();
    await page.waitForLoadState('networkidle');

    const modeAfterReload = await page.evaluate(() => document.documentElement.dataset.mode);
    expect(modeAfterReload).toBe(newMode);
  });

  test('theme persists to localStorage', async ({ page }) => {
    await page.evaluate(() => localStorage.setItem('orca-theme', 'ocean'));
    await page.reload();
    await page.waitForLoadState('networkidle');

    const theme = await page.evaluate(() => document.documentElement.dataset.theme);
    expect(theme).toBe('ocean');
  });

  test('invalid localStorage theme falls back to violet', async ({ page }) => {
    await page.evaluate(() => localStorage.setItem('orca-theme', 'not-a-theme'));
    await page.reload();
    await page.waitForLoadState('networkidle');

    const theme = await page.evaluate(() => document.documentElement.dataset.theme);
    expect(theme).toBe('violet');
  });
});
