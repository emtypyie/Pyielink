const fs = require('fs');
const path = require('path');
const os = require('os');
const readline = require('readline');

const ENV_PATH = path.join(os.homedir(), '.emtypyie', '.env');

function readEnv() {
  try {
    const content = fs.readFileSync(ENV_PATH, 'utf8');
    const env = {};
    content.split('\n').filter(Boolean).forEach(line => {
      const idx = line.indexOf('=');
      if (idx > 0) env[line.slice(0, idx).trim()] = line.slice(idx + 1).trim();
    });
    return env;
  } catch { return {}; }
}

function writeEnv(env) {
  const dir = path.dirname(ENV_PATH);
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(ENV_PATH, Object.entries(env).map(([k, v]) => `${k}=${v}`).join('\n'), 'utf8');
}

function getRl(rl) {
  if (rl) return rl;
  return readline.createInterface({ input: process.stdin, output: process.stdout });
}

function question(rl, query) {
  const useRl = getRl(rl);
  return new Promise(resolve => useRl.question(query, answer => {
    if (!rl) useRl.close();
    resolve(answer);
  }));
}

async function setenv(rl) {
  console.log();
  console.log('  \u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501');
  console.log('  Set Environment Variables');
  console.log('  \u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501\u2501');
  console.log();
  const current = readEnv();
  const token = await question(rl, `  GITHUB_TOKEN [${current.GITHUB_TOKEN ? '***set***' : 'not set'}]: `);
  const username = await question(rl, `  GIT_USERNAME [${current.GIT_USERNAME || ''}]: `);
  const email = await question(rl, `  GIT_EMAIL [${current.GIT_EMAIL || ''}]: `);
  const npmToken = await question(rl, `  NPM_TOKEN [${current.NPM_TOKEN ? '***set***' : 'not set'}]: `);
  const env = {};
  if (token) env.GITHUB_TOKEN = token;
  else if (current.GITHUB_TOKEN) env.GITHUB_TOKEN = current.GITHUB_TOKEN;
  if (username) env.GIT_USERNAME = username;
  else if (current.GIT_USERNAME) env.GIT_USERNAME = current.GIT_USERNAME;
  if (email) env.GIT_EMAIL = email;
  else if (current.GIT_EMAIL) env.GIT_EMAIL = current.GIT_EMAIL;
  if (npmToken) env.NPM_TOKEN = npmToken;
  else if (current.NPM_TOKEN) env.NPM_TOKEN = current.NPM_TOKEN;
  writeEnv(env);
  console.log();
  console.log('  \u2714 Environment variables saved.');
  console.log();
}

module.exports = { setenv, readEnv, ENV_PATH };