#!/usr/bin/env node
// Micro-benchmark: in-process `auto_node` Runner call latency (ADR-0026).
//
// The napi twin of evals/embedded-python/bench.py — same protocol, same
// honesty rules, so the two rungs are comparable:
//
//   * warmup calls are executed but NOT counted in the statistics;
//   * the one-time load cost (module compiled once) is reported separately;
//   * abstentions/errors are timed like any call and counted, never hidden;
//   * percentiles use linear interpolation between ranks (numpy's "linear"),
//     stated so the reported number is reproducible.
//
// v0 embeds PURE artifacts only: a capability-bearing artifact is refused at
// load with a thrown Error whose code is "AutoError" (see README.md). This
// script measures; it makes no parity or cross-machine claim — the numbers
// are wall time (process.hrtime.bigint) on the machine that ran it.
//
// usage:
//   node bench.js ARTIFACT.cbin INPUTS.jsonl [--warmup W] [--iters N] [--addon PATH]
//
// INPUTS.jsonl: one JSON value per line (the same protocol as `auto run --stdio`).
// --addon: path to the built addon (auto_node.node, or the raw cargo cdylib —
// process.dlopen loads either). Default: first existing of
//   crates/auto-node/auto_node.node, target/release/auto_node.node,
//   target/release/auto_node.dll | libauto_node.so | libauto_node.dylib.
'use strict';

const fs = require('fs');
const path = require('path');

function percentile(sortedUs, q) {
  // Percentile of an already-sorted array, q in [0, 100]. Linear
  // interpolation between the two closest ranks — same method as bench.py.
  if (sortedUs.length === 0) return NaN;
  if (sortedUs.length === 1) return sortedUs[0];
  const rank = (q / 100.0) * (sortedUs.length - 1);
  const low = Math.floor(rank);
  const high = Math.min(low + 1, sortedUs.length - 1);
  const frac = rank - low;
  return sortedUs[low] * (1.0 - frac) + sortedUs[high] * frac;
}

function loadInputs(file) {
  // Read non-blank lines, validating each is JSON so we bench the runner,
  // not a parse error inside it. Throws (loud) on malformed input.
  const lines = fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  for (const line of lines) JSON.parse(line);
  return lines;
}

function defaultAddonCandidates(repoRoot) {
  return [
    path.join(repoRoot, 'crates', 'auto-node', 'auto_node.node'),
    path.join(repoRoot, 'target', 'release', 'auto_node.node'),
    path.join(repoRoot, 'target', 'release', 'auto_node.dll'),
    path.join(repoRoot, 'target', 'release', 'libauto_node.so'),
    path.join(repoRoot, 'target', 'release', 'libauto_node.dylib'),
  ];
}

function loadAddon(addonPath) {
  // process.dlopen is what require() uses for .node files, minus the
  // extension check — so a raw cargo cdylib (auto_node.dll) loads too.
  const holder = { exports: {} };
  process.dlopen(holder, path.resolve(addonPath));
  return holder.exports;
}

function parseArgs(argv) {
  const args = { warmup: 200, iters: 5000, addon: null, positional: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--warmup' || arg === '--iters' || arg === '--addon') {
      const value = argv[i + 1];
      if (value === undefined) throw new Error(`${arg} needs a value`);
      if (arg === '--addon') args.addon = value;
      else args[arg.slice(2)] = Number.parseInt(value, 10);
      i += 1;
    } else {
      args.positional.push(arg);
    }
  }
  return args;
}

function main() {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (e) {
    console.error(String(e.message || e));
    return 2;
  }
  if (args.positional.length !== 2) {
    console.error(
      'usage: node bench.js ARTIFACT.cbin INPUTS.jsonl [--warmup W] [--iters N] [--addon PATH]'
    );
    return 2;
  }
  if (!Number.isInteger(args.iters) || args.iters <= 0) {
    console.error('--iters must be a positive integer');
    return 2;
  }
  if (!Number.isInteger(args.warmup) || args.warmup < 0) {
    console.error('--warmup must be a non-negative integer');
    return 2;
  }
  const [artifact, inputsFile] = args.positional;

  const repoRoot = path.resolve(__dirname, '..', '..');
  let addonPath = args.addon;
  if (addonPath === null) {
    addonPath = defaultAddonCandidates(repoRoot).find((p) => fs.existsSync(p));
    if (addonPath === undefined) {
      console.error(
        'no built addon found; build it first (see evals/embedded-node/README.md) ' +
          'or pass --addon PATH'
      );
      return 2;
    }
  }

  const autoNode = loadAddon(addonPath);
  const lines = loadInputs(inputsFile);
  if (lines.length === 0) {
    console.error('no input lines');
    return 2;
  }

  // one-time load: the wasm module is compiled exactly once, here.
  const loadStart = process.hrtime.bigint();
  let runner;
  try {
    runner = new autoNode.Runner(artifact);
  } catch (e) {
    if (e && e.code === 'AutoError') {
      console.error(`load refused: ${e.message}`);
      return 1;
    }
    throw e;
  }
  const loadNs = Number(process.hrtime.bigint() - loadStart);

  // The three runner outcomes, by thrown error code (ADR-0026). Anything
  // else is a harness bug and propagates — fail loud, never miscount.
  function call(line) {
    try {
      runner.answer(line);
      return 'output';
    } catch (e) {
      if (e && e.code === 'AutoAbstained') return 'abstained';
      if (e && e.code === 'AutoError') return 'error';
      throw e;
    }
  }

  // warmup — executed, not counted
  for (let i = 0; i < args.warmup; i += 1) call(lines[i % lines.length]);

  // timed region
  const durationsUs = [];
  const counts = { output: 0, abstained: 0, error: 0 };
  const wallStart = process.hrtime.bigint();
  for (let i = 0; i < args.iters; i += 1) {
    const line = lines[i % lines.length];
    const callStart = process.hrtime.bigint();
    const kind = call(line);
    const callEnd = process.hrtime.bigint();
    durationsUs.push(Number(callEnd - callStart) / 1000.0);
    counts[kind] += 1;
  }
  const wallNs = Number(process.hrtime.bigint() - wallStart);

  durationsUs.sort((a, b) => a - b);
  const meanUs = durationsUs.reduce((a, b) => a + b, 0) / durationsUs.length;
  const callsPerSec = args.iters / (wallNs / 1e9);
  const int = (n) => n.toLocaleString('en-US', { maximumFractionDigits: 0 });

  console.log(`auto_node version: ${autoNode.version()}`);
  console.log(`addon:             ${addonPath}`);
  console.log(`artifact:          ${artifact}`);
  console.log(`inputs:            ${lines.length} distinct line(s)`);
  console.log(`one-time load:     ${(loadNs / 1000.0).toFixed(1)} us  (module compiled once)`);
  console.log(`warmup calls:      ${args.warmup}  (not counted)`);
  console.log(`timed calls:       ${args.iters}`);
  console.log(
    `outcomes:          output=${counts.output} abstained=${counts.abstained} error=${counts.error}`
  );
  console.log(`per-call p50:      ${percentile(durationsUs, 50).toFixed(3)} us`);
  console.log(`per-call p95:      ${percentile(durationsUs, 95).toFixed(3)} us`);
  console.log(`per-call mean:     ${meanUs.toFixed(3)} us`);
  console.log(`throughput:        ${int(callsPerSec)} calls/sec`);
  return 0;
}

process.exitCode = main();
