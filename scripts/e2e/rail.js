// RemoteApp / RAIL: publish an app and check a real remote window appears.
const puppeteer = require('puppeteer-core');
// Override with CHROME=/path/to/chrome when Chrome lives elsewhere.
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const [url, user, pass] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
const results = [];
const ok = (n, d) => { results.push(1); console.log(`  PASS  ${n.padEnd(22)} ${d || ''}`); };
const bad = (n, d) => { results.push(0); console.log(`  FAIL  ${n.padEnd(22)} ${d || ''}`); };

(async () => {
  const browser = await puppeteer.launch({
    executablePath: CHROME,
    headless: false, defaultViewport: null,
    args: ['--window-size=1280,860', '--window-position=30,30'],
  });
  const page = (await browser.pages())[0] || (await browser.newPage());
  const logs = [];
  page.on('console', m => logs.push(m.text()));
  page.on('dialog', async d => { try { await d.accept(); } catch (_) {} });

  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.waitForSelector('#username');

  // The picker only exists when the server was started with --enable-remote-app.
  const picker = await page.evaluate(() => {
    const g = document.getElementById('rail-app-group');
    const s = document.getElementById('rail-app');
    return {
      shown: !!g && !g.hidden,
      opts: s ? Array.from(s.options).map(o => `${o.textContent.trim()}|${o.value}`) : [],
    };
  });
  if (picker.shown && picker.opts.length > 1) ok('rail-catalog', picker.opts.join('  '));
  else bad('rail-catalog', `shown=${picker.shown} opts=${JSON.stringify(picker.opts)}`);

  await page.evaluate(() => {
    document.getElementById('fullscreen-preconnect').checked = false;
    const s = document.getElementById('rail-app');
    // Pick the first entry that actually names a program.
    const opt = Array.from(s.options).find(o => o.value && o.value.length > 1);
    if (opt) { s.value = opt.value; s.dispatchEvent(new Event('change', { bubbles: true })); }
  });
  await page.type('#username', user);
  await page.type('#password', pass);
  await page.click('#connect-btn');

  // A RAIL session paints into per-window canvases inside #rail-desktop.
  let appeared = false;
  try {
    await page.waitForFunction(() => {
      const d = document.getElementById('rail-desktop');
      return d && !d.hidden && d.querySelectorAll('canvas').length > 0;
    }, { timeout: 90000 });
    appeared = true;
  } catch (_) {}
  await sleep(6000);

  const st = await page.evaluate(() => {
    const d = document.getElementById('rail-desktop');
    const cs = d ? Array.from(d.querySelectorAll('canvas')) : [];
    const lit = cs.map((c) => {
      const g = c.getContext('2d', { willReadFrequently: true });
      let n = 0, on = 0;
      try {
        const px = g.getImageData(0, 0, c.width, c.height).data;
        const sx = Math.max(1, (c.width / 32) | 0), sy = Math.max(1, (c.height / 24) | 0);
        for (let y = 2; y < c.height; y += sy) for (let x = 2; x < c.width; x += sx) {
          const i = (y * c.width + x) * 4; n++;
          if (px[i] > 12 || px[i + 1] > 12 || px[i + 2] > 12) on++;
        }
      } catch (_) {}
      return { w: c.width, h: c.height, lit: n ? Math.round(100 * on / n) : -1 };
    });
    const tb = document.getElementById('rail-taskbar');
    return {
      windows: lit,
      taskbarItems: document.querySelectorAll('#rail-taskbar-items button').length,
      taskbarShown: !!tb && !tb.hidden,
      mainCanvasHidden: !!document.getElementById('canvas-container')?.hidden,
    };
  });

  // RAIL sessions carry 0x0 helper windows and a full-size blank backdrop, so
  // pick the window that actually has pixels in it, not the biggest one.
  const real = st.windows.filter(w => w.w > 100 && w.h > 100).sort((a, b) => b.lit - a.lit);
  if (appeared && real.length) {
    const w = real[0];
    ok('rail-window', `${st.windows.length} canvases, app window ${w.w}x${w.h}`);
    if (w.lit >= 5) ok('rail-window-paints', `lit ${w.lit}%`);
    else bad('rail-window-paints', `lit ${w.lit}% (blank)`);
  } else {
    bad('rail-window', `no app-sized window (sizes: ${st.windows.map(w => w.w + 'x' + w.h).join(',')})`);
    bad('rail-window-paints', 'n/a');
  }

  const failed = logs.find(l => /exec failed/i.test(l));
  if (failed) bad('rail-exec', failed.slice(0, 90)); else ok('rail-exec', 'launched without an exec error');

  // Keyboard into the published window: type and expect the canvas to change.
  const shot = () => page.evaluate(() => {
    const d = document.getElementById('rail-desktop');
    const stat = (c) => {
      const g = c.getContext('2d', { willReadFrequently: true });
      const px = g.getImageData(0, 0, c.width, c.height).data;
      let h = 0, n = 0, on = 0;
      const sx = Math.max(1, (c.width / 48) | 0), sy = Math.max(1, (c.height / 32) | 0);
      for (let y = 2; y < c.height; y += sy) for (let x = 2; x < c.width; x += sx) {
        const i = (y * c.width + x) * 4; n++;
        if (px[i] > 12 || px[i + 1] > 12 || px[i + 2] > 12) on++;
        h = (h * 31 + px[i] * 7 + px[i + 1] * 11 + px[i + 2] * 13) | 0;
      }
      return { h: h >>> 0, lit: n ? on / n : 0 };
    };
    const cs = Array.from(d.querySelectorAll('canvas')).filter(c => c.width > 100 && c.height > 100);
    if (!cs.length) return 0;
    return cs.map(stat).sort((a, b) => b.lit - a.lit)[0].h;
  });
  // Start from an empty buffer so the screenshot shows only this run's text.
  await page.keyboard.down('Control'); await page.keyboard.press('KeyA'); await page.keyboard.up('Control');
  await page.keyboard.press('Delete');
  await sleep(1500);
  const h0 = await shot();
  await page.keyboard.type('ironbridge rail test', { delay: 90 });
  await sleep(4000);
  const h1 = await shot();
  if (h1 !== h0) ok('rail-keyboard', 'typing reached the published app');
  else bad('rail-keyboard', 'window did not change after typing');

  // Explicit Shift: puppeteer's type() never presses it, and our scancode map
  // is driven by real key events, so capitals need a real ShiftLeft.
  await page.keyboard.press('Enter');
  await page.keyboard.down('ShiftLeft');
  for (const k of ['KeyC', 'KeyA', 'KeyP', 'KeyS']) { await page.keyboard.press(k); await sleep(120); }
  await page.keyboard.up('ShiftLeft');
  await sleep(4000);
  const h2 = await shot();
  if (h2 !== h1) ok('rail-shift', 'Shift+letter reached the app (see screenshot for CAPS)');
  else bad('rail-shift', 'no change from Shift+letters');

  await page.screenshot({ path: 'rail-win.png' });
  console.log('\n--- rail console ---');
  console.log(logs.filter(l => /rail|RAIL|exec|window/i.test(l)).slice(0, 18).join('\n'));

  await browser.close();
  console.log(`\n=== rail: ${results.filter(Boolean).length}/${results.length} passed ===`);
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
