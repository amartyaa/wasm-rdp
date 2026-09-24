// Decisive pointer-motion probe: hovering a maximised window's close button
// turns it red on Windows, and highlights on GNOME/Cinnamon too. No clicks.
const puppeteer = require('puppeteer-core');
// Override with CHROME=/path/to/chrome when Chrome lives elsewhere.
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const [url, user, pass, label] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

(async () => {
  const browser = await puppeteer.launch({
    executablePath: CHROME,
    headless: false, defaultViewport: null,
    args: ['--window-size=1280,860', '--window-position=30,30'],
  });
  const page = (await browser.pages())[0] || (await browser.newPage());
  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.waitForSelector('#username');
  await page.evaluate(() => { document.getElementById('fullscreen-preconnect').checked = false; });
  await page.type('#username', user);
  await page.type('#password', pass);
  await page.click('#connect-btn');
  await page.waitForFunction(() => {
    const c = document.getElementById('rdp-canvas');
    return c && c.width > 0 && !c.closest('.canvas-container').hidden;
  }, { timeout: 60000 });
  await sleep(10000);

  // Sample a small patch only, so a local hover highlight cannot be diluted.
  const patch = (fx, fy) => page.evaluate(([fx, fy]) => {
    const c = document.getElementById('rdp-canvas');
    const g = c.getContext('2d', { willReadFrequently: true });
    const x = Math.max(0, Math.min(c.width - 40, Math.round(c.width * fx) - 20));
    const y = Math.max(0, Math.min(c.height - 20, Math.round(c.height * fy) - 10));
    const d = g.getImageData(x, y, 40, 20).data;
    let r = 0, gg = 0, b = 0;
    for (let i = 0; i < d.length; i += 4) { r += d[i]; gg += d[i + 1]; b += d[i + 2]; }
    const n = d.length / 4;
    return { r: Math.round(r / n), g: Math.round(gg / n), b: Math.round(b / n) };
  }, [fx, fy]);

  const rect = await page.evaluate(() => {
    const r = document.getElementById('rdp-canvas').getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height };
  });
  const at = (fx, fy) => [Math.round(rect.x + rect.w * fx), Math.round(rect.y + rect.h * fy)];

  // Park the pointer away from the target, sample, then hover it and re-sample.
  const TX = 0.984, TY = 0.023;               // close button of a maximised window
  await page.mouse.move(...at(0.5, 0.5));
  await sleep(3000);
  const cold = await patch(TX, TY);
  await page.mouse.move(...at(TX, TY));
  await sleep(3000);
  const hot = await patch(TX, TY);
  await page.mouse.move(...at(0.5, 0.5));
  await sleep(3000);
  const back = await patch(TX, TY);

  const d = (a, b) => Math.abs(a.r - b.r) + Math.abs(a.g - b.g) + Math.abs(a.b - b.b);
  console.log(`${label} close-button patch  cold rgb(${cold.r},${cold.g},${cold.b})  hovered rgb(${hot.r},${hot.g},${hot.b})  unhovered rgb(${back.r},${back.g},${back.b})`);
  console.log(`  delta on hover = ${d(cold, hot)},  delta after leaving = ${d(hot, back)}`);
  console.log(d(cold, hot) > 25 ? '  PASS  pointer motion tracked by the remote'
                                : '  FAIL  no hover response at the close button');
  await page.screenshot({ path: `hover-${label}.png` });
  await browser.close();
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
