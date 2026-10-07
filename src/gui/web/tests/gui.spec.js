import { test, expect } from '@playwright/test';

test('the visualizer moves while the demo runs', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('.visualizer')).toHaveAttribute('data-live', 'false');
  await page.getByRole('button', { name: 'Start demo' }).click();
  await expect(page.locator('.visualizer')).toHaveAttribute('data-live', 'true');
  await expect.poll(() => page.locator('.visualizer').getAttribute('data-bass').then(Number)).toBeGreaterThan(0);
  await page.getByRole('button', { name: 'Stop demo' }).click();
  await expect(page.locator('.visualizer')).toHaveAttribute('data-live', 'false');
});

test('output selection updates command; demo start and stop never run the backend', async ({ page }) => {
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  await page.goto('/');
  await expect(page.locator('.count')).toHaveText('3');
  await page.getByLabel('Search outputs').fill('Bose');
  await page.getByRole('checkbox', { name: 'Enable Bose Flex SoundLink' }).check();
  await page.getByLabel('Search outputs').fill('');
  await expect(page.locator('.count')).toHaveText('4');
  await expect(page.locator('.command-panel code')).toContainText('pulseaudio:bluez_output.example+180ms@70%');
  await page.locator('.stream-summary').click();
  await page.getByLabel('Listen port').fill('46001');
  await page.getByRole('button',{name:'Done',exact:true}).click();
  await expect(page.locator('.command-panel code')).toContainText('--port 46001');
  await page.getByRole('button', { name: 'Start demo' }).click();
  await expect(page.locator('.session-status')).toHaveText('SIMULATED');
  await page.locator('.stream-summary').click();
  await expect(page.getByLabel('Listen port')).toBeDisabled();
  await page.getByRole('button',{name:'Done',exact:true}).click();
  await expect(page.locator('.meter .lit').first()).toBeVisible();
  await page.screenshot({ path: 'preview-running.png', fullPage: true });
  await page.getByRole('button', { name: 'Stop demo' }).click();
  await expect(page.locator('.session-status')).toHaveText('IDLE');
  await expect(page.locator('.meter .lit')).toHaveCount(0);
  expect(errors).toEqual([]);
});

test('presets persist and restore output-specific settings', async ({ page }) => {
  await page.goto('/');
  await page.locator('.row-gain input').first().fill('60');
  await page.locator('#delay-0').fill('180');
  await page.getByRole('button', { name: 'Save setup', exact: true }).click();
  await page.getByPlaceholder('e.g. Three monitors').fill('Evening mix');
  await page.locator('.preset-form button').click();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Evening mix', exact: true })).toBeVisible();
  await expect(page.locator('#delay-0')).toHaveValue('180');
  await page.locator('#delay-0').fill('0');
  await page.getByRole('button', { name: 'Evening mix', exact: true }).click();
  await expect(page.locator('#delay-0')).toHaveValue('180');
  await page.getByRole('button', { name: 'Delete Evening mix' }).click();
  await expect(page.getByRole('button', { name: 'Evening mix', exact: true })).toHaveCount(0);
});

test('sender preview respects web/microphone', async ({ page }) => {
  await page.goto('/');
  await page.locator('.nav-item').nth(1).click();
  await page.getByLabel('Delivery').selectOption({ label: 'Browsers (noVNC, ws://)' });
  await page.getByLabel('Allow browser microphone').check();
  await page.getByLabel('Opus bit rate').selectOption({ label: '64 kbit/s · slow links' });
  await expect(page.locator('.command-panel code')).toContainText('--bitrate 64');
  await expect(page.locator('.command-panel code')).toContainText('--web');
  await expect(page.locator('.command-panel code')).toContainText('--mic');
  await page.getByLabel('Source', { exact: true }).fill("test's source");
  await expect(page.locator('.command-panel code')).toContainText("'test'\\''s source'");
});

test('compact browser preview has no horizontal overflow', async ({ page }) => {
  await page.setViewportSize({ width: 700, height: 900 });
  await page.goto('/');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
});

test('40 outputs paginate without growing the window', async ({page}) => {
  await page.setViewportSize({width:1040,height:720});
  await page.goto('/');
  await page.getByRole('button',{name:'40-device demo',exact:true}).click();
  await expect(page.locator('.output-pagination')).toContainText('40 devices');
  const rows=await page.locator('.output-row').count();
  expect(rows).toBeLessThan(9);
  expect(await page.evaluate(()=>({w:document.documentElement.scrollWidth,h:document.documentElement.scrollHeight}))).toEqual({w:1040,h:720});
  await page.getByRole('button',{name:'Next outputs page'}).click();
  await expect(page.getByLabel('Enable HDA NVidia, HDMI 3',{exact:true})).toHaveCount(0);
  await page.getByLabel('Search outputs').fill('Output 40');
  await expect(page.locator('.output-row')).toHaveCount(1);
  await page.getByLabel('Enable Output 40',{exact:true}).check();
  await page.getByLabel('Search outputs').fill('');
  await page.getByLabel('Active only',{exact:true}).check();
  await expect(page.locator('.output-pagination')).toContainText('4 results');
  await page.screenshot({path:'preview-paged.png'});
  await page.setViewportSize({width:820,height:620});
  await expect.poll(()=>page.evaluate(()=>document.documentElement.scrollHeight)).toBe(620);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth)).toBe(820);
});
