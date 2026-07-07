/**
 * Auto trace SDK for typescript agents (S1).
 *
 * Real functionality: recording agent runs to v0 JSONL traces and replaying
 * them with recorded outputs substituted for live calls — the same contract
 * as the python SDK. See spec/trace.md.
 *
 * Not yet here (later spine items): automatic instrumentation of model/tool
 * frameworks, remote export. What is here works and is tested; nothing else
 * is pretended.
 */

export {
  FORMAT_VERSION,
  ReplayDivergence,
  ReplayedError,
  SDK_NAME,
  SDK_VERSION,
  Tracer,
  canonicalJson,
  digestHex,
} from "./tracer.ts";
export type { TracerOptions } from "./tracer.ts";
