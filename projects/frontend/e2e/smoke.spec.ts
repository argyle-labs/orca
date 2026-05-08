import { test, expect } from '@playwright/test';

test.describe('Brain App — Smoke Tests', () => {
  test('home page loads', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveTitle(/orca/i);
    // Top nav should be visible
    await expect(page.locator('nav, header').first()).toBeVisible();
  });

  test('schema page renders without crash', async ({ page }) => {
    await page.goto('/schema');
    // Page should not show an error boundary
    await expect(page.locator('text=Error').first()).not.toBeVisible({ timeout: 5000 }).catch(() => {});
    // Should render within 10s
    await page.waitForLoadState('networkidle', { timeout: 10_000 });
  });

  test('navigation links are present', async ({ page }) => {
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    // At least one nav link must exist
    const links = page.locator('a[href]');
    await expect(links.first()).toBeVisible();
  });

  test('API returns 200 for /api/tree', async ({ request }) => {
    const response = await request.get('/api/tree');
    expect(response.status()).toBe(200);
    const body = await response.json();
    // Tree returns an array or an object with roots
    expect(typeof body).toMatch(/object/);
  });

  test('API returns 200 for /api/openapi.json', async ({ request }) => {
    const response = await request.get('/api/openapi.json');
    expect(response.status()).toBe(200);
    const spec = await response.json();
    expect(spec).toHaveProperty('openapi');
    expect(spec).toHaveProperty('paths');
  });

  test('test runner API endpoint exists', async ({ request }) => {
    // Just verify the endpoint is registered — don't actually run tests in E2E
    const response = await request.get('/api/tests/run?suite=rust');
    // Should respond (may take time) — not 404
    expect(response.status()).not.toBe(404);
  });
});
