// Toolbar: drag to reposition, pin, hide/show shortcut, and — the one that
// matters — that dragging never reaches the canvas as remote mouse input.
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
  await sleep(6000);

  // Reveal the toolbar (top-edge hover), then count canvas mousemoves.
  await page.mouse.move(640, 4);
  await sleep(500);
  await page.evaluate(() => {
    window.__canvasMoves = 0;
    document.getElementById('rdp-canvas').addEventListener('mousemove', () => { window.__canvasMoves++; });
  });

  // One-time hint: visible on a first-ever session, gone for good once dismissed.
  const hint1 = await page.evaluate(() => {
    const el = document.getElementById('toolbar-hint');
    const t = el ? el.textContent.replace(/\s+/g, ' ').trim() : '';
    return { shown: el && !el.hidden, text: t, preview: t.slice(0, 70) };
  });
  check('hint shown on first session', hint1.shown, hint1.preview);
  check('hint mentions the shortcut', /Ctrl.*Shift.*H/i.test(hint1.text));
  await page.click('#btn-toolbar-hint-dismiss');
  await sleep(300);
  const hint2 = await page.evaluate(() => ({
    hidden: document.getElementById('toolbar-hint').hidden,
    seen: localStorage.getItem('rdp_toolbar_hint_seen'),
  }));
  check('dismiss hides it and records it', hint2.hidden && hint2.seen === '1');

  const before = await page.evaluate(() => {
    const t = document.getElementById('toolbar').getBoundingClientRect();
    return { x: t.left, y: t.top, visible: document.getElementById('toolbar').classList.contains('visible') };
  });
  check('toolbar visible after hover', before.visible);

  // Drag the grip to (300, 400).
  const grip = await page.evaluate(() => {
    const r = document.getElementById('toolbar-grip').getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  });
  await page.mouse.move(grip.x, grip.y);
  await page.mouse.down();
  for (const [x, y] of [[grip.x - 60, grip.y + 80], [400, 250], [300, 400]]) {
    await page.mouse.move(x, y, { steps: 10 });
    await sleep(60);
  }
  await page.mouse.up();
  await sleep(400);

  const after = await page.evaluate(() => {
    const el = document.getElementById('toolbar');
    const r = el.getBoundingClientRect();
    return {
      x: r.left, y: r.top, moved: el.classList.contains('moved'),
      stored: localStorage.getItem('rdp_toolbar_pos'),
      canvasMoves: window.__canvasMoves,
    };
  });
  check('toolbar moved', after.moved && Math.abs(after.x - before.x) > 50, `x ${Math.round(before.x)} -> ${Math.round(after.x)}, y ${Math.round(before.y)} -> ${Math.round(after.y)}`);
  check('position persisted', !!after.stored, after.stored || '');
  check('no canvas mouse input during drag', after.canvasMoves === 0, `canvas mousemoves=${after.canvasMoves}`);

  // Pin toggles.
  await page.click('#btn-pin');
  await sleep(200);
  const pinned = await page.evaluate(() => ({
    pressed: document.getElementById('btn-pin').getAttribute('aria-pressed'),
    stored: localStorage.getItem('rdp_toolbar_pinned'),
  }));
  check('pin sets pressed state', pinned.pressed === 'true' && pinned.stored === '1');

  // Ctrl+Shift+H hides, then shows.
  await page.keyboard.down('Control'); await page.keyboard.down('Shift');
  await page.keyboard.press('KeyH');
  await page.keyboard.up('Shift'); await page.keyboard.up('Control');
  await sleep(500);
  const hidden = await page.evaluate(() => document.getElementById('toolbar').classList.contains('visible'));
  check('Ctrl+Shift+H hides toolbar', hidden === false);

  await page.keyboard.down('Control'); await page.keyboard.down('Shift');
  await page.keyboard.press('KeyH');
  await page.keyboard.up('Shift'); await page.keyboard.up('Control');
  await sleep(500);
  const shown = await page.evaluate(() => document.getElementById('toolbar').classList.contains('visible'));
  check('Ctrl+Shift+H shows it again', shown === true);

  // Position survives a reload, clamped into the viewport.
  await page.reload({ waitUntil: 'networkidle2' });
  await sleep(1200);
  const restored = await page.evaluate(() => {
    const el = document.getElementById('toolbar');
    return { moved: el.classList.contains('moved'), left: el.style.left, top: el.style.top,
             pinned: document.getElementById('btn-pin').getAttribute('aria-pressed') };
  });
  check('position restored after reload', restored.moved && restored.left !== '', `${restored.left},${restored.top}`);
  check('pin restored after reload', restored.pinned === 'true');
  const hint3 = await page.evaluate(() => document.getElementById('toolbar-hint').hidden);
  check('hint does not come back', hint3 === true);

  console.log(fails ? `\n${fails} FAILED` : '\nall toolbar checks passed');
  await browser.close();
  process.exit(fails ? 1 : 0);
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
