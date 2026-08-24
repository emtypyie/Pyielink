const fs = require('fs');
const path = require('path');
const os = require('os');
const readline = require('readline');
const { execSync } = require('child_process');
const https = require('https');
const { readEnv } = require('./auth');

function run(cmd, cwd) {
  try {
    return execSync(cmd, { cwd, stdio: 'inherit', encoding: 'utf8', timeout: 120000 });
  } catch (e) {
    throw new Error(`Command failed: ${cmd.slice(0, 80)} — ${e.message}`);
  }
}

function runCapture(cmd, cwd) {
  try {
    return execSync(cmd, { cwd, encoding: 'utf8', timeout: 30000 }).toString().trim();
  } catch (e) {
    throw new Error(`Command failed: ${cmd.slice(0, 80)} — ${e.message}`);
  }
}

function ghRequest(method, endpoint, token, body) {
  return new Promise((resolve, reject) => {
    const opts = {
      hostname: 'api.github.com',
      path: endpoint,
      method,
      headers: {
        'User-Agent': 'emtypyie-cli',
        'Authorization': `token ${token}`,
        'Accept': 'application/vnd.github.v3+json',
        'Content-Type': 'application/json'
      }
    };
    const req = https.request(opts, res => {
      let data = '';
      res.on('data', chunk => data += chunk);
      res.on('end', () => {
        try { resolve({ status: res.statusCode, data: JSON.parse(data) }); }
        catch { resolve({ status: res.statusCode, data }); }
      });
    });
    req.on('error', reject);
    if (body) req.write(JSON.stringify(body));
    req.end();
  });
}

function getRl(rl) {
  if (rl) return rl;
  return readline.createInterface({ input: process.stdin, output: process.stdout });
}

function question(rl, query) {
  const useRl = getRl(rl);
  return new Promise(resolve => useRl.question(`  ${query} `, answer => {
    if (!rl) useRl.close();
    resolve(answer);
  }));
}

async function askYesNo(rl, q) {
  const answer = await question(rl, `${q} (Y/n):`);
  return !answer || answer.toLowerCase() === 'y' || answer.toLowerCase() === 'yes';
}

function gitAdd(cwd) {
  console.log(`  Staging files in ${path.basename(path.resolve(cwd))}...`);
  run('git add .', cwd);
  console.log('  \u2714 Staged.');
}

function gitCommitPush(cwd, msg) {
  const env = readEnv();
  const config = env.GIT_USERNAME && env.GIT_EMAIL
    ? `-c user.name="${env.GIT_USERNAME}" -c user.email="${env.GIT_EMAIL}"`
    : '';
  if (msg) {
    run(`git ${config} commit -m "${msg.replace(/"/g, '\\"')}"`, cwd);
  }
  try {
    const remote = runCapture('git remote get-url origin', cwd);
    if (remote) {
      let pushUrl = remote;
      if (env.GITHUB_TOKEN && remote.includes('github.com')) {
        pushUrl = remote.replace('https://github.com', `https://${env.GITHUB_TOKEN}@github.com`);
      }
      run(`git push "${pushUrl}"`, cwd);
      console.log('  \u2714 Pushed.');
    }
  } catch {
    console.log('  \u26a0 No remote set. Skipping push.');
  }
}

async function doWrapDir(rl, dir, msg) {
  const cwd = path.resolve(dir);
  if (!fs.existsSync(cwd)) {
    console.log(`  Directory "${dir}" not found.`);
    return;
  }
  if (!fs.existsSync(path.join(cwd, '.git'))) {
    const ans = await askYesNo(rl, 'Not a git repo. Initialize one?');
    if (!ans) return;
    run('git init', cwd);
    run('git add .', cwd);
    if (msg) {
      run(`git commit -m "${msg.replace(/"/g, '\\"')}"`, cwd);
    }
  } else {
    gitAdd(cwd);
    if (msg) {
      gitCommitPush(cwd, msg);
    }
  }
  console.log('  \u2714 Done.');
}

async function doWrapAll(rl, msg) {
  const cwd = process.cwd();
  const dirs = fs.readdirSync(cwd, { withFileTypes: true })
    .filter(d => d.isDirectory() && !d.name.startsWith('.') && d.name !== 'node_modules')
    .map(d => d.name);
  if (!dirs.length) {
    console.log('  No subdirectories found.');
    return;
  }
  for (const dir of dirs) {
    console.log(`\n  \u2503 ${dir} \u2503`);
    await doWrapDir(rl, dir, msg);
  }
}

function doNpmPublish() {
  const env = readEnv();
  if (!env.NPM_TOKEN) {
    console.log('  NPM_TOKEN not set. Use /setenv first.');
    return;
  }
  const cwd = process.cwd();
  const pkgPath = path.join(cwd, 'package.json');
  if (!fs.existsSync(pkgPath)) {
    console.log('  No package.json found in current directory.');
    return;
  }
  const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
  const ver = pkg.version.split('.').map(Number);
  ver[2] = (ver[2] || 0) + 1;
  pkg.version = ver.join('.');
  fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n');
  console.log(`  Version bumped to ${pkg.version}`);
  const npmrcPath = path.join(os.homedir(), '.npmrc');
  const back = fs.existsSync(npmrcPath) ? fs.readFileSync(npmrcPath, 'utf8') : null;
  fs.writeFileSync(npmrcPath, `//registry.npmjs.org/:_authToken=${env.NPM_TOKEN}\n`);
  try {
    run('npm publish', cwd);
    console.log('  \u2714 Published to npm.');
  } catch (e) {
    console.log(`  \u2717 npm publish failed: ${e.message}`);
  }
  if (back) {
    fs.writeFileSync(npmrcPath, back);
  } else {
    try { fs.unlinkSync(npmrcPath); } catch {}
  }
}

async function doRepo(rl, name) {
  if (!name) {
    console.log('  Usage: /wrap repo "name"');
    return;
  }
  const env = readEnv();
  if (!env.GITHUB_TOKEN) {
    console.log('  GITHUB_TOKEN not set. Use /setenv first.');
    return;
  }
  if (!env.GIT_USERNAME || !env.GIT_EMAIL) {
    console.log('  GIT_USERNAME and GIT_EMAIL must be set. Use /setenv first.');
    return;
  }
  const res = await ghRequest('GET', `/repos/emtypyie/${name}`, env.GITHUB_TOKEN);
  if (res.status === 404) {
    console.log(`  Repo "${name}" not found on GitHub.`);
    const ans = await askYesNo(rl, 'Create it?');
    if (!ans) return;
    const createRes = await ghRequest('POST', '/user/repos', env.GITHUB_TOKEN, { name, private: false, auto_init: false });
    if (createRes.status === 201) {
      console.log(`  \u2714 Repo "${name}" created.`);
      const cwd = process.cwd();
      if (fs.existsSync(path.join(cwd, '.git'))) {
        run(`git remote add origin https://${env.GITHUB_TOKEN}@github.com/emtypyie/${name}.git`, cwd);
        const msg = await question(rl, 'Initial commit message:');
        gitAdd(cwd);
        run(`git -c user.name="${env.GIT_USERNAME}" -c user.email="${env.GIT_EMAIL}" commit -m "${(msg || 'initial').replace(/"/g, '\\"')}"`, cwd);
        run('git branch -M main', cwd);
        run('git push -u origin main', cwd);
        console.log('  \u2714 Pushed to main.');
      }
    } else {
      console.log(`  \u2717 Failed to create repo: ${JSON.stringify(createRes.data)}`);
    }
  } else if (res.status === 200) {
    console.log(`  Repo "${name}" exists.`);
    const cwd = process.cwd();
    const currentBranch = runCapture('git rev-parse --abbrev-ref HEAD', cwd);
    const branchName = `wrap/${Date.now()}`;
    gitAdd(cwd);
    run(`git checkout -b ${branchName}`, cwd);
    const msg = await question(rl, 'Commit message for PR:');
    gitCommitPush(cwd, msg || 'update');
    run(`git push -u origin ${branchName}`, cwd);
    const prRes = await ghRequest('POST', `/repos/emtypyie/${name}/pulls`, env.GITHUB_TOKEN, {
      title: msg || 'Update',
      head: branchName,
      base: 'main',
      body: 'Automated PR via emtypyie CLI'
    });
    if (prRes.status === 201) {
      const prNum = prRes.data.number;
      console.log(`  \u2714 PR #${prNum} created.`);
      const mergeAns = await askYesNo(rl, 'Merge the PR?');
      if (mergeAns) {
        const mergeRes = await ghRequest('PUT', `/repos/emtypyie/${name}/pulls/${prNum}/merge`, env.GITHUB_TOKEN, {
          merge_method: 'merge'
        });
        if (mergeRes.status === 200) {
          console.log('  \u2714 PR merged successfully.');
        } else {
          console.log(`  \u2717 Merge failed: ${JSON.stringify(mergeRes.data)}`);
        }
      }
      run(`git checkout ${currentBranch}`, cwd);
    } else {
      console.log(`  \u2717 Failed to create PR: ${JSON.stringify(prRes.data)}`);
    }
  } else {
    console.log(`  \u2717 GitHub API error: ${res.status} ${JSON.stringify(res.data)}`);
  }
}

async function doWrap(rl, arg) {
  const parts = arg.trim().split(/\s+/);
  if (!arg) {
    console.log('  Usage:');
    console.log('  /wrap <dir>');
    console.log('  /wrap <dir> --commit -m "message"');
    console.log('  /wrap all');
    console.log('  /wrap all --commit -m "message"');
    console.log('  /wrap npm publish');
    console.log('  /wrap repo "name"');
    return;
  }
  if (parts[0].toLowerCase() === 'npm' && parts[1]?.toLowerCase() === 'publish') {
    return doNpmPublish();
  }
  if (parts[0].toLowerCase() === 'repo') {
    const name = parts.slice(1).join(' ').replace(/^"|"$/g, '');
    return await doRepo(rl, name);
  }
  if (parts[0].toLowerCase() === 'all') {
    const hasCommit = parts.includes('--commit');
    const msgIdx = parts.indexOf('-m');
    const msg = msgIdx > -1 ? parts.slice(msgIdx + 1).join(' ').replace(/^"|"$/g, '') : null;
    return await doWrapAll(rl, hasCommit ? msg : null);
  }
  const dir = parts[0];
  const hasCommit = parts.includes('--commit');
  const msgIdx = parts.indexOf('-m');
  const msg = msgIdx > -1 ? parts.slice(msgIdx + 1).join(' ').replace(/^"|"$/g, '') : null;
  return await doWrapDir(rl, dir, hasCommit ? msg : null);
}

module.exports = { doWrap };