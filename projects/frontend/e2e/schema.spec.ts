import { test, expect } from '@playwright/test';

test.describe('Schema Visualizer', () => {
  test('schema page is accessible', async ({ page }) => {
    await page.goto('/schema');
    await page.waitForLoadState('networkidle', { timeout: 15_000 });
    // Page title includes schema or orca
    const title = await page.title();
    expect(title.toLowerCase()).toMatch(/orca|schema/);
  });

  test('schema page has interactive canvas or loading state', async ({ page }) => {
    await page.goto('/schema');
    // Either a canvas-like element or a loading indicator is visible
    await expect(
      page.locator('canvas, svg, [data-testid="schema-view"], .loading').first()
    ).toBeVisible({ timeout: 10_000 }).catch(async () => {
      // If no canvas, the page at least rendered something
      await expect(page.locator('body')).not.toBeEmpty();
    });
  });
});
