// End-to-end feature sweep against a live IronBridge session: connect/NLA,
// rendering, HUD, keyboard + mouse round trips, clipboard, dynamic resize,
// both fullscreen paths, hi-dpi and graceful disconnect. Audio is not covered.
//
//   node e2e.js <url> <user> <pass> <label> [flags]
//
//   --clip          assert clipboard sync (proxy needs --enable-text-clipboard-sync)
//   --hidpi         tick the High-DPI toggle and force a 2x device scale factor
//   --quick         two resizes and one fullscreen cycle instead of the full set
//   --domain=X      fill the domain field
//   --login=PASS    log in past a GDM greeter (grd Remote Login lands there);
//                   this is the host's PAM password, not the RDP credential
//
// Writes e2e-<label>.json, e2e-<label>-logs.txt and e2e-<label>-final.png to
// the working directory. Exits non-zero if any check fails.
const puppeteer = require('puppeteer-core');
// Override with CHROME=/path/to/chrome when Chrome lives elsewhere.
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const fs = require('fs');

const argv = process.argv.slice(2);
const [url, user, pass, label] = argv.filter(a => !a.startsWith('--'));
const flag = (n) => argv.includes('--' + n);
const opt = (n, d) => (argv.find(a => a.startsWith('--' + n + '=')) || '=' + d).split('=')[1];

const results = [];
const ok = (name, detail) => { results.push({ name, pass: true, detail }); console.log(`  PASS  ${name.padEnd(22)} ${detail || ''}`); };
const bad = (name, detail) => { results.push({ name, pass: false, detail }); console.log(`  FAIL  ${name.padEnd(22)} ${detail || ''}`); };
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

function finish() {
  const p = results.filter(r => r.pass).length;
  console.log(`\n=== ${label}: ${p}/${results.length} passed ===`);
  fs.writeFileSync(`e2e-${label}.json`, JSON.stringify(results, null, 2));
  process.exit(results.some(r => !r.pass) ? 1 : 0);
}

(async () => {
  const browser = await puppeteer.launch({
    executablePath: CHROME,
    headless: false, defaultViewport: null,
    // This display is dpr 1.0, so the hi-dpi path is a no-op unless we force a
    // scale factor; 2x makes `logical x dpr` observably different from logical.
    args: ['--window-size=1280,860', '--window-position=30,30']
      .concat(flag('hidpi') ? ['--force-device-scale-factor=2'] : []),
  });
  const page = (await browser.pages())[0] || (await browser.newPage());
  const t0 = Date.now();
  const logs = [];
  page.on('console', m => logs.push(`[${((Date.now() - t0) / 1000).toFixed(1)}s] ${m.text()}`));
  page.on('pageerror', e => logs.push(`[pageerror] ${e.message}`));
  const ctx = browser.defaultBrowserContext();
  try { await ctx.overridePermissions(url, ['clipboard-read', 'clipboard-write']); } catch (_) {}

  const cdp = await page.createCDPSession();
  const { windowId } = await cdp.send('Browser.getWindowForTarget');

  // ---- page state probe -------------------------------------------------
  const probe = () => page.evaluate(() => {
    const c = document.getElementById('rdp-canvas');
    if (!c || !c.width) return { alive: false, lit: -1, hash: 0, hud: {}, rect: { x: 0, y: 0, w: 0, h: 0 } };
    const g = c.getContext('2d', { willReadFrequently: true });
    let lit = 0, n = 0, hash = 0;
    try {
      const d = g.getImageData(0, 0, c.width, c.height).data;
      const sx = Math.max(1, (c.width / 48) | 0), sy = Math.max(1, (c.height / 32) | 0);
      for (let y = 2; y < c.height; y += sy) for (let x = 2; x < c.width; x += sx) {
        const i = (y * c.width + x) * 4; n++;
        if (d[i] > 12 || d[i + 1] > 12 || d[i + 2] > 12) lit++;
        hash = (hash * 31 + d[i] * 7 + d[i + 1] * 11 + d[i + 2] * 13) | 0;
      }
    } catch (_) { return { alive: false, lit: -1, hash: 0, hud: {}, rect: { x: 0, y: 0, w: 0, h: 0 } }; }
    const txt = (id) => (document.getElementById(id) || {}).textContent ? document.getElementById(id).textContent.trim() : '';
    const r = c.getBoundingClientRect();
    return {
      alive: true, bw: c.width, bh: c.height,
      rect: { x: r.x, y: r.y, w: r.width, h: r.height },
      vw: window.innerWidth, vh: window.innerHeight, dpr: window.devicePixelRatio,
      lit: n ? Math.round(100 * lit / n) : -1, hash: hash >>> 0,
      hud: {
        latency: txt('hud-latency'), fps: txt('hud-fps'), res: txt('hud-resolution'),
        codec: txt('hud-codec'), resize: txt('hud-resize'), ver: txt('hud-version'),
      },
      fs: !!document.fullscreenElement,
    };
  });
  // A GPU-less host can take seconds to repaint a large desktop after a
  // reactivation, so a single black sample is not yet a failure: re-probe and
  // report how long it took to come back.
  const settle = async (extraMs = 12000) => {
    let r = await probe();
    if (r.lit >= 5) return { r, late: 0 };
    const t = Date.now();
    while (Date.now() - t < extraMs) {
      await sleep(1000);
      r = await probe();
      if (r.lit >= 5) return { r, late: Math.round((Date.now() - t) / 100) / 10 };
    }
    return { r, late: -1 };
  };
  const cx0 = (r) => Math.round(r.rect.x + r.rect.w / 2);
  const cy0 = (r) => Math.round(r.rect.y + r.rect.h / 2);
  const mark = () => logs.length;
  const framesSince = (i) => logs.slice(i).filter(l => /prog frame|bitmap update|SolidFill|surface|frame/i.test(l)).length;

  // ---- 1. login page ----------------------------------------------------
  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.waitForSelector('#username', { timeout: 20000 });
  const ver = await page.evaluate(() => {
    const e = document.getElementById('app-version');
    return e && e.textContent ? e.textContent.trim() : '';
  });
  if (ver) ok('login-page', `version "${ver}"`); else bad('login-page', 'no version string injected');

  // ---- 2. connect -------------------------------------------------------
  // app.js only picks the toggle up from its `change` listener, so assigning
  // .checked alone leaves advHidpi false.
  await page.evaluate((hidpi) => {
    document.getElementById('fullscreen-preconnect').checked = false;
    const h = document.getElementById('adv-hidpi');
    if (h) { h.checked = hidpi; h.dispatchEvent(new Event('change', { bubbles: true })); }
  }, flag('hidpi'));
  const dpr0 = await page.evaluate(() => window.devicePixelRatio);
  console.log(`  ----  devicePixelRatio ${dpr0}, hidpi toggle ${flag('hidpi') ? 'ON' : 'off'}`);
  const dom = opt('domain', '');
  if (dom) { await page.click('#domain'); await page.type('#domain', dom); }
  await page.type('#username', user);
  await page.type('#password', pass);
  const tConn = Date.now();
  await page.click('#connect-btn');
  try {
    await page.waitForFunction(() => {
      const c = document.getElementById('rdp-canvas');
      const cc = c && c.closest('.canvas-container');
      return c && c.width > 0 && cc && !cc.hidden;
    }, { timeout: 60000 });
    ok('connect-nla', `${((Date.now() - tConn) / 1000).toFixed(1)}s`);
  } catch (e) {
    const err = await page.evaluate(() => {
      const e2 = document.getElementById('login-error');
      return e2 ? e2.textContent : '';
    });
    bad('connect-nla', `timeout; login-error="${err}"`);
    fs.writeFileSync(`e2e-${label}-logs.txt`, logs.join('\n'));
    await browser.close();
    return finish();
  }
  await sleep(8000);

  // ---- 2b. greeter login ------------------------------------------------
  // grd Remote Login hands us a GDM user-picker, not a desktop. Without this
  // every input test silently probes the greeter and proves nothing.
  const deskpass = opt('login', '');
  if (deskpass) {
    const pre = await probe();
    // The user tile sits above the canvas midpoint; clicking it opens the
    // password prompt. NB this is the PAM password, not grd's RDP credential.
    await page.mouse.click(Math.round(pre.rect.x + pre.rect.w * 0.5), Math.round(pre.rect.y + pre.rect.h * 0.425));
    await sleep(4000);
    for (const ch of deskpass) { await page.keyboard.type(ch); await sleep(80); }
    await sleep(1000);
    await page.keyboard.press('Enter');
    await sleep(30000);
    const post = await probe();
    // A real desktop has a top bar and wallpaper: demand a big change and
    // that the greeter's dark grey ground is gone.
    if (post.hash !== pre.hash && post.lit >= 5) ok('desktop-login', `reached a session, lit ${post.lit}%`);
    else bad('desktop-login', 'still at the greeter after entering the password');
  }

  // ---- 3. render --------------------------------------------------------
  let s = await probe();
  if (s.lit >= 5) ok('render-desktop', `${s.bw}x${s.bh} lit ${s.lit}%`);
  else bad('render-desktop', `lit ${s.lit}% (black screen)`);

  // ---- 4. HUD -----------------------------------------------------------
  await page.click('#fps-badge');
  await sleep(2500);
  s = await probe();
  const latMs = parseInt(s.hud.latency, 10);
  if (latMs >= 0 && !isNaN(latMs)) ok('hud-latency', s.hud.latency); else bad('hud-latency', `"${s.hud.latency}"`);
  if (/[x\u00d7]/.test(s.hud.res)) ok('hud-resolution', s.hud.res); else bad('hud-resolution', `"${s.hud.res}"`);
  if (s.hud.codec) ok('hud-codec', s.hud.codec); else bad('hud-codec', 'empty');
  if (s.hud.ver) ok('hud-version', s.hud.ver); else bad('hud-version', 'empty');

  // ---- 5/6. mouse + keyboard round trip ---------------------------------
  // Super opens Activities / the Cinnamon menu / Start on all three targets;
  // right-clicking a bare GNOME Shell desktop legitimately paints nothing, so
  // it is no use as a cross-target probe.
  const cx = Math.round(s.rect.x + s.rect.w / 2), cy = Math.round(s.rect.y + s.rect.h / 2);
  const before = s.hash;
  let i = mark();
  await page.mouse.move(cx, cy); await sleep(300);
  await page.keyboard.down('MetaLeft'); await sleep(120); await page.keyboard.up('MetaLeft');
  await sleep(4000);
  const opened = await probe();
  if (opened.hash !== before) ok('keyboard-super', `menu opened, ${framesSince(i)} gfx msgs`);
  else bad('keyboard-super', 'no repaint after Super');

  // Mouse, with that menu still open: hover the lower band, where Windows
  // puts pinned Start tiles, GNOME its dash and Cinnamon its menu entries.
  // Empty desktop space highlights nothing, so it is useless as a probe.
  i = mark();
  for (const [fx, fy] of [[0.5, 0.75], [0.45, 0.82], [0.55, 0.72], [0.18, 0.8], [0.5, 0.88]]) {
    await page.mouse.move(Math.round(s.rect.x + s.rect.w * fx), Math.round(s.rect.y + s.rect.h * fy));
    await sleep(700);
  }
  await sleep(2500);
  const hovered = await probe();
  if (hovered.hash !== opened.hash) ok('mouse-move', `hover highlight, ${framesSince(i)} gfx msgs`);
  else bad('mouse-move', 'no repaint while hovering the open menu');

  i = mark();
  await page.keyboard.press('Escape');
  await sleep(3000);
  const afterEsc = await probe();
  if (afterEsc.hash !== hovered.hash) ok('keyboard-escape', `menu dismissed, ${framesSince(i)} gfx msgs`);
  else if (afterEsc.hash === before) ok('keyboard-escape', 'already back at the original desktop');
  else bad('keyboard-escape', 'no repaint after Escape and not back at desktop');

  // Buttons: desktop context menus differ per DE and may be covered by a
  // maximised window, so this is reported, not asserted.
  i = mark();
  await page.mouse.click(cx, cy, { button: 'right' });
  await sleep(3000);
  const rc = await probe();
  results.push({ name: 'mouse-button', pass: true, detail: 'informational' });
  console.log(`  INFO  ${'mouse-button'.padEnd(22)} right-click ${rc.hash !== afterEsc.hash ? 'opened a menu' : 'hit no menu target here'}`);
  await page.keyboard.press('Escape');
  await sleep(2000);

  // ---- 7. clipboard (only when the server has sync enabled) -------------
  // navigator.clipboard.writeText needs a focused window, which a driven
  // browser often isn't; dispatch the paste event app.js actually listens for.
  if (flag('clip')) {
    const chanUp = logs.some(l => /Clipboard: channel ready|Clipboard virtual channel initialized/i.test(l));
    if (chanUp) ok('clipboard-channel', 'CLIPRDR negotiated'); else bad('clipboard-channel', 'never initialized');

    const token = 'IronBridge-e2e-' + Date.now();
    i = mark();
    await page.mouse.click(cx, cy);
    await page.evaluate((t) => {
      const dt = new DataTransfer();
      dt.setData('text/plain', t);
      document.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
    }, token);
    await sleep(3000);
    const advertised = logs.slice(i).some(l => /sending format list/i.test(l));
    const accepted = logs.slice(i).some(l => /Remote accepted format list/i.test(l));
    if (advertised && accepted) ok('clipboard-local2remote', 'format list sent + accepted by remote');
    else if (advertised) bad('clipboard-local2remote', 'advertised but remote never accepted');
    else bad('clipboard-local2remote', 'paste never reached CLIPRDR');
  }

  // ---- 8. resize --------------------------------------------------------
  const doResize = async (n, w, h) => {
    const j = mark();
    await cdp.send('Browser.setWindowBounds', { windowId, bounds: { windowState: 'normal' } });
    await cdp.send('Browser.setWindowBounds', { windowId, bounds: { width: w, height: h } });
    await sleep(9000);
    const { r, late } = await settle();
    const want = r.vw * (flag('hidpi') ? r.dpr : 1);
    const followed = Math.abs(r.bw - want) < 60;
    const lateNote = late > 0 ? ` (black ${late}s then recovered)` : late < 0 ? ' STILL BLACK' : '';
    const detail = `backing ${r.bw}x${r.bh} vp ${r.vw}x${r.vh} lit ${r.lit}% hud "${r.hud.resize}" frames ${framesSince(j)}${lateNote}`;
    if (followed && r.lit >= 5) ok(`resize-${n}`, detail); else bad(`resize-${n}`, detail);
  };
  // Bounds are DIP, so at dpr 2 anything over ~960 wide is clamped to the
  // screen and the window never actually moves.
  const sc = flag('hidpi') ? 0.6 : 1;
  const R = (w, h) => [Math.round(w * sc), Math.round(h * sc)];
  await doResize(1, ...R(1000, 720));
  await doResize(2, ...R(1180, 800));
  if (!flag('quick')) { await doResize(3, ...R(900, 640)); await doResize(4, ...R(1240, 840)); }

  // ---- 9a. toolbar fullscreen toggle (Element.requestFullscreen path) ---
  // The toolbar button is a toggle, so click it for both directions. F11
  // cannot be driven here: app.js maps F11 to RDP scancode 0x57 and forwards
  // it to the remote, and CDP synthetic keys never reach browser chrome.
  const note = (late) => late > 0 ? ` (black ${late}s then recovered)` : late < 0 ? ' STILL BLACK' : '';

  // In fullscreen the toolbar hides itself after 3s and only comes back when
  // the pointer reaches the top edge (clientY < 10), so reveal it before
  // clicking or the button is not a click target.
  const clickFullscreenBtn = async () => {
    const r = await probe();
    await page.mouse.move(Math.round(r.vw / 2), 400);
    await page.mouse.move(Math.round(r.vw / 2), 3);
    await sleep(800);
    await page.click('#btn-fullscreen');
  };

  for (let k = 1; k <= (flag('quick') ? 1 : 3); k++) {
    let j = mark();
    await clickFullscreenBtn();
    await sleep(8000);
    let { r, late } = await settle();
    let want = r.vw * (flag('hidpi') ? r.dpr : 1);
    const d1 = `backing ${r.bw}x${r.bh} vp ${r.vw}x${r.vh} lit ${r.lit}% frames ${framesSince(j)}${note(late)}`;
    if (r.fs && r.lit >= 5 && Math.abs(r.bw - want) < 60) ok(`fullscreen-${k}`, d1);
    else bad(`fullscreen-${k}`, `fs=${r.fs} ${d1}`);

    j = mark();
    await clickFullscreenBtn();
    await sleep(8000);
    ({ r, late } = await settle());
    want = r.vw * (flag('hidpi') ? r.dpr : 1);
    const d2 = `backing ${r.bw}x${r.bh} vp ${r.vw}x${r.vh} lit ${r.lit}% frames ${framesSince(j)}${note(late)}`;
    if (!r.fs && r.lit >= 5 && Math.abs(r.bw - want) < 60) ok(`windowed-${k}`, d2);
    else bad(`windowed-${k}`, `fs=${r.fs} ${d2}`);
  }

  // ---- 9b. browser fullscreen (the F11 path: resize, no fullscreenchange)
  for (let k = 1; k <= (flag('quick') ? 1 : 2); k++) {
    let j = mark();
    await cdp.send('Browser.setWindowBounds', { windowId, bounds: { windowState: 'fullscreen' } });
    await sleep(8000);
    let { r, late } = await settle();
    let want = r.vw * (flag('hidpi') ? r.dpr : 1);
    const d1 = `backing ${r.bw}x${r.bh} vp ${r.vw}x${r.vh} lit ${r.lit}% hud "${r.hud.resize}" frames ${framesSince(j)}${note(late)}`;
    if (r.lit >= 5 && Math.abs(r.bw - want) < 60) ok(`f11-fullscreen-${k}`, d1); else bad(`f11-fullscreen-${k}`, d1);

    j = mark();
    await cdp.send('Browser.setWindowBounds', { windowId, bounds: { windowState: 'normal' } });
    await sleep(8000);
    ({ r, late } = await settle());
    want = r.vw * (flag('hidpi') ? r.dpr : 1);
    const d2 = `backing ${r.bw}x${r.bh} vp ${r.vw}x${r.vh} lit ${r.lit}% hud "${r.hud.resize}" frames ${framesSince(j)}${note(late)}`;
    if (r.lit >= 5 && Math.abs(r.bw - want) < 60) ok(`f11-windowed-${k}`, d2); else bad(`f11-windowed-${k}`, d2);
  }

  // ---- 10. hidpi backing ratio -----------------------------------------
  if (flag('hidpi')) {
    const r = await probe();
    const ratio = r.bw / r.vw;
    const expect = Math.min(2, r.dpr);
    if (r.dpr <= 1.05) ok('hidpi-supersample', `SKIPPED - display is dpr ${r.dpr}, nothing to supersample`);
    else if (Math.abs(ratio - expect) < 0.15) ok('hidpi-supersample', `backing/vp = ${ratio.toFixed(2)}x at dpr ${r.dpr}`);
    else bad('hidpi-supersample', `backing/vp = ${ratio.toFixed(2)}x at dpr ${r.dpr}, expected ~${expect.toFixed(2)}x`);
  }

  // ---- 11. still interactive after all that ----------------------------
  const st = await probe();
  i = mark();
  await page.keyboard.down('MetaLeft'); await sleep(120); await page.keyboard.up('MetaLeft');
  await sleep(4000);
  const alive1 = await probe();
  const gfx = framesSince(i);
  await page.keyboard.press('Escape');
  await sleep(2500);
  const fin = await probe();
  const detail = `lit ${fin.lit}% repaint=${alive1.hash !== st.hash} gfx=${gfx}`;
  if (fin.lit >= 5 && (alive1.hash !== st.hash || gfx > 0)) ok('session-alive-after', detail);
  else bad('session-alive-after', `${detail} (frozen?)`);

  await page.screenshot({ path: `e2e-${label}-final.png` });

  // ---- 12. graceful disconnect -----------------------------------------
  page.on('dialog', async d => { try { await d.accept(); } catch (_) {} });
  await page.click('#btn-disconnect');
  await sleep(5000);
  const back = await page.evaluate(() => {
    const l = document.getElementById('login-screen');
    return !!l && !l.hidden && getComputedStyle(l).display !== 'none';
  });
  if (back) ok('disconnect', 'returned to login screen'); else bad('disconnect', 'login screen not restored');

  fs.writeFileSync(`e2e-${label}-logs.txt`, logs.join('\n'));
  await browser.close();
  finish();
})().catch(e => { console.error('HARNESS ERROR:', e.stack); process.exit(2); });
