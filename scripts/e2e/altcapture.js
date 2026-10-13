// Does Keyboard Lock actually hand us Alt+Tab? Inject a real OS-level Alt+Tab
// and see whether the page gets the keydown and the desktop stays put.
const puppeteer = require('puppeteer-core');
const { execSync } = require('child_process');
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const PS = process.argv[5];
const [url, user, pass] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
const inject = () => { try { return execSync(`powershell -NoProfile -ExecutionPolicy Bypass -File "${PS}"`, { encoding: 'utf8' }).trim(); } catch (e) { return 'ERR ' + e.message; } };

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

  await page.evaluate(() => {
    window.__keys = [];
    window.addEventListener('keydown', (e) => {
      window.__keys.push(`${e.code}${e.altKey ? '+alt' : ''}`);
    }, true);
  });

  // Enter fullscreen through the toolbar button (needs a real user gesture).
  await page.mouse.move(640, 4);
  await sleep(600);
  await page.click('#btn-fullscreen');
  await sleep(2500);
  const state = await page.evaluate(() => ({
    fs: !!document.fullscreenElement,
    hasApi: !!(navigator.keyboard && navigator.keyboard.lock),
  }));
  console.log(`fullscreen=${state.fs}  keyboardLockApi=${state.hasApi}`);

  console.log('\n--- injecting real Alt+Tab WITH keyboard lock (fullscreen) ---');
  await page.bringToFront();
  await sleep(500);
  const locked = inject();
  console.log(locked);
  const lockedKeys = await page.evaluate(() => { const k = window.__keys.slice(); window.__keys = []; return k; });
  console.log('page saw: ' + (lockedKeys.join(', ') || '(nothing)'));

  // Control: leave fullscreen, so no lock, and inject the same thing.
  await page.evaluate(() => document.exitFullscreen());
  await sleep(1500);
  await page.bringToFront();
  await sleep(500);
  console.log('\n--- control: same injection, NOT fullscreen (no lock) ---');
  const control = inject();
  console.log(control);
  const controlKeys = await page.evaluate(() => window.__keys.slice());
  console.log('page saw: ' + (controlKeys.join(', ') || '(nothing)'));

  await browser.close();
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
