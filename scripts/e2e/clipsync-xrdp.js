// xrdp clipboard sync, judged by the session X clipboard (xclip via the WSL crun/cdump helpers).
//   node clipsync-xrdp.js <url> <user> <PAM pass> <grant|prompt>
const puppeteer = require('puppeteer-core');
const { execSync } = require('child_process');
const [url, user, pass, perm, pam] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
const sh = (c) => { try { return execSync(c, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).replace(/\0/g, '').trim(); } catch (e) { return '<err>'; } };
const ps = (c) => sh(`powershell -NoProfile -Sta -Command "${c}"`);
const crun = (n) => sh(`wsl -d Ubuntu-26.04 -u root -- /usr/local/bin/crun ${n}`);
const dump = () => { crun('cdump'); return sh('wsl -d Ubuntu-26.04 -- cat /cliptest/dump.txt'); };
const row = (n, ok, d) => console.log(`${ok ? 'OK    ' : 'BROKEN'} ${n.padEnd(36)} ${d}`);
(async () => {
  const tag = Math.random().toString(36).slice(2, 6).toUpperCase();
  ps(`Set-Clipboard -Value 'HOST-X-${tag}'`);
  const b = await puppeteer.launch({ executablePath: 'C:/Program Files/Google/Chrome/Application/chrome.exe', headless: false, defaultViewport: null, args: ['--window-size=1280,860'] });
  const origin = new URL(url).origin;
  if (perm === 'grant') await b.defaultBrowserContext().overridePermissions(origin, ['clipboard-read', 'clipboard-write', 'clipboard-sanitized-write']);
  const page = (await b.pages())[0];
  await (await page.target().createCDPSession()).send('Emulation.setFocusEmulationEnabled', { enabled: true });
  const logs = []; const t0 = Date.now();
  page.on('console', m => { const t = m.text(); if (/clip|format/i.test(t)) logs.push(`${((Date.now() - t0) / 1000).toFixed(2)} ${t}`); });
  page.on('dialog', d => d.accept().catch(() => {}));
  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.evaluate(() => { document.getElementById('fullscreen-preconnect').checked = false; });
  await page.type('#username', user); await page.type('#password', pass);
  await page.click('#connect-btn');
  await page.waitForFunction(() => { const c = document.getElementById('rdp-canvas'); return c && c.width > 0 && !c.closest('.canvas-container').hidden; }, { timeout: 90000 });
  await sleep(15000);
  const key = async (m, k) => { await page.keyboard.down(m); await page.keyboard.press(k); await page.keyboard.up(m); };

  row('channel handshake completes', logs.some(l => /virtual channel initialized/.test(l)), '');
  let d = dump();
  if (perm === 'grant') row('host copy reaches session at connect', d.includes(`HOST-X-${tag}`), d);
  ps(`Set-Clipboard -Value 'HOST-Y-${tag}'`); await sleep(perm === 'grant' ? 1500 : 300);
  if (perm !== 'grant') { await key('Control', 'KeyV'); await sleep(1500); }
  d = dump(); row(perm === 'grant' ? 'host change synced with no paste' : 'host change synced by Ctrl+V', d.includes(`HOST-Y-${tag}`), d);

  crun('csettext'); await sleep(2500);
  const host = ps('Get-Clipboard -Raw');
  row('session copy reaches host', host === 'REMOTE-TEXT-MARKER', `host "${host}"`);
  const mark = logs.length;
  await key('Control', 'KeyV'); await sleep(1500);
  const echoed = logs.slice(mark).some(l => /User initiated copy/.test(l));
  row('no echo back to the session', !echoed, echoed ? 'client re-advertised' : '');

  crun('cset'); await sleep(2000);
  await key('Control', 'KeyV'); await sleep(1500);
  await key('Control', 'KeyV'); await sleep(1500);
  d = dump(); row('in-session file copy survives 2 pastes', /uri-list|nemo/.test(d), d);
  console.log('--- log ---\n' + logs.join('\n'));
  await b.close();
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
