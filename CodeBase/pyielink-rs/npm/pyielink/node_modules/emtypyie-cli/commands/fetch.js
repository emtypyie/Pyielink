const https = require('https');

const API_HOST = 'cdn.emtypyie.in';
const API_BASE = '/dev';
const TIMEOUT = 10000;

function _get(path) {
  return new Promise((resolve, reject) => {
    const opts = {
      hostname: API_HOST,
      path: path,
      method: 'GET',
      headers: { 'User-Agent': 'emtypyie-cli' },
      timeout: TIMEOUT
    };

    const req = https.get(opts, (res) => {
      let data = '';
      res.on('data', chunk => data += chunk);
      res.on('end', () => {
        if (res.statusCode === 404) {
          reject(new Error('Failed to fetch data from cdn.emtypyie.in - use /list to see if it actually exists'));
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error('Cdn network error'));
          return;
        }
        try {
          resolve(JSON.parse(data));
        } catch {
          reject(new Error('Cdn network error'));
        }
      });
    });

    req.on('error', () => {
      req.destroy();
      reject(new Error('Cdn network error'));
    });

    req.on('timeout', () => {
      req.destroy();
      reject(new Error('Cdn network error'));
    });
  });
}

function fetchProjectList() {
  return _get(`${API_BASE}/meta.json`);
}

function fetchProject(name) {
  return _get(`${API_BASE}/${encodeURIComponent(name)}/metadata.json`);
}

module.exports = { fetchProjectList, fetchProject };