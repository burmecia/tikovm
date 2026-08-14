//! Webapp entrypoint: config → hostd client → registry → HTTP server.
//!
//! Serves the SPA (web/dist) plus the `/api/demo` REST surface on PORT
//! (default 4000). On boot it sweeps orphaned demo VMs left by previous
//! runs; a TTL sweeper expires projects; SIGINT/SIGTERM clean up everything.

import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import express from 'express';
import { loadConfig } from './config.js';
import { installShutdownHandler, startTtlSweeper, sweepOrphans } from './cleanup.js';
import { makeClient } from './hostd.js';
import { apiRouter } from './routes.js';
import { Registry } from './state.js';

const here = dirname(fileURLToPath(import.meta.url));

async function main(): Promise<void> {
  const cfg = loadConfig();
  const client = makeClient(cfg);
  const registry = new Registry();

  const app = express();
  app.use(express.json({ limit: '1mb' }));
  app.use('/api/demo', apiRouter({ cfg, registry, client }));

  // Serve the built SPA (web/dist) and fall back to index.html for client
  // routing. In dev, Vite serves the frontend itself and proxies /api here.
  const webDist = join(here, '../../web/dist');
  if (existsSync(webDist)) {
    app.use(express.static(webDist));
    app.use((req, res, next) => {
      if (req.method === 'GET' && !req.path.startsWith('/api')) {
        res.sendFile(join(webDist, 'index.html'));
        return;
      }
      next();
    });
  }

  const server = app.listen(cfg.port, () => {
    console.log(
      `[webapp] listening on http://0.0.0.0:${cfg.port} (hostd ${cfg.hostdUrl}, ` +
        `ttl ${Math.round(cfg.projectTtlMs / 1000)}s)`,
    );
  });

  // Remove demo VMs a previous (crashed/killed) webapp run left behind.
  try {
    const n = await sweepOrphans(cfg, client, registry);
    if (n > 0) {
      console.log(`[webapp] orphan sweep deleted ${n} leftover VM(s)`);
    }
  } catch (err) {
    console.warn('[webapp] orphan sweep failed (is hostd running?):', err);
  }

  startTtlSweeper(registry, cfg, client);
  installShutdownHandler(cfg, client, registry, server);
}

main().catch((err) => {
  console.error('[webapp] fatal:', err);
  process.exit(1);
});
