const chalk = require('chalk');
const os = require('os');
const fs = require('fs');
const path = require('path');
const bakafetch = require('./bakafetch');

const CONFIG_DIR = path.join(os.homedir(), '.emtypyie');
const CONFIG_PATH = path.join(CONFIG_DIR, 'config.json');

const THEMES = {
  slate: { main: '#e2e8f0', dim: '#64748b', accent: '#38bdf8', warn: '#fbbf24', err: '#f87171' },
  green: { main: '#33ff33', dim: '#1a7a1a', accent: '#66ff66', warn: '#ffaa00', err: '#ff3333' },
  amber: { main: '#ffb000', dim: '#805800', accent: '#ffd700', warn: '#ff6600', err: '#ff2200' },
  violet: { main: '#c084fc', dim: '#7c3aed', accent: '#a78bfa', warn: '#fbbf24', err: '#f87171' },
  cyan:  { main: '#22d3ee', dim: '#0891b2', accent: '#67e8f9', warn: '#fbbf24', err: '#f87171' },
};

function getDevDir(name) {
  const dir = path.join(CONFIG_DIR, 'dev', name);
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
  return dir;
}

const t = {
  retro: null,
  retroDim: null,
  retroAccent: null,
  retroWarn: null,
  retroErr: null,
  current: 'slate',
  THEMES,
  apply,
  getDevDir,
  setBakafetchColor: bakafetch.setColor,
  getBakafetchColor: bakafetch.getColor,
  bakafetchColors: bakafetch.COLORS,
};

function apply(name) {
  const pal = THEMES[name];
  if (!pal) return false;
  t.current = name;
  t.retro = chalk.hex(pal.main);
  t.retroDim = chalk.hex(pal.dim);
  t.retroAccent = chalk.hex(pal.accent);
  t.retroWarn = chalk.hex(pal.warn);
  t.retroErr = chalk.hex(pal.err);
  save();
  return true;
}

function load() {
  try {
    const data = JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf8'));
    if (data.theme && THEMES[data.theme]) apply(data.theme);
  } catch {}
  if (!t.retro) apply('slate');
}

function save() {
  try {
    if (!fs.existsSync(CONFIG_DIR)) fs.mkdirSync(CONFIG_DIR, { recursive: true });
    fs.writeFileSync(CONFIG_PATH, JSON.stringify({ theme: t.current }));
  } catch {}
}

load();

module.exports = t;