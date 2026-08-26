import type {
  DocumentStateToken,
  ExternalIoSnapshot,
  PersistentWork,
  SessionSettleResult,
  SettleFailureCode,
  SettleLimit,
  SettleOutcome,
  SettleResult,
  UnsupportedWork,
} from "./types.js";
import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  isSelectableSessionProfile,
  type SelectableSessionProfile,
  type SessionSupportProfile,
} from "./profile.js";

export {
  CONTROLLED_WEBAPP_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V2_PROFILE,
} from "./profile.js";

export const SETTLEMENT_EVIDENCE_MAX_ITEMS = 32 as const;

const runtimeSessionSettleProfiles = new WeakMap<object, SelectableSessionProfile>();

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

/** A bounded session terminal snapshot bound to one exact document and selected session profile. */
export interface SettlementEvidenceV2<
  Profile extends SelectableSessionProfile = SessionSupportProfile,
> {
  readonly schemaVersion: 2;
  readonly completeness: "terminal_snapshot";
  readonly profile: Profile;
  readonly stateToken: DocumentStateToken;
  readonly outcome: SettleOutcome;
  readonly virtualTimeNs: bigint;
  readonly stateGeneration: bigint;
  readonly domEpoch: bigint;
  readonly reason: SettlementEvidenceReason;
  readonly bounds: {
    readonly maxItems: typeof SETTLEMENT_EVIDENCE_MAX_ITEMS;
  };
}

/** Build session evidence using the result's SDK-bound profile, or v1 for a manual legacy result. */
export function settlementEvidence<
  Profile extends SelectableSessionProfile = SessionSupportProfile,
>(result: SessionSettleResult<Profile>): SettlementEvidenceV2<Profile>;
/** A structurally copied or manual session result has only the legacy-v1 identity. */
export function settlementEvidence(
  result: SessionSettleResult,
): SettlementEvidenceV2<SessionSupportProfile>;
/** Build session evidence bound to an explicitly selected session profile. */
export function settlementEvidence<Profile extends SelectableSessionProfile>(
  result: SessionSettleResult<Profile>,
  profile: NoInfer<Profile>,
): SettlementEvidenceV2<Profile>;
export function settlementEvidence(
  result: SettleResult & { readonly stateToken?: never },
): SettlementEvidenceV1;
export function settlementEvidence(
  result: SettleResult | SessionSettleResult<SelectableSessionProfile>,
  profile?: SelectableSessionProfile,
): SettlementEvidenceV1 | SettlementEvidenceV2<SelectableSessionProfile> {
  const stateToken = sessionDocumentStateToken(result);
  const reason = settlementEvidenceReason(result);
  if (stateToken !== null) {
    if (profile !== undefined && !isSelectableSessionProfile(profile)) {
      throw new TypeError("Session settlement evidence requires a supported session profile");
    }
    const runtimeProfile = runtimeSessionSettleProfiles.get(result);
    if (
      runtimeProfile === undefined &&
      profile !== undefined &&
      profile !== CONTROLLED_WEB_SESSION_V1_PROFILE
    ) {
      throw new TypeError(
        `Unbound session settle results can only produce ${CONTROLLED_WEB_SESSION_V1_PROFILE} evidence`,
      );
    }
    if (runtimeProfile !== undefined && profile !== undefined && runtimeProfile !== profile) {
      throw new TypeError(
        `Session settlement evidence profile ${profile} does not match runtime-bound profile ${runtimeProfile}`,
      );
    }
    const selectedProfile = runtimeProfile ?? profile ?? CONTROLLED_WEB_SESSION_V1_PROFILE;
    return {
      schemaVersion: 2,
      completeness: "terminal_snapshot",
      profile: selectedProfile,
      stateToken,
      outcome: result.outcome,
      virtualTimeNs: result.virtualTimeNs,
      stateGeneration: result.stateGeneration,
      domEpoch: result.domEpoch,
      reason,
      bounds: { maxItems: SETTLEMENT_EVIDENCE_MAX_ITEMS },
    };
  }

  if (profile !== undefined) {
    throw new TypeError("Legacy settlement evidence cannot carry a session profile");
  }

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

  return { ...base, reason };
}

function settlementEvidenceReason(result: SettleResult): SettlementEvidenceReason {
  switch (result.outcome) {
    case "quiescent":
      return { kind: "quiescent" };

    case "quiescent_with_persistent_work":
    case "blocked_on_open_ended_work": {
      const { items, omitted } = boundedCopy(result.persistentWork, copyPersistentWork);
      return { kind: "persistent_work", items, omitted };
    }

    case "blocked_on_external_io": {
      const { items, omitted } = boundedCopy(result.externalIo, copyExternalIo);
      return { kind: "external_io", items, omitted };
    }

    case "unsupported_work": {
      const { items, omitted } = boundedCopy(result.unsupportedWork, copyUnsupportedWork);
      return { kind: "unsupported_work", code: result.failure.code, items, omitted };
    }

    case "virtual_time_limit_exceeded":
    case "task_limit_exceeded":
    case "microtask_limit_exceeded":
    case "rendering_limit_exceeded":
    case "mutation_limit_exceeded":
    case "control_turn_limit_exceeded":
      return { kind: "limit", limit: copyLimit(result.limit) };

    case "runtime_error":
      return { kind: "runtime_error", code: result.failure.code };
  }
}

function sessionDocumentStateToken(
  result: SettleResult | SessionSettleResult,
): DocumentStateToken | null {
  const resultRecord = result as unknown as Record<string, unknown>;
  const snapshotValue = resultRecord.snapshot;
  const snapshotRecord =
    typeof snapshotValue === "object" && snapshotValue !== null && !Array.isArray(snapshotValue)
      ? (snapshotValue as Record<string, unknown>)
      : null;
  const resultHasToken = Object.hasOwn(resultRecord, "stateToken");
  const snapshotHasToken = snapshotRecord !== null && Object.hasOwn(snapshotRecord, "stateToken");
  if (!resultHasToken && !snapshotHasToken) return null;
  if (!resultHasToken || !snapshotHasToken) {
    throw new TypeError(
      "Session settlement evidence requires stateToken on both the result and its snapshot",
    );
  }
  const stateToken = resultRecord.stateToken;
  const snapshotStateToken = snapshotRecord.stateToken;
  if (typeof stateToken !== "string" || stateToken.length === 0) {
    throw new TypeError("Session settlement evidence requires a non-empty stateToken");
  }
  if (snapshotStateToken !== stateToken) {
    throw new TypeError("Session settlement evidence stateToken disagrees with its snapshot");
  }
  return stateToken as DocumentStateToken;
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

/** @internal Bind a decoded SDK result to its selected session without mutating wire evidence. */
export function bindSessionSettleResultProfile<Profile extends SelectableSessionProfile>(
  result: SessionSettleResult<SelectableSessionProfile>,
  profile: Profile,
): SessionSettleResult<Profile> {
  if (!isSelectableSessionProfile(profile)) {
    throw new TypeError("Session settle result requires a supported session profile");
  }
  runtimeSessionSettleProfiles.set(result, profile);
  return result as SessionSettleResult<Profile>;
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
