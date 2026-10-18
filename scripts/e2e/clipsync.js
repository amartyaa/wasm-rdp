// Clipboard sync on ONE Windows desktop session, judged by what Notepad inside
// the session actually receives (saved to C:\cliptest\paste.txt and read back
// over SSH) and by the session clipboard's format list. Covers every paste
// gesture (Ctrl+V, Shift+Insert, Ctrl+Shift+V, Edit menu), echo suppression and
// in-session copies surviving a paste. Needs the VM helpers in README.md.
//   node clipsync.js <url> <user> <pass> <grant|prompt|deny>
const puppeteer = require('puppeteer-core');
const { execSync } = require('child_process');
const CHROME = 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const [url, user, pass, perm] = process.argv.slice(2);  // perm: grant | deny | prompt
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
const SSH = 'ssh -i C:/Users/amart/ironbridge-vm/vmkey -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR admin@localhost';
const sh = (c) => { try { return execSync(c, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim(); } catch (e) { return '<err>'; } };
const ps = (c) => sh(`powershell -NoProfile -Sta -Command "${c}"`);
const vm = (f) => sh(`${SSH} "type C:\\cliptest\\${f} 2>nul"`).replace(/[^\x20-\x7e|]/g, '');
const hostSet = (t) => ps(`Set-Clipboard -Value '${t}'`);
const hostGet = () => ps('Get-Clipboard -Raw');
const t0 = Date.now();
const ts = () => ((Date.now() - t0) / 1000).toFixed(2).padStart(7);

(async () => {
  const tag = Math.random().toString(36).slice(2, 6).toUpperCase();
  hostSet(`HOST-A-${tag}`);
  const browser = await puppeteer.launch({ executablePath: CHROME, headless: false, defaultViewport: null,
    args: ['--window-size=1280,860', '--window-position=30,30'] });
  const ctx = browser.defaultBrowserContext(), origin = new URL(url).origin;
  if (perm === 'grant') await ctx.overridePermissions(origin, ['clipboard-read', 'clipboard-write', 'clipboard-sanitized-write']);
  const page = (await browser.pages())[0];
  // overridePermissions without clipboard-read still reports it granted; CDP's
  // per-permission call is the one that really denies it.
  if (perm === 'deny') await (await page.target().createCDPSession()).send('Browser.setPermission',
    { origin, permission: { name: 'clipboard-read' }, setting: 'denied' });
  const cdp = await page.target().createCDPSession();
  await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: true });
  await page.evaluateOnNewDocument(() => {
    window.__readTextCalls = 0;
    const c = navigator.clipboard, rt = c && c.readText.bind(c);
    if (rt) c.readText = () => { window.__readTextCalls++; return rt(); };
  });
  const logs = [];
  page.on('console', m => { const t = m.text(); if (/clip|format/i.test(t)) { logs.push(`${ts()} ${t}`); } });
  page.on('dialog', d => d.accept().catch(() => {}));
  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.evaluate(() => { document.getElementById('fullscreen-preconnect').checked = false; });
  await page.type('#username', user); await page.type('#password', pass);
  await page.click('#connect-btn');
  await page.waitForFunction(() => { const c = document.getElementById('rdp-canvas'); return c && c.width > 0 && !c.closest('.canvas-container').hidden; }, { timeout: 60000 });
  await sleep(1500);
  const toastSeen = await page.evaluate(() => { const t = document.getElementById('toast'); return t && !t.hidden ? t.textContent : ''; });
  await sleep(8500);

  const key = async (mod, k) => { await page.keyboard.down(mod); await page.keyboard.press(k); await page.keyboard.up(mod); };
  const run = async (name) => {
    await key('MetaLeft', 'KeyR'); await sleep(1800);
    await page.keyboard.type(name, { delay: 100 }); await page.keyboard.press('Delete'); await sleep(300);
    await page.keyboard.press('Enter'); await sleep(3500);
  };
  const fmts = async () => { await run('cfmt'); return vm('dump.txt'); };
  // Paste into the open Notepad and return what it saved.
  const gesture = async (g) => {
    if (g === 'shiftins') return key('Shift', 'Insert');
    if (g === 'ctrlshiftv') { await page.keyboard.down('Control'); await page.keyboard.down('Shift'); await page.keyboard.press('KeyV'); await page.keyboard.up('Shift'); return page.keyboard.up('Control'); }
    if (g === 'menu') { await page.keyboard.press('Alt'); await sleep(400); await page.keyboard.press('KeyE'); await sleep(400); return page.keyboard.press('KeyP'); }
    return key('Control', 'KeyV');
  };
  const pasteNotepad = async (g) => {
    await key('Control', 'KeyA'); await page.keyboard.press('Delete'); await sleep(300);
    const mark = logs.length;
    await gesture(g); await sleep(1200);
    await key('Control', 'KeyS'); await sleep(1500);
    return { got: vm('paste.txt'), remoteMsgs: logs.slice(mark).filter(l => /remote format list/.test(l)).length };
  };
  const row = (n, ok, d) => console.log(`${ok ? 'OK    ' : 'BROKEN'} ${n.padEnd(34)} ${d}`);

  await run('npad'); await sleep(1500);

  // T1: host clipboard set before connect, first Ctrl+V of the session.
  let r = await pasteNotepad();
  row('T1 first paste after connect', r.got === `HOST-A-${tag}`, `want HOST-A-${tag} got "${r.got}"`);
  r = await pasteNotepad();
  row('T1b second press', r.got === `HOST-A-${tag}`, `got "${r.got}"`);

  // T5: host clipboard changes while the tab keeps focus (copy in another
  // monitor's app / clipboard manager), then one Ctrl+V each.
  let hits = 0; const N = 10; const misses = [];
  for (let i = 1; i <= N; i++) {
    hostSet(`HOST-R${i}-${tag}`); await sleep(400);
    r = await pasteNotepad();
    if (r.got === `HOST-R${i}-${tag}`) hits++; else misses.push(`#${i}:"${r.got}"`);
  }
  row('T5 host change -> 1 press', hits === N, `${hits}/${N} ${misses.join(' ')}`);

  // Other paste gestures, each after a fresh host copy.
  for (const g of ['shiftins', 'ctrlshiftv', 'menu']) {
    const want = `HOST-${g.toUpperCase()}-${tag}`;
    hostSet(want); await sleep(600);
    r = await pasteNotepad(g);
    row(`T7 ${g} pastes new host text`, r.got === want, `got "${r.got}"`);
  }

  // T3: in-session text copy, then paste inside the session. Does the client
  // re-advertise the text it just wrote to the host (echo)?
  await run('csettext'); await sleep(1500);
  const host3 = hostGet();
  row('T3 remote->host text', host3 === 'REMOTE-TEXT-MARKER', `host clipboard "${host3}"`);
  const f3a = await fmts();
  r = await pasteNotepad();
  const f3b = await fmts();
  row('T3 in-session paste keeps remote', f3a === f3b, `\n         before ${f3a}\n         after  ${f3b}`);

  // T4: in-session HTML copy (Word/Outlook/browser-style rich clip), one paste.
  await run('csethtml'); await sleep(1500);
  const f4a = await fmts();
  r = await pasteNotepad();
  const f4b = await fmts();
  row('T4 rich clip survives a paste', /HTML Format/.test(f4b), `\n         before ${f4a}\n         after  ${f4b}`);

  // T6: in-session rich copy (HTML + text, what Word/Outlook/Chrome put up).
  await run('csetrich'); await sleep(1500);
  const f6a = await fmts();
  r = await pasteNotepad();
  const f6b = await fmts();
  row('T6 HTML+text survives a paste', /HTML Format/.test(f6b), `
         before ${f6a}
         after  ${f6b}`);

  // T2: in-session FILE copy, pasted twice (two target folders).
  await run('cset'); await sleep(1500);
  const f2a = await fmts();
  await key('Control', 'KeyV'); await sleep(1500);
  const f2b = await fmts();
  await key('Control', 'KeyV'); await sleep(1500);
  const f2c = await fmts();
  row('T2 file clip after 1st paste', /FileDrop/.test(f2b), f2b);
  row('T2 file clip after 2nd paste', /FileDrop/.test(f2c), f2c);
  console.log(`         before ${f2a}`);

  const toast = await page.evaluate(() => { const t = document.getElementById('toast'); return t && !t.hidden ? t.textContent : ''; });
  console.log(`\nreadText calls: ${await page.evaluate(() => window.__readTextCalls)}   toast seen: ${toastSeen ? '"' + toastSeen.slice(0, 60) + '..."' : '(none)'}`);
  console.log('\n--- clipboard log ---\n' + logs.join('\n'));
  await browser.close();
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
