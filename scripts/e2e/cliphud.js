// Clipboard HUD rows: mode per permission state, last-sync direction/age, and
// that copied content never appears in the HUD (only kind + size, masked).
//   node cliphud.js <url> <user> <pass> <grant|deny>
const puppeteer = require('puppeteer-core');
const { execSync } = require('child_process');
const [url, user, pass, perm] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
const ps = (c) => { try { return execSync(`powershell -NoProfile -Sta -Command "${c}"`, { encoding: 'utf8' }).trim(); } catch (e) { return '<err>'; } };
(async () => {
  const b = await puppeteer.launch({ executablePath: 'C:/Program Files/Google/Chrome/Application/chrome.exe', headless: false, defaultViewport: null, args: ['--window-size=1280,860'] });
  const page = (await b.pages())[0];
  const origin = new URL(url).origin;
  if (perm === 'grant') await b.defaultBrowserContext().overridePermissions(origin, ['clipboard-read', 'clipboard-write', 'clipboard-sanitized-write']);
  if (perm === 'deny') await (await page.target().createCDPSession()).send('Browser.setPermission', { origin, permission: { name: 'clipboard-read' }, setting: 'denied' });
  await (await page.target().createCDPSession()).send('Emulation.setFocusEmulationEnabled', { enabled: true });
  page.on('dialog', d => d.accept().catch(() => {}));
  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.evaluate(() => { document.getElementById('fullscreen-preconnect').checked = false; });
  await page.type('#username', user); await page.type('#password', pass);
  await page.click('#connect-btn');
  await page.waitForFunction(() => { const c = document.getElementById('rdp-canvas'); return c && c.width > 0 && !c.closest('.canvas-container').hidden; }, { timeout: 60000 });
  await sleep(8000);
  await page.evaluate(() => document.getElementById('fps-badge').click());
  const hud = () => page.evaluate(() => ({
    mode: document.getElementById('hud-clip-mode').textContent, tip: document.getElementById('hud-clip-mode').title,
    sync: document.getElementById('hud-clip-sync').textContent, content: document.getElementById('hud-clip-content').textContent,
    all: document.getElementById('perf-hud').textContent }));
  let h = await hud();
  console.log(`[${perm}] at connect: mode="${h.mode}" sync="${h.sync}" content="${h.content}"`);
  const secret = 'SECRET-' + Math.random().toString(36).slice(2, 8);
  ps(`Set-Clipboard -Value '${secret}'`); await sleep(800);
  if (perm !== 'grant') { await page.keyboard.down('Control'); await page.keyboard.press('KeyV'); await page.keyboard.up('Control'); }
  await sleep(2500);
  h = await hud();
  console.log(`[${perm}] after host copy (${secret.length} chars): sync="${h.sync}" content="${h.content}" leaked=${h.all.includes(secret)}`);
  await sleep(3000); h = await hud(); console.log(`[${perm}] 3 s later: sync="${h.sync}"`);
  // session -> PC: Run dialog csettext (REMOTE-TEXT-MARKER, 18 chars)
  await page.keyboard.down('MetaLeft'); await page.keyboard.press('KeyR'); await page.keyboard.up('MetaLeft'); await sleep(1800);
  await page.keyboard.type('csettext', { delay: 100 }); await page.keyboard.press('Delete'); await page.keyboard.press('Enter'); await sleep(4500);
  h = await hud();
  console.log(`[${perm}] after session copy: sync="${h.sync}" content="${h.content}" leaked=${h.all.includes('REMOTE-TEXT-MARKER')}`);
  console.log(`[${perm}] tooltip: ${h.tip}`);
  await page.screenshot({ path: `cliphud-${perm}.png` });
  await b.close();
})().catch(e => { console.error('ERR', e.message); process.exit(1); });
