// Clipboard matrix. Measures the REMOTE session clipboard before and after a
// real Ctrl+V in the browser, so the override is observed directly rather than
// inferred from what Explorer does or doesn't paste.
//
//   node clip.js <url> <user> <pass> <label> <hostClip: empty|text|file>
//
// Run from C:\Users\amart\ironbridge-vm (vmkey lives there).
const puppeteer = require('puppeteer-core');
const { execSync } = require('child_process');
const CHROME = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const [url, user, pass, label, hostClip] = process.argv.slice(2);
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

const PLATFORM = process.argv[8] || 'win';   // win | linux
const SSH = 'ssh -i C:/Users/amart/ironbridge-vm/vmkey -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR admin@localhost';
const sh = (c) => { try { return execSync(c, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim(); } catch (e) { return '<err>'; } };
const vmRead = () => PLATFORM === 'linux'
  // No shell redirection here: this string is parsed by cmd.exe, where
  // 2>/dev/null means a literal \dev\null path and fails the whole command.
  ? sh('wsl -d Ubuntu-26.04 -- cat /cliptest/dump.txt')
  : sh(`${SSH} "type C:\\cliptest\\dump.txt 2>nul"`);
const ps = (c) => sh(`powershell -NoProfile -Sta -Command "${c}"`);

// Put the LOCAL (host) clipboard into the state under test. The browser runs on
// this machine, so this really is the clipboard onPaste will read.
function setHostClipboard(mode) {
  if (mode === 'text') ps("Set-Clipboard -Value 'HOST-TEXT-MARKER'");
  else if (mode === 'file') {
    ps("New-Item -ItemType Directory -Force $env:TEMP\\cliphost | Out-Null; Set-Content -Encoding ascii $env:TEMP\\cliphost\\hostfile.txt 'HOST-FILE'; Set-Clipboard -Path $env:TEMP\\cliphost\\hostfile.txt");
  } else {
    ps("Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::Clear()");
  }
}

(async () => {
  setHostClipboard(hostClip);
  const hostNow = ps("$f=@(Get-Clipboard -Format FileDropList); $t=(Get-Clipboard -Format Text); 'files=' + ($f -join ';') + '|text=' + $t");

  const browser = await puppeteer.launch({
    executablePath: CHROME, headless: false, defaultViewport: null,
    args: ['--window-size=1280,860', '--window-position=30,30'],
  });
  const page = (await browser.pages())[0] || (await browser.newPage());
  const logs = [];
  page.on('console', m => logs.push(m.text()));
  page.on('dialog', async d => { try { await d.accept(); } catch (_) {} });

  await page.goto(url, { waitUntil: 'networkidle2' });
  await page.waitForSelector('#username');
  await page.evaluate(() => { document.getElementById('fullscreen-preconnect').checked = false; });
  await page.type('#username', user);
  await page.type('#password', pass);
  await page.click('#connect-btn');
  try {
    await page.waitForFunction(() => {
      const c = document.getElementById('rdp-canvas');
      return c && c.width > 0 && !c.closest('.canvas-container').hidden;
    }, { timeout: 60000 });
  } catch (e) {
    const err = await page.evaluate(() => {
      const el = document.getElementById('login-error');
      return el ? el.textContent : '';
    });
    console.log(`CONNECT FAILED. login-error="${err}"`);
    console.log(logs.slice(-25).join('\n'));
    await browser.close();
    process.exit(1);
  }
  await sleep(9000);

  // Run a staged helper inside the RDP session via the Run dialog. Bare names
  // only -- they are in C:\Windows, so no ':' or '\' needs typing.
  const runInVm = async (name) => {
    // On Linux the session clipboard is just an X selection, so we can drive it
    // straight from outside via DISPLAY -- no run dialog needed. (On Windows
    // the clipboard is per-window-station, so SSH cannot reach the RDP
    // session's one and the Run dialog is the only way in.)
    if (PLATFORM === 'linux') {
      sh(`wsl -d Ubuntu-26.04 -u root -- /usr/local/bin/crun ${name}`);
      await sleep(1200);
      return;
    }
    {
      await page.keyboard.down('MetaLeft');
      await page.keyboard.press('KeyR');
      await page.keyboard.up('MetaLeft');
    }
    await sleep(1800);
    await page.keyboard.type(name, { delay: 110 });
    // The Run dialog autocompletes from history and leaves the suffix
    // selected; Delete removes it so Enter runs the name we typed.
    await page.keyboard.press('Delete');
    await sleep(400);
    await page.keyboard.press('Enter');
    await sleep(3200);
  };

  // mode=internal: the user copied inside the session first (the bug case).
  // mode=external: nothing copied in-session, so the host clipboard must win.
  const mode = process.argv[7] || 'internal';
  await runInVm('creset');
  if (mode === 'internal') {
    // Remote-side copy: a FILE on the session clipboard, as Explorer does.
    await runInVm('cset');
  }
  await runInVm('cdump');
  const before = vmRead();

  // The action under test: a real Ctrl+V in the browser.
  const mark = logs.length;
  await page.keyboard.down('Control');
  await page.keyboard.press('KeyV');
  await page.keyboard.up('Control');
  await sleep(3000);

  await runInVm('cdump');
  const after = vmRead();

  // What a user would actually see (Windows only: cpaste is a .bat).
  let pasted = '';
  if (PLATFORM === 'win') {
    await runInVm('cpaste');
    pasted = sh(`${SSH} "type C:\\cliptest\\pasted.txt 2>nul"`);
  }

  const advertised = logs.slice(mark).some(l => /sending format list|format list/i.test(l));
  const fileOf = (s) => (String(s).match(/files=([^|]*)/) || [, ''])[1];
  // Windows reports it under files=; the Linux dump lists X selection targets
  // and the uri-list, so just look for the payload anywhere in the dump.
  const keptFile = /payload\.txt/i.test(after);
  // External→internal: the host's content must actually reach the remote.
  // A file sent over CLIPRDR arrives as a promise: FileGroupDescriptorW is
  // advertised, but no filesystem path exists until the paste target pulls
  // FileContents. So the format list, not FileDropList, is the oracle here.
  const gotHost = /HOST-TEXT-MARKER/.test(after) || /FileGroupDescriptorW/i.test(after);
  const verdictOk = mode === 'internal' ? keptFile : gotHost;

  console.log(`\n=== ${label}  (host clipboard: ${hostClip}) ===`);
  console.log(`  host clip      ${hostNow.replace(/\s+/g, ' ').slice(0, 90)}`);
  console.log(`  remote BEFORE  ${before}`);
  console.log(`  remote AFTER   ${after}`);
  console.log(`  pasted into dst: ${pasted || '(nothing)'}`);
  console.log(`  client advertised a Format List on Ctrl+V: ${advertised ? 'YES' : 'no'}`);
  const why = mode === 'internal'
    ? (keptFile ? 'internal copy survived' : 'remote clipboard was overwritten')
    : (gotHost ? 'host clipboard reached the remote' : 'host clipboard did NOT reach the remote');
  console.log(`  mode: ${mode}`);
  console.log(`  VERDICT: ${verdictOk ? 'OK' : 'BROKEN'} - ${why}`);

  require('fs').writeFileSync(`clip-${label}-${hostClip}.json`,
    JSON.stringify({ label, hostClip, hostNow, before, after, pasted, advertised, keptFile }, null, 2));
  console.log('\n' + logs.filter(l => /clip|format|cliprdr/i.test(l)).slice(-8).join('\n'));

  await browser.close();
})().catch(e => { console.error('ERR', e.stack); process.exit(1); });
