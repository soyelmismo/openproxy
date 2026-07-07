import { test, expect, type Page } from '@playwright/test';

test.describe('Combo Re-arrange functionality', () => {
  test('Rearranging targets inside a combo with the down arrow', async ({ page }: { page: Page }) => {
    // Navigate to a specific combo view
    const comboId = 1;
    
    // 1. Mock the API responses to simulate a combo with two targets
    await page.route(`**/admin/api/combos/${comboId}`, async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          id: comboId,
          name: 'Test Combo',
          strategy: 'fallback',
          priority_mode: 'strict'
        })
      });
    });

    await page.route(`**/admin/api/combos/${comboId}/targets`, async route => {
      if (route.request().method() === 'GET') {
        await route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([
            {
              id: 101,
              combo_id: comboId,
              priority_order: 1,
              model_row_id: 201,
              provider_id: 'gemini',
              model_id: 'gemini-1.5-flash',
              weight: 1
            },
            {
              id: 102,
              combo_id: comboId,
              priority_order: 2,
              model_row_id: 202,
              provider_id: 'openai',
              model_id: 'gpt-4o',
              weight: 1
            }
          ])
        });
      } else {
        await route.continue();
      }
    });

    // 2. Set up a listener for the reorder POST request
    let reorderPayload: any = null;
    await page.route(`**/admin/api/combos/${comboId}/targets/reorder`, async route => {
      expect(route.request().method()).toBe('POST');
      reorderPayload = JSON.parse(route.request().postData() || '{}');
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ success: true })
      });
    });

    // Go to the combo detail view
    await page.goto(`/#/combos/${comboId}`);

    // Wait for the rows to render
    // Row 101 is gemini, row 102 is openai
    const row1 = page.locator('tr[data-drag-id="101"]');
    const row2 = page.locator('tr[data-drag-id="102"]');
    await row1.waitFor({ state: 'visible' });
    await row2.waitFor({ state: 'visible' });

    // Assert initial order in DOM
    const rows = await page.locator('tbody tr').all();
    expect(await rows[0].getAttribute('data-drag-id')).toBe('101');
    expect(await rows[1].getAttribute('data-drag-id')).toBe('102');

    // Click the "Down" button on the first row (target 101)
    const downBtn = row1.locator('button:text-is("↓")');
    await downBtn.click();

    // Verify the reorder API was called with [102, 101]
    expect(reorderPayload).not.toBeNull();
    expect(reorderPayload.target_ids).toEqual([102, 101]);
  });
});
