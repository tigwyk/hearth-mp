#!/usr/bin/env node
// Hearth v0.4 Task 1: tick-driver benchmark client.
//
// Opens one hibernatable WebSocket per (driver, hz, work_us) combination, starts
// that combination's Durable Object, waits for the run, then records:
//   - the server's /stats (ticks, expected, missed, interval error, evicted),
//   - the client-side receive jitter of the 1-byte tick frames,
// into bench/tick-bench/results.json.
//
// Local smoke check (the only local numbers that are meaningful; production
// scheduling and billing come from the deployed matrix):
//   cd bench/tick-bench && npx wrangler dev          # in another shell
//   node run.mjs --url http://127.0.0.1:8787 --hz 20 --secs 60
//
// Deployed matrix (ask the user first, it creates a Worker and costs cents):
//   npx wrangler deploy
//   node run.mjs --url https://<worker>.<account>.workers.dev \
//       --hz 10,20 --secs 600 --work-us 0,2000
//
// With CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID set, it also queries the
// GraphQL Analytics API for the observed request counts over the run window. The
// query is best-effort; verify the dataset fields with GraphQL introspection on
// your account. Without those variables it is skipped and results.json carries
// only the local billing estimate.

import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const defaultUrl = 'http://127.0.0.1:8787';
const drivers = ['alarm', 'loop'];

function parseArgs(argv) {
  const options = { positional: [] };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith('--')) {
      options.positional.push(arg);
      continue;
    }
    const [name, inline] = arg.slice(2).split('=');
    if (inline !== undefined) {
      options[name] = inline;
    } else if (argv[index + 1] !== undefined && !argv[index + 1].startsWith('--')) {
      options[name] = argv[index + 1];
      index += 1;
    } else {
      options[name] = true;
    }
  }
  return options;
}

function parseList(value) {
  return String(value)
    .split(',')
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
}

function round3(value) {
  return Math.round(value * 1000) / 1000;
}

function percentile(values, percent) {
  if (values.length === 0) {
    return 0;
  }
  const sorted = [...values].sort((left, right) => left - right);
  const rank = Math.ceil((percent / 100) * sorted.length);
  return sorted[Math.max(0, rank - 1)];
}

function intervalStats(times, expectedTicks) {
  const intervals = [];
  for (let index = 1; index < times.length; index += 1) {
    intervals.push(times[index] - times[index - 1]);
  }
  const mean = intervals.length === 0 ? 0 : intervals.reduce((sum, value) => sum + value, 0) / intervals.length;
  return {
    received: times.length,
    expected: expectedTicks,
    interval_ms: {
      p50: round3(percentile(intervals, 50)),
      p95: round3(percentile(intervals, 95)),
      p99: round3(percentile(intervals, 99)),
      max: round3(intervals.length === 0 ? 0 : Math.max(...intervals)),
      mean: round3(mean),
    },
    // Arrival-based estimate only: one dropped frame inflates two intervals.
    drop_ratio: expectedTicks <= 1 ? 0 : round3(Math.max(0, 1 - (times.length - 1) / (expectedTicks - 1))),
  };
}

function randomId() {
  return Math.random().toString(36).slice(2, 8);
}

const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

async function openSocket(run, baseUrl) {
  const socketUrl = `${baseUrl.replace(/^http/, 'ws')}/${run.driver}/${run.name}/ws`;
  const socket = new WebSocket(socketUrl);
  socket.binaryType = 'arraybuffer';
  socket.addEventListener('message', () => run.receiveTimes.push(performance.now()));
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', () => reject(new Error(`websocket failed: ${socketUrl}`)), {
      once: true,
    });
  });
  run.socket = socket;
}

async function startRun(run, baseUrl) {
  const query = `hz=${run.hz}&secs=${run.secs}&work_us=${run.work_us}`;
  const response = await fetch(`${baseUrl}/${run.driver}/${run.name}/start?${query}`, {
    method: 'POST',
  });
  if (!response.ok) {
    throw new Error(`start failed for ${run.name}: HTTP ${response.status} ${await response.text()}`);
  }
}

async function collect(run, baseUrl) {
  try {
    const response = await fetch(`${baseUrl}/${run.driver}/${run.name}/stats`);
    if (!response.ok) {
      throw new Error(`stats failed for ${run.name}: HTTP ${response.status} ${await response.text()}`);
    }
    run.server = await response.json();
  } finally {
    try {
      run.socket?.close();
    } catch {
      // The socket is best effort; the stats are the deliverable.
    }
  }
}

/// The local billing estimate, independent of the analytics API: a Durable
/// Object bills requests plus wall-clock duration (GB-s) while it is active.
function billingEstimate(runs, secs) {
  const memoryGb = 0.128; // Hearth's v0.3b cost model: 128 MB per DO.
  const perDriver = {};
  for (const driver of drivers) {
    const driverRuns = runs.filter((run) => run.driver === driver);
    const objects = driverRuns.length;
    const wallSeconds = objects * secs;
    perDriver[driver] = {
      durable_objects: objects,
      wall_seconds: wallSeconds,
      // GB-s while ticking; the hibernation difference between the drivers is
      // the reason this benchmark exists.
      estimated_duration_gb_s: round3(wallSeconds * memoryGb),
      // Ticks are Durable Object requests; add /ws, /start and /stats.
      requests_estimate: driverRuns.reduce(
        (sum, run) => sum + (run.server?.ticks ?? 0) + 3,
        0,
      ),
    };
  }
  return { memory_gb: memoryGb, per_driver: perDriver };
}

async function analyticsSection({ accountId, apiToken, window, scriptName, runs, secs }) {
  const estimate = billingEstimate(runs, secs);
  if (!accountId || !apiToken) {
    return {
      skipped:
        'set CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID to query the GraphQL Analytics API',
      estimate,
    };
  }
  const query = `query TickBenchUsage($accountTag: String!, $since: Time!, $until: Time!, $scriptName: String!) {
  viewer {
    accounts(filter: {accountTag: $accountTag}) {
      durableObjectsInvocationsAdaptiveGroups(
        limit: 1000
        filter: {datetime_geq: $since, datetime_leq: $until, scriptName: $scriptName}
      ) {
        count
        sum { requests }
      }
      durableObjectsPeriodicGroups(
        limit: 1000
        filter: {datetime_geq: $since, datetime_leq: $until, scriptName: $scriptName}
      ) {
        count
        sum { cpuTime }
      }
    }
  }
}`;
  try {
    const response = await fetch('https://api.cloudflare.com/client/v4/graphql', {
      method: 'POST',
      headers: {
        authorization: `Bearer ${apiToken}`,
        'content-type': 'application/json',
      },
      body: JSON.stringify({
        query,
        variables: {
          accountTag: accountId,
          since: window.started_at,
          until: window.ended_at,
          scriptName,
        },
      }),
    });
    const payload = await response.json();
    if (!response.ok || payload.errors) {
      return { estimate, query, error: payload.errors ?? `HTTP ${response.status}` };
    }
    return { estimate, query, data: payload.data?.viewer?.accounts?.[0] ?? null };
  } catch (error) {
    return { estimate, query, error: String(error) };
  }
}

function printSummary(runs) {
  const header = [
    'DRIVER'.padEnd(6),
    'HZ'.padStart(3),
    'WORK_US'.padStart(8),
    'TICKS'.padStart(7),
    'EXPECTED'.padStart(9),
    'MISSED'.padStart(7),
    'P99_ERR_MS'.padStart(11),
    'P99_RX_MS'.padStart(10),
    'EVICTED'.padStart(8),
  ].join('  ');
  console.log(`\n${header}`);
  for (const run of runs) {
    const server = run.server ?? {};
    const client = intervalStats(run.receiveTimes, run.hz * run.secs);
    console.log(
      [
        run.driver.padEnd(6),
        String(run.hz).padStart(3),
        String(run.work_us).padStart(8),
        String(server.ticks ?? '-').padStart(7),
        String(server.expected ?? '-').padStart(9),
        String(server.missed ?? '-').padStart(7),
        round3(server.interval_err_ms?.p99 ?? 0).toFixed(3).padStart(11),
        client.interval_ms.p99.toFixed(3).padStart(10),
        String(server.evicted ?? '-').padStart(8),
      ].join('  '),
    );
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const baseUrl = String(options.url ?? defaultUrl).replace(/\/+$/, '');
  const hzValues = parseList(options.hz ?? '20').map(Number);
  const workValues = parseList(options['work-us'] ?? '0').map(Number);
  const secs = Number(options.secs ?? 60);
  const scriptName = String(options['script-name'] ?? 'hearth-tick-bench');
  const outputPath = options.out ? String(options.out) : join(here, 'results.json');
  const analyticsEnabled = options.analytics !== false && options['no-analytics'] !== true;

  if (hzValues.length === 0 || !hzValues.every(Number.isFinite) || hzValues.some((hz) => hz < 1)) {
    throw new Error('--hz must be a comma-separated list of positive integers');
  }
  if (workValues.length === 0 || !workValues.every(Number.isFinite) || workValues.some((us) => us < 0)) {
    throw new Error('--work-us must be a comma-separated list of non-negative integers');
  }
  if (!Number.isFinite(secs) || secs < 1) {
    throw new Error('--secs must be a positive integer');
  }

  const runs = [];
  for (const driver of drivers) {
    for (const hz of hzValues) {
      for (const workUs of workValues) {
        runs.push({
          driver,
          hz,
          workUs,
          secs,
          work_us: workUs,
          name: `${driver}-${hz}-${workUs}-${Date.now().toString(36)}-${randomId()}`,
          receiveTimes: [],
          socket: null,
          server: null,
          error: null,
        });
      }
    }
  }

  console.log(`tick-bench: ${runs.length} runs against ${baseUrl} for ${secs}s each`);
  console.log(`  drivers: ${drivers.join(', ')}  hz: ${hzValues.join(', ')}  work_us: ${workValues.join(', ')}`);
  const window = { started_at: new Date().toISOString(), ended_at: new Date().toISOString() };

  for (const run of runs) {
    try {
      await openSocket(run, baseUrl);
    } catch (error) {
      run.error = String(error);
    }
  }
  window.started_at = new Date().toISOString();
  await Promise.all(
    runs.map((run) =>
      startRun(run, baseUrl).catch((error) => {
        run.error = String(error);
      }),
    ),
  );
  if (!runs.some((run) => run.error === null)) {
    throw new Error(`every run failed to start: ${runs.map((run) => run.error).join('; ')}`);
  }

  // Runs are in flight in raw wall time; the server stops itself after `secs`.
  await sleep((secs + 3) * 1000);
  window.ended_at = new Date().toISOString();
  await Promise.all(
    runs.map((run) =>
      collect(run, baseUrl).catch((error) => {
        run.error = run.error ?? String(error);
      }),
    ),
  );

  const accountId = process.env.CLOUDFLARE_ACCOUNT_ID;
  const apiToken = process.env.CLOUDFLARE_API_TOKEN;
  const analytics = analyticsEnabled
    ? await analyticsSection({ accountId, apiToken, window, scriptName, runs, secs })
    : { skipped: '--no-analytics', estimate: billingEstimate(runs, secs) };

  const results = {
    generated_at: new Date().toISOString(),
    url: baseUrl,
    secs,
    script_name: scriptName,
    window,
    runs: runs.map((run) => ({
      driver: run.driver,
      hz: run.hz,
      work_us: run.work_us,
      name: run.name,
      server: run.server,
      client: intervalStats(run.receiveTimes, run.hz * secs),
      error: run.error,
    })),
    analytics,
  };
  writeFileSync(outputPath, `${JSON.stringify(results, null, 2)}\n`);
  printSummary(runs);
  if (analytics.error) {
    console.warn(`\nanalytics query failed (best effort): ${JSON.stringify(analytics.error)}`);
  }
  if (!accountId || !apiToken) {
    console.log('\nanalytics: skipped (CLOUDFLARE_API_TOKEN / CLOUDFLARE_ACCOUNT_ID not set)');
  }
  console.log(`\nwrote ${outputPath}`);

  if (runs.some((run) => run.error !== null)) {
    for (const run of runs.filter((candidate) => candidate.error !== null)) {
      console.error(`${run.name}: ${run.error}`);
    }
    process.exitCode = 1;
  }
}

main().catch((error) => {
  console.error(String(error?.stack ?? error));
  process.exitCode = 1;
});
