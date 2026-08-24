import type {
  ExternalIoSnapshot,
  PersistentWork,
  SettleFailureCode,
  SettleLimit,
  SettleOutcome,
  SettleResult,
  UnsupportedWork,
} from "./types.js";
import { CONTROLLED_WEBAPP_V1_PROFILE } from "./profile.js";

export { CONTROLLED_WEBAPP_V1_PROFILE } from "./profile.js";

export const SETTLEMENT_EVIDENCE_MAX_ITEMS = 32 as const;

export type SettlementEvidenceReason =
  | { readonly kind: "quiescent" }
  | {
      readonly kind: "persistent_work";
      readonly items: readonly Readonly<PersistentWork>[];
      readonly omitted: number;
    }
  | {
      readonly kind: "external_io";
      readonly items: readonly Readonly<ExternalIoSnapshot>[];
      readonly omitted: number;
    }
  | {
      readonly kind: "unsupported_work";
      readonly code: SettleFailureCode;
      readonly items: readonly Readonly<UnsupportedWork>[];
      readonly omitted: number;
    }
  | { readonly kind: "limit"; readonly limit: Readonly<SettleLimit> }
  | { readonly kind: "runtime_error"; readonly code: SettleFailureCode };

/**
 * A bounded explanation of why one settlement call terminated.
 *
 * This is deliberately a terminal snapshot, not a causal journal. It copies only the
 * allow-listed blocker metadata already present in SettleResult. Selectors, fill values, URLs,
 * headers, and bodies cannot enter this projection.
 */
export interface SettlementEvidenceV1 {
  readonly schemaVersion: 1;
  readonly completeness: "terminal_snapshot";
  readonly profile: typeof CONTROLLED_WEBAPP_V1_PROFILE;
  readonly outcome: SettleOutcome;
  readonly virtualTimeNs: bigint;
  readonly stateGeneration: bigint;
  readonly domEpoch: bigint;
  readonly reason: SettlementEvidenceReason;
  readonly bounds: {
    readonly maxItems: typeof SETTLEMENT_EVIDENCE_MAX_ITEMS;
  };
}

/** Build a bounded, redacted terminal-settlement explanation without mutating the result. */
export function settlementEvidence(result: SettleResult): SettlementEvidenceV1 {
  const base = {
    schemaVersion: 1 as const,
    completeness: "terminal_snapshot" as const,
    profile: CONTROLLED_WEBAPP_V1_PROFILE,
    outcome: result.outcome,
    virtualTimeNs: result.virtualTimeNs,
    stateGeneration: result.stateGeneration,
    domEpoch: result.domEpoch,
    bounds: { maxItems: SETTLEMENT_EVIDENCE_MAX_ITEMS },
  };

  switch (result.outcome) {
    case "quiescent":
      return { ...base, reason: { kind: "quiescent" } };

    case "quiescent_with_persistent_work":
    case "blocked_on_open_ended_work": {
      const { items, omitted } = boundedCopy(result.persistentWork, copyPersistentWork);
      return { ...base, reason: { kind: "persistent_work", items, omitted } };
    }

    case "blocked_on_external_io": {
      const { items, omitted } = boundedCopy(result.externalIo, copyExternalIo);
      return { ...base, reason: { kind: "external_io", items, omitted } };
    }

    case "unsupported_work": {
      const { items, omitted } = boundedCopy(result.unsupportedWork, copyUnsupportedWork);
      return {
        ...base,
        reason: { kind: "unsupported_work", code: result.failure.code, items, omitted },
      };
    }

    case "virtual_time_limit_exceeded":
    case "task_limit_exceeded":
    case "microtask_limit_exceeded":
    case "rendering_limit_exceeded":
    case "mutation_limit_exceeded":
    case "control_turn_limit_exceeded":
      return { ...base, reason: { kind: "limit", limit: copyLimit(result.limit) } };

    case "runtime_error":
      return { ...base, reason: { kind: "runtime_error", code: result.failure.code } };
  }
}

function boundedCopy<Input, Output>(
  values: readonly Input[],
  copy: (value: Input) => Output,
): { items: Output[]; omitted: number } {
  const retained = values.slice(0, SETTLEMENT_EVIDENCE_MAX_ITEMS);
  return {
    items: retained.map(copy),
    omitted: Math.max(0, values.length - retained.length),
  };
}

function copyPersistentWork(work: PersistentWork): PersistentWork {
  return {
    ...(work.sourceId === undefined ? {} : { sourceId: work.sourceId }),
    kind: work.kind,
    count: work.count,
    reason: work.reason,
    ...(work.requestedPeriodNs === undefined
      ? {}
      : { requestedPeriodNs: work.requestedPeriodNs }),
  };
}

function copyExternalIo(operation: ExternalIoSnapshot): ExternalIoSnapshot {
  return {
    sourceId: operation.sourceId,
    kind: operation.kind,
    phase: operation.phase,
    owner: operation.owner,
    loadBlocking: operation.loadBlocking,
    startedAtNs: operation.startedAtNs,
  };
}

function copyUnsupportedWork(work: UnsupportedWork): UnsupportedWork {
  return {
    ...(work.sourceId === undefined ? {} : { sourceId: work.sourceId }),
    kind: work.kind,
    count: work.count,
    reason: work.reason,
    ...(work.timeSurface === undefined ? {} : { timeSurface: work.timeSurface }),
  };
}

function copyLimit(limit: SettleLimit): SettleLimit {
  if (limit.kind === "virtual_time") {
    return {
      kind: "virtual_time",
      limit: limit.limit,
      startVirtualTimeNs: limit.startVirtualTimeNs,
      requestedVirtualTimeNs: limit.requestedVirtualTimeNs,
    };
  }
  if (limit.kind === "control_turns") {
    return { kind: "control_turns", limit: limit.limit };
  }
  return { kind: limit.kind, limit: limit.limit, observed: limit.observed };
}
