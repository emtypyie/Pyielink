#!/usr/bin/env node

const readline = require('readline');
const fs = require('fs');
const path = require('path');
const { execSync, spawn } = require('child_process');
const chalk = require('chalk');
const getCommand = require('./commands/get');
const auth = require('./commands/auth');
const publish = require('./commands/publish');
const bakafetch = require('./commands/bakafetch');
const fetch = require('./commands/fetch');
const runtime = require('./commands/install-runtime');

const t = require('./commands/theme');

const ANIM_LINES = [
  '[  OK  ] Booting emtypyie-core v3.0.2 (Baking Bread)...',
  '[  OK  ] Platform: x86_64-windows-nt detected',
  '[  OK  ] Memory: 2.1 GiB arena allocated / 16 GiB available',
  '[  OK  ] CPU: AVX2, FMA, SSE4.2, POPCNT, BMI1/2 online',
  '[  OK  ] Storage: NTFS 3.1 mounted on C:\\ (1.8 TB free)',
  '[  OK  ] Console: VT100 sequences enabled',
  '[  OK  ] Modules: shell, auth, fetch, runtime loaded',
  '[  OK  ] Commands: 23 built-in commands registered',
  '[  OK  ] Emtypyie CLI v3.0.2 ready -- type /help',
  '[  OK  ] Welcome back. The witches are wandering.',
];

const ANIM_TARGET_COUNT = 10;

const SEG_BOUNDS = [40, 90, 130, 180, 240, 300];
const SEG_INTERVAL = [6, 14, 3, 7, 14, 2];

const PAUSE_MAIN_LINE = 100;
const PAUSE_MAIN_MS = 500;
const PAUSE_SMALL_LINE = 150;
const PAUSE_SMALL_MS = 150;

const sleep = (ms) => new Promise(r => setTimeout(r, ms));

async function playStartupAnimation(options = {}) {
  const { skip = false } = options;
  if (skip || process.env.EMTYPYIE_NO_ANIM) return;

  const termWidth = process.stdout.columns || 80;
  const targetWidth = Math.max(40, Math.min(termWidth, Math.floor(termWidth * 0.65)));

  for (let i = 0; i < ANIM_TARGET_COUNT; i++) {
    let seg = 0;
    while (seg < 5 && i >= SEG_BOUNDS[seg]) seg++;
    const intervalMs = SEG_INTERVAL[seg];
    const freq = 760 + seg * 50;

    const line = ANIM_LINES[i % ANIM_LINES.length];
    const padded = line.length >= targetWidth
      ? line.slice(0, targetWidth)
      : line + ' '.repeat(targetWidth - line.length);
    process.stdout.write(padded + '\n');

    if (intervalMs > 0) await sleep(intervalMs);

    if (i === PAUSE_MAIN_LINE) {
      process.stdout.write(`  [ ... ] Downloading manifests...\r`);
      await sleep(PAUSE_MAIN_MS);
      process.stdout.write(`  [  OK  ] Downloaded manifests (12.4MB)\n`);
    } else if (i === PAUSE_SMALL_LINE) {
      process.stdout.write(`  [ ... ] \r`);
      await sleep(PAUSE_SMALL_MS);
      process.stdout.write(`  [  OK  ] \n`);
    }
  }
}

const BANNER = `
___________        __                         .__                  .__  .__ 
\\_   _____/ ______/  |_ ___.__. ______ ___.__.|__| ____       ____ |  | |__|
 |    __)_ /     \\   __<   |  | \\____ <   |  ||  |/ __ \\    _/ ___\\|  | |  |
 |        \\  Y Y  \\  |  \\___  | |  |_> >___  ||  \\  ___/    \\  \\___|  |_|  |
/_______  /__|_|  /__|  / ____| |   __// ____||__|\\___  > /\\ \\___  >____/__|
        \\/      \\/      \\/      |__|   \\/             \\/  \\/     \\/ 
`;

function openBrowser(url) {
  try {
    execSync(`start ${url}`, { stdio: 'ignore' });
    return true;
  } catch {
    try {
      execSync(`explorer ${url}`, { stdio: 'ignore' });
      return true;
    } catch {
      return false;
    }
  }
}

async function handleCommand(cmd, arg, rl) {
  const normalized = cmd.replace(/^\//, '').toLowerCase();

  switch (normalized) {
    case 'help':
      await showHelp();
      break;

    case 'about':
      showAbout();
      break;

    case 'wiki':
      console.log(t.retro('  Opening wiki.emtypyie.in...'));
      if (openBrowser('https://wiki.emtypyie.in')) {
        console.log(t.retroDim('  Browser opened.'));
      } else {
        console.log(t.retroDim('  Visit: https://wiki.emtypyie.in'));
      }
      break;

    case 'info':
      await showProjectInfo(arg);
      break;

    case 'get':
      await doGet(arg);
      break;

    case 'flash':
      await doFlash(arg);
      break;

    case 'rm':
      await doRemove(arg);
      break;

    case 'issue':
      await doIssue(arg);
      break;

    case 'bakafetch':
    case 'bf':
      bakafetch.show();
      break;

    case 'wrap':
      try {
        await publish.doWrap(rl, arg);
      } catch (e) {
        console.log(t.retroErr(`  Wrap failed: ${e.message}`));
      }
      break;

    case 'setenv':
      await auth.setenv(rl);
      break;

    case 'theme':
      doTheme(arg);
      break;

    case 'clear':
      console.clear();
      break;

    case 'update':
      doUpdate();
      break;

    case 'list':
      await doList();
      break;

    case 'docs':
      await doDocs(arg);
      break;

    case 'changelog':
      doChangelog();
      break;

    case 'exit':
    case 'quit':
      console.log(t.retroDim('\n  System halted.'));
      if (rl) rl.close();
      process.exit(0);
      break;

    default:
      await runProject(normalized);
  }
}

async function showHelp() {
  console.log();
  console.log(t.retroDim('  \u2503 Commands \u2503'));
  console.log();
  console.log(t.retro('  /get <project>') + t.retroDim('     install a project'));
  console.log(t.retro('  /get gcc') + t.retroDim('           auto-install GCC/G++ compiler'));
  console.log(t.retro('  /flash <project>') + t.retroDim('   re-download latest version'));
  console.log(t.retro('  /info <project>') + t.retroDim('    show project details'));
  console.log(t.retro('  /rm <project>') + t.retroDim('      delete project files'));
  console.log(t.retro('  /issue <project>') + t.retroDim('   open issue tracker'));
  console.log(t.retro('  /issue <project> -m') + t.retroDim('  file a bug report'));
  console.log(t.retro('  /setenv') + t.retroDim('            set env variables (tokens, creds)'));
  console.log(t.retro('  /theme <name>') + t.retroDim('       change CLI color theme'));
  console.log(t.retro('  /theme bakafetch <c>') + t.retroDim(' change bakafetch color'));
  console.log(t.retro('  /bakafetch') + t.retroDim('         system info with style'));
  console.log(t.retro('  /bf') + t.retroDim('               shortcut for /bakafetch'));
  console.log(t.retro('  /wrap <dir>') + t.retroDim('        stage files in directory'));
  console.log(t.retro('  /wrap <dir> --commit -m') + t.retroDim('  stage, commit & push'));
  console.log(t.retro('  /wrap all') + t.retroDim('          stage all subdirectories'));
  console.log(t.retro('  /wrap npm publish') + t.retroDim('   auto-bump & npm publish'));
  console.log(t.retro('  /wrap repo "name"') + t.retroDim('   create/PR on GitHub'));
  console.log(t.retro('  /clear') + t.retroDim('              clear screen'));
  console.log(t.retro('  /update') + t.retroDim('            update emtypyie'));
  console.log(t.retro('  /list') + t.retroDim('              list projects'));
  console.log(t.retro('  /docs <project>') + t.retroDim('    open project docs'));
  console.log(t.retro('  /changelog') + t.retroDim('         what\'s new'));
  console.log(t.retro('  /about') + t.retroDim('             about emtypyie'));
  console.log(t.retro('  /wiki') + t.retroDim('              open wiki.emtypyie.in'));
  console.log(t.retro('  /help') + t.retroDim('             this screen'));
  console.log(t.retro('  /exit') + t.retroDim('             quit'));
  console.log();
  console.log(t.retroDim('  \u2503 Projects \u2503'));
  console.log();
  try {
    const list = await fetch.fetchProjectList();
    const projList = Array.isArray(list) ? list : (list.projects || []);
    for (const entry of projList) {
      const name = typeof entry === 'string' ? entry : entry.name;
      const desc = entry.description || '';
      const pad = ' '.repeat(Math.max(0, 12 - name.length));
      console.log(`  ${t.retroAccent(name)}${pad}${t.retroDim(desc)}`);
    }
  } catch {
    console.log(t.retroDim('  (Could not fetch project list)'));
  }
  console.log();
}

function getReleaseDate() {
  try {
    return execSync('git log -1 --format=%ad --date=short', { encoding: 'utf8', timeout: 5000, stdio: ['pipe', 'pipe', 'ignore'] }).toString().trim();
  } catch {
    const pkg = require('./package.json');
    return pkg.releaseDate || 'unknown';
  }
}

function generateLicense() {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  let result = '';
  for (let i = 0; i < 16; i++) {
    result += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return result;
}

const WITCHES_ART = [
  '⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀',
  '⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀',
  '⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣔⣶⣎⠉⠉⠓⠦⡀⠀⠀⠀',
  '⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⠴⠋⠁⠀⠀⠀⠙⠻⢆⠀⠀⠀⣹⡄⠀',
  '⠀⠀⠀⠀⠀⠀⠀⢀⠴⠿⣤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⡇⠀',
  '⠀⠀⠀⠀⠀⡠⠊⠀⠀⠀⠀⠙⢷⡄⠀⠀⠀⠀⠀⠀⠀⢀⢾⠟⠀⠀',
  '⠀⠀⠀⠀⡜⠀⠀⠀⠀⠀⠀⠀⠀⠀⠃⠀⠀⠀⠀⡀⣔⣿⠗⠀⠀⠀',
  '⠀⠀⠀⡮⣝⢦⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡸⣷⡟⠁⠀⠀⠀⠀',
  '⠀⠀⡞⠀⠀⠁⠙⠀⠀⠀⠀⠀⠀⠀⠀⣠⢀⡯⡞⠁⠀⠀⠀⠀⠀⠀',
  '⠀⢸⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⢔⡱⡽⢍⠀⠀⠀⠀⠀⠀⠀⠀⠀',
  '⠀⢸⡀⠀⠀⠀⠀⡠⡠⠦⢧⢯⣫⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀',
  '⠀⠀⠙⢷⣧⠭⢝⡻⣿⠿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀',
  '⠀⠀⠀⠀⠀⠉⠉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀ ',
];

function showAbout() {
  console.log();
  const pkg = require('./package.json');
  console.log(t.retro(`  EMTYPYIE CLI v${pkg.version}`));
  console.log(t.retroDim('  "Baking Bread"'));
  console.log();
  for (const line of WITCHES_ART) console.log(t.retroAccent(line));
  console.log();
  console.log(t.retroDim('  Release: ') + t.retroAccent('Baking Bread'));
  console.log(t.retroDim('  DESIGNED AND ENGINEERED BY  EMTYPYIE'));
  console.log(t.retroDim(`  Copyright \u00a9 ${new Date().getFullYear()} EMTYPYIE. All rights reserved.`));
  console.log();
}

async function showProjectInfo(name) {
  if (!name) {
    console.log(t.retroErr('  Specify a project: /info <project>'));
    return;
  }
  let proj;
  try {
    proj = await fetch.fetchProject(name.toLowerCase());
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }
  console.log();
  console.log(t.retroDim('  \u2503 ') + t.retroAccent(proj.name || name) + t.retroDim(' \u2503'));
  console.log(t.retroDim('  Description: ') + t.retro(proj.description || 'N/A'));
  console.log(t.retroDim('  Version:     ') + t.retro(proj.version || 'latest'));
  console.log(t.retroDim('  Repo:        ') + t.retroAccent(proj.repo || 'N/A'));
  console.log(t.retroDim('  Download:    ') + t.retroDim(proj.download || 'N/A'));
  if (proj.info) {
    console.log();
    console.log(proj.info.trim().split('\n').map(l => `  ${l}`).join('\n'));
  }
  console.log();
}

async function doGet(name) {
  if (!name) {
    console.log(t.retroErr('  Specify a project: /get <project>'));
    return;
  }
  const lower = name.toLowerCase();
  if (lower === 'gcc' || lower === 'g++') {
    await runtime.installCompiler(lower);
    return;
  }
  let proj;
  try {
    proj = await fetch.fetchProject(lower);
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }
  await getCommand.install(lower, proj);
  installDeps(t.getDevDir(lower));
}

async function doFlash(name) {
  if (!name) {
    console.log(t.retroErr('  Specify a project: /flash <project>'));
    return;
  }
  let proj;
  try {
    proj = await fetch.fetchProject(name.toLowerCase());
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }
  console.log();
  console.log(t.retroWarn('  Re-downloading ') + t.retroAccent(proj.name || name) + t.retroWarn('...'));
  console.log();
  const dir = t.getDevDir(name.toLowerCase());
  if (fs.existsSync(dir)) {
    fs.rmSync(dir, { recursive: true, force: true });
    console.log(t.retroDim('  Removed old files.'));
  }
  await getCommand.install(name.toLowerCase(), proj);
  installDeps(t.getDevDir(name.toLowerCase()));
}

async function doRemove(name) {
  if (!name) {
    console.log(t.retroErr('  Specify a project: /rm <project>'));
    return;
  }
  try {
    await fetch.fetchProject(name.toLowerCase());
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }
  const dir = t.getDevDir(name.toLowerCase());
  if (fs.existsSync(dir)) {
    fs.rmSync(dir, { recursive: true, force: true });
    console.log(t.retro('  Removed ') + t.retroDim(dir));
  } else {
    console.log(t.retroDim('  No local files found for ') + t.retroAccent(name));
  }
}

async function doIssue(arg) {
  let name, message;
  const msgMatch = arg.match(/^(\S+)\s+-m\s+"(.+?)"/);
  if (msgMatch) {
    name = msgMatch[1].toLowerCase();
    message = msgMatch[2];
  } else {
    name = arg.trim().toLowerCase();
    message = '';
  }

  if (!name) {
    console.log(t.retroErr('  Usage: /issue <project> -m "your message"'));
    return;
  }

  let proj;
  try {
    proj = await fetch.fetchProject(name);
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }

  const repoPath = proj.repo || `emtypyie/${name}`;
  let url;
  if (message) {
    url = `https://github.com/${repoPath}/issues/new?title=${encodeURIComponent(message)}`;
  } else {
    url = `https://github.com/${repoPath}/issues`;
  }

  console.log(t.retro(`  Opening issue tracker for ${t.retroAccent(proj.name || name)}...`));
  if (openBrowser(url)) {
    console.log(t.retroDim('  Browser opened.'));
  } else {
    console.log(t.retroDim(`  Visit: ${url}`));
  }
}

function installDeps(dir) {
  const reqPath = path.join(dir, 'requirements.txt');
  if (!fs.existsSync(reqPath)) return;
  console.log(t.retroDim('  Installing Python dependencies...'));
  try {
    const launcher = process.platform === 'win32' ? 'py -3' : 'python3';
    execSync(`${launcher} -m pip install -r "${reqPath}" -q`, { stdio: 'pipe', timeout: 120000 });
    console.log(t.retro('  Dependencies installed.'));
  } catch {
    console.log(t.retroWarn('  pip install had issues (see above).'));
  }
}

function launch(launchPath) {
  const isBat = /\.bat$|\.cmd$/i.test(launchPath);
  let child;
  if (isBat) {
    child = spawn(process.env.COMSPEC || 'cmd.exe', ['/c', launchPath], { detached: true, stdio: 'ignore' });
  } else {
    child = spawn(launchPath, [], { detached: true, stdio: 'ignore' });
  }
  child.on('error', (err) => {
    console.log(t.retroErr(`  Failed to launch: ${err.message}`));
  });
  child.unref();
}

function isScript(scriptPath) {
  return /\.(bat|cmd|py|sh)$/i.test(scriptPath);
}

function runSync(syncPath) {
  const isBat = /\.bat$|\.cmd$/i.test(syncPath);
  if (isBat) {
    const cmd = `${process.env.COMSPEC || 'cmd.exe'} /c "${syncPath}"`;
    execSync(cmd, { stdio: 'inherit', timeout: 0 });
  } else {
    execSync(`"${syncPath}"`, { stdio: 'inherit', timeout: 0 });
  }
}

async function runProject(name) {
  let proj;
  try {
    proj = await fetch.fetchProject(name);
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }
  const runTarget = proj.run || proj.filename;
  if (!runTarget) {
    console.log(t.retroErr(`  Project "${name}" has no run target.`));
    return;
  }
  const runPath = path.resolve(t.getDevDir(name), runTarget);
  if (!fs.existsSync(runPath)) {
    console.log(t.retroErr(`  "${name}" not downloaded yet. Run /get ${name} first.`));
    return;
  }
  installDeps(t.getDevDir(name));

  if (isScript(runTarget)) {
    console.log(t.retro(`  Running ${t.retroAccent(proj.name || name)}...`));
    try { runSync(runPath); } catch (e) {
      console.log(t.retroErr(`  Project exited with error: ${e.message}`));
    }
  } else {
    console.log(t.retro(`  Launching ${t.retroAccent(proj.name || name)}...`));
    launch(runPath);
  }
}

function doUpdate() {
  console.log(t.retro('  Updating emtypyie...'));
  try {
    execSync('npm update -g emtypyie-cli', { stdio: 'inherit' });
    console.log(t.retro('  Update complete!'));
  } catch {
    console.log(t.retroErr('  Update failed. Try: npm update -g emtypyie-cli'));
  }
}

async function doList() {
  console.log();
  console.log(t.retroDim('  \u2503 Projects \u2503'));
  console.log();
  try {
    const list = await fetch.fetchProjectList();
    const projList = Array.isArray(list) ? list : (list.projects || []);
    for (const entry of projList) {
      const name = typeof entry === 'string' ? entry : entry.name;
      const ver = entry.version || '?';
      const desc = entry.description || '';
      const pad = ' '.repeat(Math.max(0, 12 - name.length));
      console.log(`  ${t.retroAccent(name)}${pad}${t.retroDim(ver)}  ${t.retro(desc)}`);
    }
  } catch {
    console.log(t.retroDim('  (Could not fetch project list)'));
  }
  console.log();
}

async function doDocs(arg) {
  if (!arg) {
    console.log(t.retroErr('  Specify a project: /docs <project>'));
    return;
  }
  let proj;
  try {
    proj = await fetch.fetchProject(arg.toLowerCase());
  } catch (err) {
    console.log(t.retroErr(err.message));
    return;
  }
  const repo = proj.repo || `emtypyie/${arg}`;
  const url = `https://github.com/${repo}#readme`;
  console.log(t.retro(`  Opening docs for ${t.retroAccent(proj.name || arg)}...`));
  if (openBrowser(url)) {
    console.log(t.retroDim('  Browser opened.'));
  } else {
    console.log(t.retroDim(`  Visit: ${url}`));
  }
}

function doChangelog() {
  const url = 'https://github.com/emtypyie/emtypyie-cli/releases';
  console.log(t.retro('  Opening changelog...'));
  if (openBrowser(url)) {
    console.log(t.retroDim('  Browser opened.'));
  } else {
    console.log(t.retroDim(`  Visit: ${url}`));
  }
}

function doTheme(arg) {
  const parts = arg.trim().split(/\s+/);
  if (!arg) {
    console.log(t.retroDim('  Usage: ') + t.retro('/theme <name>'));
    console.log(t.retroDim('  Usage: ') + t.retro('/theme bakafetch <color>'));
    console.log(t.retroDim('  Themes: ') + t.retro(Object.keys(t.THEMES).join(', ')));
    console.log(t.retroDim('  Bakafetch colors: ') + t.retro(Object.keys(bakafetch.COLORS).join(', ')) + t.retroDim(' or hex'));
    return;
  }
  if (parts[0].toLowerCase() === 'bakafetch') {
    const color = parts.slice(1).join(' ');
    if (bakafetch.setColor(color)) {
      console.log(t.retro('  Bakafetch color set to ') + chalk.hex(bakafetch.getColor())(color));
    } else {
      console.log(t.retroErr('  Invalid color. Use: ') + t.retro(Object.keys(bakafetch.COLORS).join(', ')) + t.retroErr(' or hex like #ff8800'));
    }
    return;
  }
  const theme = arg.trim().toLowerCase();
  if (t.THEMES[theme]) {
    t.apply(theme);
    bakafetch.setColor(theme);
    console.log(t.retro('  Theme set to ') + t.retroAccent(theme));
  } else {
    console.log(t.retroErr('  Invalid theme. Use: ') + t.retro(Object.keys(t.THEMES).join(', ')));
  }
}

const COMMANDS = ['help', 'about', 'wiki', 'setenv', 'theme', 'info', 'get', 'flash', 'rm', 'issue', 'bakafetch', 'bf', 'wrap', 'clear', 'update', 'list', 'docs', 'changelog', 'exit', 'quit'];

function completer(line) {
  const input = line.replace(/^\//, '');
  const parts = input.split(/\s+/);
  const hits = COMMANDS.filter(c => c.startsWith(parts[0])).map(c => '/' + c + ' ');
  return [hits.length ? hits : [], line];
}

async function interactive() {
  process.title = 'emtypyie cli';
  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
    prompt: t.retro('>> '),
    completer
  });

  console.clear();
  await playStartupAnimation({ skip: args.includes('--no-animation') });
  console.log(BANNER);
  console.log(t.retro('  EMTYPYIE CLI v3.0.2'));
  console.log(t.retroDim('  "Baking Bread"'));
  console.log();
  console.log(t.retroDim(`
  \u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501
  `) + t.retro('"code. create. conquer."') + t.retroDim(`
  \u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501

  Type `) + t.retroAccent('/help') + t.retroDim(` for available commands.
  \u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501`));
  console.log();

  rl.prompt();

  rl.on('line', async (line) => {
    const input = line.trim();
    const parts = input.split(/\s+/);
    const cmd = parts[0]?.toLowerCase() || '';
    const arg = parts.slice(1).join(' ');

    if (!cmd) {
      rl.prompt();
      return;
    }

    try {
      await handleCommand(cmd, arg, rl);
    } catch (e) {
      console.log(t.retroErr(`  Error: ${e.message}`));
    }
    rl.prompt();
  });

  rl.on('close', () => {
    console.log(t.retroDim('\n  System halted.'));
    process.exit(0);
  });
}

async function direct(args) {
  process.title = 'emtypyie cli';
  const cmd = args[0]?.toLowerCase();
  const arg = args.slice(1).join(' ');

  if (!cmd || cmd === 'help' || cmd === '/help') {
    await showHelp();
    return;
  }

  try {
    await handleCommand(cmd, arg, null);
  } catch (e) {
    console.log(t.retroErr(`  Error: ${e.message}`));
  }
}

const args = process.argv.slice(2);

if (args.length === 0) {
  interactive().catch(e => { console.error(t.retroErr(`Fatal: ${e.message}`)); process.exit(1); });
} else {
  direct(args).catch(e => { console.error(t.retroErr(`Fatal: ${e.message}`)); process.exit(1); });
}