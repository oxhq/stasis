import assert from "node:assert/strict";
import test from "node:test";

import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  SETTLEMENT_EVIDENCE_MAX_ITEMS,
  settlementEvidence,
} from "../src/evidence.js";
import type {
  DocumentStateToken,
  ExternalIoSnapshot,
  PendingSnapshot,
  SessionSettleResult,
  SettleResult,
  UnsupportedWork,
} from "../src/types.js";
import type { SettlementEvidenceV1, SettlementEvidenceV2 } from "../src/evidence.js";

const documentToken = (alias: bigint): DocumentStateToken =>
  `document:33333333333333333333333333333333:${alias}` as DocumentStateToken;

const baseResult = {
  virtualTimeNs: 10n,
  wallTimeNs: 20n,
  stateGeneration: 30n,
  domEpoch: 40n,
  effectivePolicy: {
    persistentWork: "report" as const,
    maxVirtualTimeNs: 1_000n,
    maxControlTurns: 100n,
    wallIoTimeoutNs: 500n,
  },
  processed: {
    controlTurns: 4n,
    tasks: 5n,
    microtasks: 6n,
    renderingOpportunities: 7n,
    mutations: 8n,
  },
  snapshot: {} as PendingSnapshot,
  persistentWork: [],
  externalIo: [],
  unsupportedWork: [],
};

test("v1 settlement evidence remains shape-identical for a legacy result", () => {
  const result: SettleResult = { ...baseResult, outcome: "quiescent" };
  const evidence: SettlementEvidenceV1 = settlementEvidence(result);

  assert.deepEqual(evidence, {
    schemaVersion: 1,
    completeness: "terminal_snapshot",
    profile: CONTROLLED_WEBAPP_V1_PROFILE,
    outcome: "quiescent",
    virtualTimeNs: 10n,
    stateGeneration: 30n,
    domEpoch: 40n,
    reason: { kind: "quiescent" },
    bounds: { maxItems: SETTLEMENT_EVIDENCE_MAX_ITEMS },
  });
  assert.deepEqual(Object.keys(evidence), [
    "schemaVersion",
    "completeness",
    "profile",
    "outcome",
    "virtualTimeNs",
    "stateGeneration",
    "domEpoch",
    "bounds",
    "reason",
  ]);
  assert.equal(Object.hasOwn(evidence, "stateToken"), false);
});

test("v2 settlement evidence includes only the exact document state token", () => {
  const stateToken = documentToken(7n);
  const result = {
    ...baseResult,
    outcome: "quiescent" as const,
    stateToken,
    snapshot: { ...baseResult.snapshot, stateToken },
    sessionStateToken: "must-not-leak",
  } as SessionSettleResult & { sessionStateToken: string };
  const evidence: SettlementEvidenceV2 = settlementEvidence(result);

  assert.deepEqual(evidence, {
    schemaVersion: 2,
    completeness: "terminal_snapshot",
    profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
    stateToken,
    outcome: "quiescent",
    virtualTimeNs: 10n,
    stateGeneration: 30n,
    domEpoch: 40n,
    reason: { kind: "quiescent" },
    bounds: { maxItems: SETTLEMENT_EVIDENCE_MAX_ITEMS },
  });
  assert.equal(Object.hasOwn(evidence, "sessionStateToken"), false);
});

test("v2 settlement evidence uses the same bounded redacted reason projection", () => {
  const stateToken = documentToken(8n);
  const externalIo = Array.from(
    { length: SETTLEMENT_EVIDENCE_MAX_ITEMS + 1 },
    (_, index) =>
      ({
        sourceId: String(index + 1),
        kind: "fetch",
        phase: "awaiting_response",
        owner: "script",
        loadBlocking: "non_blocking",
        startedAtNs: BigInt(index),
        url: `https://example.test/private?token=${index}`,
      }) as ExternalIoSnapshot,
  );
  const result = {
    ...baseResult,
    outcome: "blocked_on_external_io" as const,
    externalIo,
    stateToken,
    snapshot: { ...baseResult.snapshot, stateToken },
  } as SessionSettleResult;
  const evidence = settlementEvidence(result);

  assert.equal(evidence.reason.kind, "external_io");
  if (evidence.reason.kind !== "external_io") return;
  assert.equal(evidence.reason.items.length, SETTLEMENT_EVIDENCE_MAX_ITEMS);
  assert.equal(evidence.reason.omitted, 1);
  assert.deepEqual(evidence.reason.items[0], {
    sourceId: "1",
    kind: "fetch",
    phase: "awaiting_response",
    owner: "script",
    loadBlocking: "non_blocking",
    startedAtNs: 0n,
  });
});

test("session evidence fails closed on ambiguous or mismatched token binding", () => {
  const stateToken = documentToken(9n);
  const resultOnly = {
    ...baseResult,
    outcome: "quiescent" as const,
    stateToken,
  } as unknown as SessionSettleResult;
  assert.throws(() => settlementEvidence(resultOnly), /both the result and its snapshot/u);

  const mismatch = {
    ...baseResult,
    outcome: "quiescent" as const,
    stateToken,
    snapshot: {
      ...baseResult.snapshot,
      stateToken: documentToken(10n),
    },
  } as SessionSettleResult;
  assert.throws(() => settlementEvidence(mismatch), /disagrees with its snapshot/u);
});

test("settlement evidence caps blockers at 32 and reports the omitted count", () => {
  const externalIo = Array.from(
    { length: SETTLEMENT_EVIDENCE_MAX_ITEMS + 8 },
    (_, index): ExternalIoSnapshot => ({
      sourceId: String(index + 1),
      kind: "fetch",
      phase: "awaiting_response",
      owner: "script",
      loadBlocking: "non_blocking",
      startedAtNs: BigInt(index),
    }),
  );
  const result: SettleResult = {
    ...baseResult,
    outcome: "blocked_on_external_io",
    externalIo,
  };

  const evidence = settlementEvidence(result);
  assert.equal(evidence.reason.kind, "external_io");
  if (evidence.reason.kind !== "external_io") return;
  assert.equal(evidence.reason.items.length, SETTLEMENT_EVIDENCE_MAX_ITEMS);
  assert.equal(evidence.reason.omitted, 8);
  assert.equal(evidence.reason.items.at(-1)?.sourceId, "32");
});

test("settlement evidence reconstructs allow-listed fields instead of leaking attached metadata", () => {
  const operation = {
    sourceId: "7",
    kind: "fetch",
    phase: "awaiting_response",
    owner: "script",
    loadBlocking: "non_blocking",
    startedAtNs: 5n,
    url: "https://example.test/private?token=secret",
    headers: { authorization: "secret" },
    body: "secret",
  } as ExternalIoSnapshot;
  const result: SettleResult = {
    ...baseResult,
    outcome: "blocked_on_external_io",
    externalIo: [operation],
  };

  const evidence = settlementEvidence(result);
  assert.equal(evidence.reason.kind, "external_io");
  if (evidence.reason.kind !== "external_io") return;
  assert.notEqual(evidence.reason.items[0], operation);
  assert.deepEqual(evidence.reason.items[0], {
    sourceId: "7",
    kind: "fetch",
    phase: "awaiting_response",
    owner: "script",
    loadBlocking: "non_blocking",
    startedAtNs: 5n,
  });
});

test("settlement evidence preserves typed unsupported and limit reasons", () => {
  const unsupported = {
    sourceId: "9",
    kind: "other",
    count: 1n,
    reason: "time_surface",
    timeSurface: "external_subscription",
    selector: "#private",
  } as UnsupportedWork;
  const unsupportedResult: SettleResult = {
    ...baseResult,
    outcome: "unsupported_work",
    unsupportedWork: [unsupported],
    failure: { code: "unsupported_clock_surface" },
  };
  const unsupportedEvidence = settlementEvidence(unsupportedResult);
  assert.deepEqual(unsupportedEvidence.reason, {
    kind: "unsupported_work",
    code: "unsupported_clock_surface",
    items: [
      {
        sourceId: "9",
        kind: "other",
        count: 1n,
        reason: "time_surface",
        timeSurface: "external_subscription",
      },
    ],
    omitted: 0,
  });

  const limitResult: SettleResult = {
    ...baseResult,
    outcome: "virtual_time_limit_exceeded",
    limit: {
      kind: "virtual_time",
      limit: 100n,
      startVirtualTimeNs: 10n,
      requestedVirtualTimeNs: 110n,
    },
  };
  assert.deepEqual(settlementEvidence(limitResult).reason, {
    kind: "limit",
    limit: {
      kind: "virtual_time",
      limit: 100n,
      startVirtualTimeNs: 10n,
      requestedVirtualTimeNs: 110n,
    },
  });
});

test("settlement evidence preserves the runtime failure code without copying its snapshot", () => {
  const result: SettleResult = {
    ...baseResult,
    outcome: "runtime_error",
    failure: { code: "runtime_terminals" },
  };

  assert.deepEqual(settlementEvidence(result).reason, {
    kind: "runtime_error",
    code: "runtime_terminals",
  });
});

test("settlement evidence does not mutate frozen settlement data", () => {
  const item = Object.freeze({
    sourceId: "1",
    kind: "timer" as const,
    count: 1n,
    reason: "interval" as const,
    requestedPeriodNs: 1_000n,
  });
  const persistentWork = Object.freeze([item]);
  const result = Object.freeze({
    ...baseResult,
    outcome: "quiescent_with_persistent_work" as const,
    persistentWork,
  }) as unknown as SettleResult;

  const evidence = settlementEvidence(result);
  assert.equal(evidence.reason.kind, "persistent_work");
  if (evidence.reason.kind !== "persistent_work") return;
  assert.deepEqual(evidence.reason.items, [item]);
  assert.notEqual(evidence.reason.items[0], item);
  assert.equal(result.persistentWork[0], item);
});
