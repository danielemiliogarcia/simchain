'use strict';

const http = require('http');
const { createRequire } = require('module');

const backendRequire = createRequire('/backend/package/package.json');
const mysql = backendRequire('mysql2/promise');

const retryDelayMs = numberEnv('SIMCHAIN_MEMPOOL_POOL_SEED_RETRY_MS', 1000);
const timeoutMs = numberEnv('SIMCHAIN_MEMPOOL_POOL_SEED_TIMEOUT_MS', 180000);

function env(key, fallback) {
  const value = process.env[key];
  return value && value.trim() ? value.trim() : fallback;
}

function numberEnv(key, fallback) {
  const value = env(key, '');
  if (!value) {
    return fallback;
  }
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    throw new Error(`${key} must be a positive number`);
  }
  return parsed;
}

function nodeConfig(prefix, fallbackHost, fallbackWallet) {
  const wallet = env(`${prefix}_WALLET_NAME`, fallbackWallet);
  return {
    name: `Simchain ${prefix === 'NODE2' ? 'Node 2' : 'Node 3'}`,
    slug: `simchain-${prefix === 'NODE2' ? 'node2' : 'node3'}`,
    uniqueId: prefix === 'NODE2' ? 900002 : 900003,
    host: env(`${prefix}_RPC_HOST`, fallbackHost),
    port: numberEnv(`${prefix}_RPC_PORT`, numberEnv('CORE_RPC_PORT', 18443)),
    wallet,
    label: env(`${prefix}_MINING_LABEL`, `simchain-miner-${wallet}`),
  };
}

function rpc(node, method, params = []) {
  const user = env('CORE_RPC_USERNAME', 'foo');
  const pass = env('CORE_RPC_PASSWORD', 'rpcpassword');
  const body = JSON.stringify({
    jsonrpc: '1.0',
    id: 'simchain-mempool-pool-seeder',
    method,
    params,
  });
  const auth = Buffer.from(`${user}:${pass}`).toString('base64');
  const path = `/wallet/${encodeURIComponent(node.wallet)}`;
  return new Promise((resolve, reject) => {
    const req = http.request({
      host: node.host,
      port: node.port,
      path,
      method: 'POST',
      headers: {
        Authorization: `Basic ${auth}`,
        'Content-Type': 'application/json',
        'Content-Length': Buffer.byteLength(body),
      },
      timeout: 5000,
    }, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        data += chunk;
      });
      res.on('end', () => {
        if (res.statusCode < 200 || res.statusCode >= 300) {
          reject(new Error(`${node.host}/${node.wallet} ${method} HTTP ${res.statusCode}: ${data}`));
          return;
        }
        try {
          const decoded = JSON.parse(data);
          if (decoded.error) {
            reject(new Error(`${node.host}/${node.wallet} ${method}: ${decoded.error.message}`));
            return;
          }
          resolve(decoded.result);
        } catch (error) {
          reject(error);
        }
      });
    });
    req.on('error', reject);
    req.on('timeout', () => {
      req.destroy(new Error(`${node.host}/${node.wallet} ${method} timed out`));
    });
    req.write(body);
    req.end();
  });
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitForMiningAddress(node) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const addresses = await rpc(node, 'getaddressesbylabel', [node.label]);
      const address = Object.keys(addresses || {}).sort()[0];
      if (address) {
        return address;
      }
      lastError = new Error(`label ${node.label} exists but has no addresses`);
    } catch (error) {
      lastError = error;
    }
    await sleep(retryDelayMs);
  }
  throw new Error(`timed out waiting for ${node.name} mining label ${node.label}: ${lastError ? lastError.message : 'no response'}`);
}

async function upsertPool(connection, node, address) {
  const addresses = JSON.stringify([address]);
  const regexes = '[]';
  const link = 'https://github.com/danielemiliogarcia/simchain';
  const [existing] = await connection.execute(
    'SELECT id FROM pools WHERE unique_id = ? OR slug = ? LIMIT 1',
    [node.uniqueId, node.slug],
  );
  let poolId;
  if (existing.length) {
    poolId = existing[0].id;
    await connection.execute(
      'UPDATE pools SET name = ?, link = ?, addresses = ?, regexes = ?, slug = ?, unique_id = ? WHERE id = ?',
      [node.name, link, addresses, regexes, node.slug, node.uniqueId, poolId],
    );
  } else {
    const [result] = await connection.execute(
      'INSERT INTO pools(name, link, addresses, regexes, slug, unique_id) VALUES (?, ?, ?, ?, ?, ?)',
      [node.name, link, addresses, regexes, node.slug, node.uniqueId],
    );
    poolId = result.insertId;
  }

  await connection.execute(
    'UPDATE blocks SET pool_id = ? WHERE coinbase_address = ?',
    [poolId, address],
  );
  return poolId;
}

async function waitForPoolTables(connection) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const [pools] = await connection.execute('SELECT COUNT(*) AS count FROM pools');
      await connection.execute('SELECT COUNT(*) AS count FROM blocks');
      if (Number(pools[0].count) > 1) {
        return;
      }
      lastError = new Error('mempool pool import has not completed yet');
    } catch (error) {
      lastError = error;
    }
    await sleep(retryDelayMs);
  }
  throw new Error(`timed out waiting for mempool database tables: ${lastError ? lastError.message : 'no response'}`);
}

async function main() {
  const nodes = [
    nodeConfig('NODE2', 'btc-simnet-node2', 'node2'),
    nodeConfig('NODE3', 'btc-simnet-node3', 'node3'),
  ];
  const resolved = [];
  for (const node of nodes) {
    const address = await waitForMiningAddress(node);
    resolved.push({ node, address });
    console.log(`[simchain-pools] ${node.name}: ${address} (${node.label})`);
  }

  const connection = await mysql.createConnection({
    host: env('DATABASE_HOST', 'mempool-db'),
    port: numberEnv('DATABASE_PORT', 3306),
    database: env('DATABASE_DATABASE', 'mempool'),
    user: env('DATABASE_USERNAME', 'mempool'),
    password: env('DATABASE_PASSWORD', 'mempool'),
    connectTimeout: 30000,
  });
  try {
    await waitForPoolTables(connection);
    for (const { node, address } of resolved) {
      const poolId = await upsertPool(connection, node, address);
      console.log(`[simchain-pools] seeded pool ${poolId}: ${node.name}`);
    }
  } finally {
    await connection.end();
  }
}

main().catch((error) => {
  console.error(`[simchain-pools] ${error.stack || error.message || error}`);
  process.exit(1);
});
