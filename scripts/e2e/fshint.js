// Fullscreen-shortcuts hint: shown once on first keyboard-lock-capturing
// fullscreen entry, dismissible, never comes back.
const puppeteer = require('puppeteer-core');
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const [url, user, pass] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
let fails = 0;
const check = (name, ok, extra) => { console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? '  ' + extra : ''}`); if (!ok) fails++; };

(async () => {
  const browser = await puppeteer.launch({ executablePath: CHROME, headless: false, defaultViewport: null,
    args: ['--window-size=1280,860', '--window-position=30,30'] });
  const page = (await browser.pages())[0] || (await browser.newPage());
  page.on('dialog', async d => { try { await d.accept(); } catch (_) {} });
  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.waitForSelector('#username');
  await page.evaluate(() => { document.getElementById('fullscreen-preconnect').checked = false; });
  await page.type('#username', user); await page.type('#password', pass);
  await page.click('#connect-btn');
  await page.waitForFunction(() => {
    const c = document.getElementById('rdp-canvas');
    return c && c.width > 0 && !c.closest('.canvas-container').hidden;
  }, { timeout: 60000 });
  await sleep(4000);

  const beforeFs = await page.evaluate(() => document.getElementById('fullscreen-hint').hidden);
  check('hint not shown before fullscreen', beforeFs === true);

  await page.mouse.move(640, 4);
  await sleep(500);
  await page.click('#btn-fullscreen');
  await sleep(1500);

  const hint1 = await page.evaluate(() => {
    const el = document.getElementById('fullscreen-hint');
    const t = el.textContent.replace(/\s+/g, ' ').trim();
    return { shown: !el.hidden, text: t, preview: t.slice(0, 70) };
  });
  check('hint shown on first fullscreen entry', hint1.shown, hint1.preview);
  check('hint mentions Esc hold', /Esc/.test(hint1.text));
  check('hint mentions Alt+Tab', /Alt\+Tab/.test(hint1.text));

  await page.click('#btn-fullscreen-hint-dismiss');
  await sleep(300);
  const hint2 = await page.evaluate(() => ({
    hidden: document.getElementById('fullscreen-hint').hidden,
    seen: localStorage.getItem('rdp_fullscreen_hint_seen'),
  }));
  check('dismiss hides it and records it', hint2.hidden && hint2.seen === '1');

  // Leave and re-enter fullscreen: must not come back.
  await page.evaluate(() => document.exitFullscreen());
  await sleep(1000);
  await page.mouse.move(640, 4);
  await sleep(500);
  await page.click('#btn-fullscreen');
  await sleep(1500);
  const hint3 = await page.evaluate(() => document.getElementById('fullscreen-hint').hidden);
  check('hint does not reappear on second fullscreen entry', hint3 === true);

  // Survives reload too.
  await page.reload({ waitUntil: 'networkidle2' });
  await sleep(1000);
  const seenAfterReload = await page.evaluate(() => localStorage.getItem('rdp_fullscreen_hint_seen'));
  check('seen-flag persisted across reload', seenAfterReload === '1');

  console.log(fails ? `\n${fails} FAILED` : '\nall fullscreen-hint checks passed');
  await browser.close();
  process.exit(fails ? 1 : 0);
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
