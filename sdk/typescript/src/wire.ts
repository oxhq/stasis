import { StasisTransportError } from "./errors.js";
import type {
  AdvanceToNextResult,
  ClockOptions,
  EffectiveSettlePolicy,
  PendingSnapshot,
  RuntimeInfo,
  SettleOutcome,
  SettlePolicy,
  SettleResult,
} from "./types.js";

const MAX_U128 = (1n << 128n) - 1n;
const MAX_U64 = (1n << 64n) - 1n;

/** All product/server spelling is confined to this module. */
export const METHOD = {
  initialize: "protocol.initialize",
  open: "session.open",
  evaluate: "dom.evaluate",
  text: "dom.text",
  activate: "action.activate",
  pending: "runtime.pending",
  settle: "runtime.settle",
  advanceToNext: "runtime.advance_to_next",
  close: "session.close",
} as const;

const SETTLE_OUTCOMES = new Set<SettleOutcome>([
  "quiescent",
  "quiescent_with_persistent_work",
  "blocked_on_external_io",
  "blocked_on_open_ended_work",
  "unsupported_work",
  "virtual_time_limit_exceeded",
  "control_turn_limit_exceeded",
  "runtime_error",
]);

const TIME_SURFACES = stringSet([
  "window_timers",
  "same_event_loop_iframe",
  "java_script_date",
  "performance",
  "host_timestamp",
  "update_rendering",
  "animation_frame",
  "document_timeline",
  "worker",
  "worklet",
  "cross_event_loop_iframe",
  "cross_event_loop_navigation",
  "auxiliary_web_view",
  "resource_thread_io",
  "external_subscription",
  "native_media",
  "embedder_control",
]);
const PRODUCER_STABILITIES = stringSet([
  "not_checkpointed",
  "busy",
  "first_empty",
  "stable_empty",
  "unqualified",
]);
const NETWORK_KINDS = stringSet([
  "navigation",
  "fetch",
  "xml_http_request",
  "image",
  "font",
  "stylesheet",
  "script",
  "unclassified_producer_io",
  "other",
]);
const EXTERNAL_IO_PHASES = stringSet([
  "queued",
  "awaiting_response",
  "streaming_body",
  "terminal_task_queued",
]);
const EXTERNAL_IO_OWNERS = stringSet([
  "top_level_navigation",
  "document_parser",
  "script",
  "document_subresource",
  "rendering_resource",
  "other",
]);
const LOAD_BLOCKING_VALUES = stringSet(["blocking", "non_blocking", "unknown"]);
const SOURCE_KINDS = stringSet([
  "task",
  "microtask",
  "timer",
  "animation_frame",
  "animation",
  "network",
  "parser",
  "rendering_update",
  "tracked_presence",
  "other",
]);
const OPEN_ENDED_REASONS = stringSet([
  "interval",
  "infinite_animation",
  "web_socket",
  "event_source",
  "broadcast_channel",
  "message_port",
  "embedder_control",
  "media_session_action_handler",
  "storage_event_listener",
]);
const PERSISTENT_REASONS = stringSet([
  "interval",
  "infinite_animation",
  "infinite_animated_image",
]);
const UNSUPPORTED_REASONS = stringSet([
  "time_surface",
  "unclassified_timer",
  "unclassified_animation",
  "animated_image",
  "web_socket",
  "event_source",
  "broadcast_channel",
  "message_port",
  "embedder_control",
  "media_session_action_handler",
  "storage_event_listener",
  "clock_not_controlled",
  "canvas_upload",
  "font_load",
  "image_load",
  "inactive_rendering",
  "throttled_rendering",
  "ineligible_logical_timer",
  "throttled_task",
  "inactive_task",
  "cross_event_loop_document",
  "worker",
  "worklet",
  "media_element",
  "graphics_source",
  "storage_backend",
  "service_worker",
  "external_subscription",
  "untracked_callback",
  "script_created_parser_input",
  "suspended_parser",
]);
const RUNTIME_FAILURE_COMPONENTS = stringSet([
  "clock",
  "target_time",
  "scheduler",
  "producer",
  "microtasks",
  "input_revision",
  "source_identity",
  "logical_timer",
  "animated_image_timer",
  "dom_generation",
  "state_generation",
  "navigation_revision",
  "pipeline_membership_revision",
  "source_epoch",
]);
const SETTLE_FAILURE_CODES = stringSet([
  "runtime_terminals",
  "web_view_identity_changed",
  "clock_not_controlled",
  "unsupported_clock_surface",
  "clock_identity_changed",
  "virtual_time_regressed",
  "unsupported_source",
  "unsupported_open_ended_source",
  "unsupported_rendering",
  "unsupported_retained_tasks",
  "ineligible_logical_timer_head",
  "inconsistent_pending_evidence",
  "missing_finite_scheduler_head",
  "unclassified_scheduler_head",
  "missing_advance_authority",
  "mismatched_advance_authority",
  "quiet_checkpoint_did_not_advance",
]);

const WIDE_INTEGER_FIELDS = new Set([
  "stateGeneration",
  "domEpoch",
  "virtualTimeNs",
  "sourceEpoch",
  "readyEvents",
  "ready",
  "throttled",
  "inactive",
  "queued",
  "pending",
  "futureFinite",
  "persistent",
  "unsupported",
  "nextDeadlineNs",
  "total",
  "awaitingExternalIo",
  "awaitingCommit",
  "awaitingScriptInput",
  "suspended",
  "navigation",
  "fetch",
  "xmlHttpRequest",
  "image",
  "font",
  "stylesheet",
  "script",
  "unclassifiedProducerIo",
  "other",
  "startedAtNs",
  "nextOpportunityNs",
  "retainedAnimationFrames",
  "runnableAnimationFrames",
  "pendingAnimationEvents",
  "finiteAnimations",
  "persistentAnimations",
  "unsupportedAnimations",
  "finiteAnimatedImages",
  "persistentAnimatedImages",
  "unsupportedAnimatedImages",
  "dirtyCanvases",
  "unsupportedCanvases",
  "pendingFonts",
  "pendingImages",
  "deadlineNs",
  "requestedPeriodNs",
  "occurrences",
  "wallTimeNs",
  "wallIoTimeoutNs",
  "maxVirtualTimeNs",
  "maxControlTurns",
  "controlTurns",
  "count",
  "fromVirtualTimeNs",
  "limit",
  "startVirtualTimeNs",
  "requestedVirtualTimeNs",
]);

export function encodeOpenParams(url: string | URL, clock: ClockOptions | undefined): Record<string, unknown> {
  const serializedUrl = typeof url === "string" ? url : url.toString();
  if (serializedUrl.length === 0) throw new TypeError("url must not be empty");
  const params: Record<string, unknown> = { url: serializedUrl };
  if (clock === undefined) return params;
  if (clock.mode === "real") {
    params.clockMode = "real";
    return params;
  }
  if (clock.mode !== "controlled") {
    throw new TypeError("clock.mode must be real or controlled");
  }
  params.clockMode = "controlled";
  params.initialVirtualTimeNs = encodeU128(
    clock.initialVirtualTimeNs ?? 0n,
    "initialVirtualTimeNs",
  );
  const unixTimeOriginNs = clock.unixTimeOriginNs ?? 0n;
  if (unixTimeOriginNs !== 0n) {
    throw new RangeError("unixTimeOriginNs must be 0 in the controlled MVP");
  }
  params.unixTimeOriginNs = encodeU128(unixTimeOriginNs, "unixTimeOriginNs");
  return params;
}

export function encodeSettleParams(policy: SettlePolicy): Record<string, unknown> {
  const params: Record<string, unknown> = {};
  if (policy.persistentWork !== undefined) {
    if (policy.persistentWork !== "report" && policy.persistentWork !== "strict") {
      throw new TypeError("persistentWork must be report or strict");
    }
    params.persistentWork = policy.persistentWork;
  }
  encodeOptionalU128(params, "maxVirtualTimeNs", policy.maxVirtualTimeNs);
  encodeOptionalU128(params, "maxControlTurns", policy.maxControlTurns);
  if (policy.wallIoTimeoutNs !== undefined) {
    params.wallIoTimeoutNs = encodeU128(policy.wallIoTimeoutNs, "wallIoTimeoutNs");
  }
  return params;
}

export function encodeDocumentTargetParams(
  selector: string,
  expectedGeneration: bigint,
): Record<string, unknown> {
  if (typeof selector !== "string") throw new TypeError("selector must be a string");
  return {
    selector,
    expectedGeneration: encodeU64(expectedGeneration, "expectedGeneration"),
  };
}

export function decodeRuntimeInfo(value: unknown): RuntimeInfo {
  const result = record(value, "protocol.initialize result");
  exactKeys(result, ["protocolVersion", "implementation", "capabilities", "limits"]);
  if (result.protocolVersion !== 1) invalid("protocol.initialize returned an unsupported version");
  const implementation = record(result.implementation, "implementation");
  const capabilities = record(result.capabilities, "capabilities");
  const limits = record(result.limits, "limits");
  exactKeys(implementation, ["name", "version", "source"]);
  exactKeys(capabilities, [
    "methods",
    "clockModes",
    "profiles",
    "settlement",
    "settlementLimits",
  ]);
  exactKeys(limits, ["maxInboundFrameBytes", "maxActiveEngineRequests"]);
  requireString(implementation.name, "implementation.name");
  requireString(implementation.version, "implementation.version");
  const source = record(implementation.source, "implementation.source");
  for (const [key, sourceValue] of Object.entries(source)) {
    requireString(sourceValue, `implementation.source.${key}`);
  }
  requireStringArray(capabilities.methods, "capabilities.methods");
  requireStringArray(capabilities.clockModes, "capabilities.clockModes");
  requireStringArray(capabilities.profiles, "capabilities.profiles");
  requireStringArray(capabilities.settlementLimits, "capabilities.settlementLimits");
  if (typeof capabilities.settlement !== "boolean") invalid("capabilities.settlement must be boolean");
  requireSafeInteger(limits.maxInboundFrameBytes, "limits.maxInboundFrameBytes");
  requireSafeInteger(limits.maxActiveEngineRequests, "limits.maxActiveEngineRequests");
  return value as RuntimeInfo;
}

export interface OpenResult {
  sessionId: string;
  requestedUrl: string;
  url: string;
  boundary: "load_complete" | "controlled_ready";
  clockMode: "real" | "controlled";
}

export function decodeOpenResult(
  value: unknown,
  envelopeSessionId: string | null,
  expectedClockMode: "real" | "controlled",
): OpenResult {
  const result = record(value, "session.open result");
  exactKeys(result, ["sessionId", "requestedUrl", "url", "boundary", "clockMode"]);
  const sessionId = requireString(result.sessionId, "session.open result.sessionId");
  if (sessionId.length === 0 || sessionId !== envelopeSessionId) {
    invalid("session.open result and response envelope disagree on sessionId");
  }
  const clockMode = expectEnum(
    result.clockMode,
    new Set(["real", "controlled"]),
    "session.open result.clockMode",
  ) as "real" | "controlled";
  if (clockMode !== expectedClockMode) {
    invalid(
      `session.open requested ${expectedClockMode} clock mode but the runtime returned ${clockMode}`,
    );
  }
  return {
    sessionId,
    requestedUrl: requireString(result.requestedUrl, "session.open result.requestedUrl"),
    url: requireString(result.url, "session.open result.url"),
    boundary: expectEnum(
      result.boundary,
      new Set(["load_complete", "controlled_ready"]),
      "session.open result.boundary",
    ) as "load_complete" | "controlled_ready",
    clockMode,
  };
}

export function decodeEvaluation(value: unknown): unknown {
  const result = record(value, "dom.evaluate result");
  exactKeys(result, ["value"]);
  return result.value;
}

export function decodeText(value: unknown): string {
  const result = record(value, "dom.text result");
  exactKeys(result, ["value", "stateGeneration"]);
  const text = requireString(result.value, "dom.text result.value");
  decodeU64(result.stateGeneration, "dom.text result.stateGeneration");
  return text;
}

export function decodeActivation(value: unknown): void {
  const result = record(value, "action.activate result");
  exactKeys(result, ["stateGeneration"]);
  decodeU64(result.stateGeneration, "action.activate result.stateGeneration");
}

export function decodePending(value: unknown): PendingSnapshot {
  const decoded = decodeWideIntegers(value);
  return normalizePending(decoded);
}

function normalizePending(value: unknown): PendingSnapshot {
  const pending = record(value, "runtime.pending result");
  exactKeys(pending, [
    "stateGeneration",
    "domEpoch",
    "virtualTimeNs",
    "clock",
    "input",
    "microtasks",
    "producers",
    "timers",
    "parser",
    "network",
    "rendering",
    "sourceEpoch",
    "sources",
    "runtimeFailures",
  ]);
  normalizeClock(pending);
  requireBigInt(pending.stateGeneration, "stateGeneration");
  requireBigInt(pending.domEpoch, "domEpoch");
  requireBigInt(pending.virtualTimeNs, "virtualTimeNs");
  requireBigInt(pending.sourceEpoch, "sourceEpoch");
  validateInput(pending.input);
  validateMicrotasks(pending.microtasks);
  validateProducers(pending.producers);
  validateTimers(pending.timers);
  validateParser(pending.parser);
  validateNetwork(pending.network);
  validateRendering(pending.rendering);
  array(pending.sources, "sources").forEach(validateSource);
  array(pending.runtimeFailures, "runtimeFailures").forEach((entryValue) => {
    const entry = record(entryValue, "runtimeFailures entry");
    exactKeys(entry, ["component", "occurrences"]);
    expectEnum(entry.component, RUNTIME_FAILURE_COMPONENTS, "runtimeFailures.component");
    requireBigInt(entry.occurrences, "runtimeFailures.occurrences");
  });
  return pending as unknown as PendingSnapshot;
}

export function decodeSettle(value: unknown): SettleResult {
  const decoded = decodeWideIntegers(value);
  const result = record(decoded, "runtime.settle result");
  exactKeys(
    result,
    [
      "outcome",
      "virtualTimeNs",
      "wallTimeNs",
      "stateGeneration",
      "domEpoch",
      "effectivePolicy",
      "processed",
      "snapshot",
      "persistentWork",
      "externalIo",
      "unsupportedWork",
    ],
    ["limit", "failure"],
  );
  if (typeof result.outcome !== "string" || !SETTLE_OUTCOMES.has(result.outcome as SettleOutcome)) {
    invalid(`runtime.settle returned unknown outcome ${String(result.outcome)}`);
  }
  const snapshot = normalizePending(result.snapshot);
  result.snapshot = snapshot;
  requireBigInt(result.virtualTimeNs, "virtualTimeNs");
  requireBigInt(result.wallTimeNs, "wallTimeNs");
  requireBigInt(result.stateGeneration, "stateGeneration");
  requireBigInt(result.domEpoch, "domEpoch");
  if (
    result.virtualTimeNs !== snapshot.virtualTimeNs ||
    result.stateGeneration !== snapshot.stateGeneration ||
    result.domEpoch !== snapshot.domEpoch
  ) {
    invalid("runtime.settle summary fields disagree with snapshot");
  }
  const policy = record(result.effectivePolicy, "effectivePolicy");
  exactKeys(policy, [
    "persistentWork",
    "maxVirtualTimeNs",
    "maxControlTurns",
    "wallIoTimeoutNs",
  ]);
  if (policy.persistentWork !== "report" && policy.persistentWork !== "strict") {
    invalid("effectivePolicy.persistentWork is invalid");
  }
  requireBigInt(policy.maxVirtualTimeNs, "effectivePolicy.maxVirtualTimeNs");
  requireBigInt(policy.maxControlTurns, "effectivePolicy.maxControlTurns");
  requireBigInt(policy.wallIoTimeoutNs, "effectivePolicy.wallIoTimeoutNs");
  result.effectivePolicy = policy as unknown as EffectiveSettlePolicy;
  const processed = record(result.processed, "processed");
  exactKeys(processed, ["controlTurns"]);
  requireBigInt(processed.controlTurns, "processed.controlTurns");
  validateClassifiedWork(result.persistentWork, "persistentWork", true);
  validateClassifiedWork(result.unsupportedWork, "unsupportedWork", false);
  array(result.externalIo, "externalIo").forEach(validateExternalIo);
  validateSettleOutcomePayload(result);
  return result as unknown as SettleResult;
}

export function decodeAdvanceToNext(value: unknown): AdvanceToNextResult {
  const decoded = decodeWideIntegers(value);
  const result = record(decoded, "runtime.advance_to_next result");
  const snapshot = normalizePending(result.snapshot);
  result.snapshot = snapshot;
  requireBigInt(result.virtualTimeNs, "virtualTimeNs");
  requireBigInt(result.stateGeneration, "stateGeneration");
  if (
    result.virtualTimeNs !== snapshot.virtualTimeNs ||
    result.stateGeneration !== snapshot.stateGeneration
  ) {
    invalid("runtime.advance_to_next summary fields disagree with snapshot");
  }
  if (result.outcome === "advanced") {
    exactKeys(result, [
      "outcome",
      "fromVirtualTimeNs",
      "virtualTimeNs",
      "stateGeneration",
      "snapshot",
    ]);
    requireBigInt(result.fromVirtualTimeNs, "fromVirtualTimeNs");
    return result as unknown as AdvanceToNextResult;
  }
  if (result.outcome === "no_finite_deadline") {
    exactKeys(result, ["outcome", "virtualTimeNs", "stateGeneration", "snapshot"]);
    return result as unknown as AdvanceToNextResult;
  }
  invalid(`runtime.advance_to_next returned unknown outcome ${String(result.outcome)}`);
}

export function decodeClose(value: unknown): void {
  const result = record(value, "session.close result");
  exactKeys(result, ["state"]);
  if (result.state !== "closed") invalid("session.close did not return the closed state");
}

function normalizeClock(pending: Record<string, unknown>): void {
  const clock = record(pending.clock, "clock");
  if (clock.unsupportedSurfaces !== undefined && !Array.isArray(clock.unsupportedSurfaces)) {
    invalid("clock.unsupportedSurfaces must be an array when present");
  }
  if (Array.isArray(clock.unsupportedSurfaces)) {
    requireStringArray(clock.unsupportedSurfaces, "clock.unsupportedSurfaces");
  } else if (typeof clock.unsupportedSurface === "string") {
    clock.unsupportedSurfaces = [clock.unsupportedSurface];
  } else if (clock.unsupportedSurface === undefined) {
    clock.unsupportedSurfaces = [];
  } else {
    invalid("clock.unsupportedSurface must be a string when present");
  }
  delete clock.unsupportedSurface;
  exactKeys(clock, ["mode", "unsupportedSurfaces"]);
  expectEnum(clock.mode, new Set(["real", "controlled"]), "clock.mode");
  for (const surface of clock.unsupportedSurfaces as string[]) {
    expectEnum(surface, TIME_SURFACES, "clock.unsupportedSurfaces entry");
  }
}

function validateInput(value: unknown): void {
  const input = record(value, "input");
  exactKeys(input, ["readyEvents", "intakeSaturated", "tasks"]);
  requireBigInt(input.readyEvents, "input.readyEvents");
  requireBoolean(input.intakeSaturated, "input.intakeSaturated");
  const tasks = record(input.tasks, "input.tasks");
  exactKeys(tasks, ["ready", "throttled", "inactive"]);
  requireBigInt(tasks.ready, "input.tasks.ready");
  requireBigInt(tasks.throttled, "input.tasks.throttled");
  requireBigInt(tasks.inactive, "input.tasks.inactive");
}

function validateMicrotasks(value: unknown): void {
  const microtasks = record(value, "microtasks");
  exactKeys(microtasks, ["queued", "checkpointInProgress", "terminal"]);
  requireBigInt(microtasks.queued, "microtasks.queued");
  requireBoolean(microtasks.checkpointInProgress, "microtasks.checkpointInProgress");
  requireBoolean(microtasks.terminal, "microtasks.terminal");
}

function validateProducers(value: unknown): void {
  const producers = record(value, "producers");
  exactKeys(producers, ["pending", "stability", "terminal"]);
  requireBigInt(producers.pending, "producers.pending");
  expectEnum(producers.stability, PRODUCER_STABILITIES, "producers.stability");
  requireBoolean(producers.terminal, "producers.terminal");
}

function validateTimers(value: unknown): void {
  const timers = record(value, "timers");
  exactKeys(timers, ["ready", "futureFinite", "persistent", "unsupported"], ["nextDeadlineNs"]);
  requireBigInt(timers.ready, "timers.ready");
  requireBigInt(timers.futureFinite, "timers.futureFinite");
  requireBigInt(timers.persistent, "timers.persistent");
  requireBigInt(timers.unsupported, "timers.unsupported");
  if (timers.nextDeadlineNs !== undefined) {
    requireBigInt(timers.nextDeadlineNs, "timers.nextDeadlineNs");
  }
}

function validateParser(value: unknown): void {
  const parser = record(value, "parser");
  exactKeys(parser, [
    "total",
    "ready",
    "awaitingExternalIo",
    "awaitingCommit",
    "awaitingScriptInput",
    "suspended",
  ]);
  for (const key of [
    "total",
    "ready",
    "awaitingExternalIo",
    "awaitingCommit",
    "awaitingScriptInput",
    "suspended",
  ]) {
    requireBigInt(parser[key], `parser.${key}`);
  }
}

function validateNetwork(value: unknown): void {
  const network = record(value, "network");
  exactKeys(network, ["counts", "active"]);
  const counts = record(network.counts, "network.counts");
  const countKeys = [
    "navigation",
    "fetch",
    "xmlHttpRequest",
    "image",
    "font",
    "stylesheet",
    "script",
    "unclassifiedProducerIo",
    "other",
  ];
  exactKeys(counts, countKeys);
  for (const key of countKeys) requireBigInt(counts[key], `network.counts.${key}`);
  array(network.active, "network.active").forEach(validateExternalIo);
}

function validateExternalIo(value: unknown): void {
  const operation = record(value, "externalIo entry");
  exactKeys(operation, ["sourceId", "kind", "phase", "owner", "loadBlocking", "startedAtNs"]);
  requireOpaqueId(operation.sourceId, "externalIo.sourceId");
  expectEnum(operation.kind, NETWORK_KINDS, "externalIo.kind");
  expectEnum(operation.phase, EXTERNAL_IO_PHASES, "externalIo.phase");
  expectEnum(operation.owner, EXTERNAL_IO_OWNERS, "externalIo.owner");
  expectEnum(operation.loadBlocking, LOAD_BLOCKING_VALUES, "externalIo.loadBlocking");
  requireBigInt(operation.startedAtNs, "externalIo.startedAtNs");
}

function validateRendering(value: unknown): void {
  const rendering = record(value, "rendering");
  const bigintKeys = [
    "retainedAnimationFrames",
    "runnableAnimationFrames",
    "pendingAnimationEvents",
    "finiteAnimations",
    "persistentAnimations",
    "unsupportedAnimations",
    "finiteAnimatedImages",
    "persistentAnimatedImages",
    "unsupportedAnimatedImages",
    "dirtyCanvases",
    "unsupportedCanvases",
    "pendingFonts",
    "pendingImages",
  ];
  const booleanKeys = [
    "opportunityReady",
    "updateRequired",
    "imageUpdateReady",
    "canvasUploadPending",
  ];
  exactKeys(rendering, [...bigintKeys, ...booleanKeys], ["nextOpportunityNs"]);
  for (const key of bigintKeys) requireBigInt(rendering[key], `rendering.${key}`);
  for (const key of booleanKeys) requireBoolean(rendering[key], `rendering.${key}`);
  if (rendering.nextOpportunityNs !== undefined) {
    requireBigInt(rendering.nextOpportunityNs, "rendering.nextOpportunityNs");
  }
}

function validateSource(value: unknown): void {
  const source = record(value, "sources entry");
  requireOpaqueId(source.sourceId, "sources.sourceId");
  expectEnum(source.kind, SOURCE_KINDS, "sources.kind");
  switch (source.state) {
    case "inert":
    case "ready":
    case "finite_rendering_opportunity":
      exactKeys(source, ["sourceId", "kind", "state"]);
      return;
    case "finite_deadline":
      exactKeys(source, ["sourceId", "kind", "state", "deadlineNs"]);
      requireBigInt(source.deadlineNs, "sources.deadlineNs");
      return;
    case "awaiting_external_io":
      exactKeys(source, ["sourceId", "kind", "state", "owner", "loadBlocking"]);
      expectEnum(source.owner, EXTERNAL_IO_OWNERS, "sources.owner");
      expectEnum(source.loadBlocking, LOAD_BLOCKING_VALUES, "sources.loadBlocking");
      return;
    case "open_ended": {
      exactKeys(source, ["sourceId", "kind", "state", "openEnded"]);
      const description = record(source.openEnded, "sources.openEnded");
      exactKeys(description, ["reason"], ["requestedPeriodNs"]);
      expectEnum(description.reason, OPEN_ENDED_REASONS, "sources.openEnded.reason");
      if (description.requestedPeriodNs !== undefined) {
        requireBigInt(description.requestedPeriodNs, "sources.openEnded.requestedPeriodNs");
      }
      return;
    }
    case "unsupported": {
      exactKeys(source, ["sourceId", "kind", "state", "unsupported"]);
      validateUnsupportedDescription(source.unsupported, "sources.unsupported");
      return;
    }
    default:
      invalid(`sources.state is invalid: ${String(source.state)}`);
  }
}

function validateUnsupportedDescription(value: unknown, label: string): void {
  const description = record(value, label);
  exactKeys(description, ["reason"], ["timeSurface"]);
  expectEnum(description.reason, UNSUPPORTED_REASONS, `${label}.reason`);
  if (description.timeSurface !== undefined) {
    expectEnum(description.timeSurface, TIME_SURFACES, `${label}.timeSurface`);
  }
}

function validateClassifiedWork(value: unknown, label: string, persistent: boolean): void {
  for (const entryValue of array(value, label)) {
    const entry = record(entryValue, `${label} entry`);
    exactKeys(
      entry,
      ["kind", "count", "reason"],
      ["sourceId", "requestedPeriodNs", "timeSurface"],
    );
    expectEnum(entry.kind, SOURCE_KINDS, `${label}.kind`);
    requireBigInt(entry.count, `${label}.count`);
    if (entry.sourceId !== undefined) requireOpaqueId(entry.sourceId, `${label}.sourceId`);
    if (persistent) {
      expectEnum(entry.reason, PERSISTENT_REASONS, `${label}.reason`);
      if (entry.requestedPeriodNs !== undefined) {
        requireBigInt(entry.requestedPeriodNs, `${label}.requestedPeriodNs`);
      }
      if (entry.timeSurface !== undefined) invalid(`${label} must not contain timeSurface`);
    } else {
      expectEnum(entry.reason, UNSUPPORTED_REASONS, `${label}.reason`);
      if (entry.timeSurface !== undefined) {
        expectEnum(entry.timeSurface, TIME_SURFACES, `${label}.timeSurface`);
      }
      if (entry.requestedPeriodNs !== undefined) {
        invalid(`${label} must not contain requestedPeriodNs`);
      }
    }
  }
}

function validateSettleOutcomePayload(result: Record<string, unknown>): void {
  const outcome = result.outcome as SettleOutcome;
  if (outcome === "virtual_time_limit_exceeded") {
    if (result.failure !== undefined) invalid(`${outcome} must omit failure`);
    const limit = record(result.limit, "limit");
    exactKeys(limit, ["kind", "limit", "startVirtualTimeNs", "requestedVirtualTimeNs"]);
    if (limit.kind !== "virtual_time") invalid(`${outcome} requires a virtual_time limit`);
    requireBigInt(limit.limit, "limit.limit");
    requireBigInt(limit.startVirtualTimeNs, "limit.startVirtualTimeNs");
    requireBigInt(limit.requestedVirtualTimeNs, "limit.requestedVirtualTimeNs");
    return;
  }
  if (outcome === "control_turn_limit_exceeded") {
    if (result.failure !== undefined) invalid(`${outcome} must omit failure`);
    const limit = record(result.limit, "limit");
    exactKeys(limit, ["kind", "limit"]);
    if (limit.kind !== "control_turns") invalid(`${outcome} requires a control_turns limit`);
    requireBigInt(limit.limit, "limit.limit");
    return;
  }
  if (outcome === "unsupported_work" || outcome === "runtime_error") {
    if (result.limit !== undefined) invalid(`${outcome} must omit limit`);
    const failure = record(result.failure, "failure");
    exactKeys(failure, ["code"]);
    expectEnum(failure.code, SETTLE_FAILURE_CODES, "failure.code");
    return;
  }
  if (result.limit !== undefined || result.failure !== undefined) {
    invalid(`${outcome} must omit limit and failure`);
  }
}

function decodeWideIntegers(value: unknown, propertyName?: string): unknown {
  if (Array.isArray(value)) return value.map((entry) => decodeWideIntegers(entry));
  if (typeof value === "object" && value !== null) {
    const output: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(value)) {
      output[key] = decodeWideIntegers(entry, key);
    }
    return output;
  }
  if (propertyName !== undefined && WIDE_INTEGER_FIELDS.has(propertyName)) {
    return decodeU128(value, propertyName);
  }
  return value;
}

function encodeOptionalU128(
  destination: Record<string, unknown>,
  key: string,
  value: bigint | undefined,
): void {
  if (value !== undefined) destination[key] = encodeU128(value, key);
}

function encodeU128(value: bigint, label: string): string {
  if (typeof value !== "bigint" || value < 0n || value > MAX_U128) {
    throw new RangeError(`${label} must be a bigint in the u128 range`);
  }
  return value.toString();
}

function encodeU64(value: bigint, label: string): string {
  if (typeof value !== "bigint" || value < 0n || value > MAX_U64) {
    throw new RangeError(`${label} must be a bigint in the u64 range`);
  }
  return value.toString();
}

function decodeU64(value: unknown, label: string): bigint {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/u.test(value)) {
    invalid(`${label} must be a canonical decimal string`);
  }
  const parsed = BigInt(value);
  if (parsed > MAX_U64) invalid(`${label} exceeds u64`);
  return parsed;
}

function decodeU128(value: unknown, label: string): bigint {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/u.test(value)) {
    invalid(`${label} must be a canonical decimal string`);
  }
  const parsed = BigInt(value);
  if (parsed > MAX_U128) invalid(`${label} exceeds u128`);
  return parsed;
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    invalid(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function requireString(value: unknown, label: string): string {
  if (typeof value !== "string") invalid(`${label} must be a string`);
  return value;
}

function requireStringArray(value: unknown, label: string): string[] {
  if (!Array.isArray(value) || value.some((entry) => typeof entry !== "string")) {
    invalid(`${label} must be an array of strings`);
  }
  return value as string[];
}

function requireSafeInteger(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    invalid(`${label} must be a non-negative safe integer`);
  }
  return value;
}

function requireBigInt(value: unknown, label: string): bigint {
  if (typeof value !== "bigint") invalid(`${label} must be an exact integer`);
  return value;
}

function requireBoolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") invalid(`${label} must be a boolean`);
  return value;
}

function requireOpaqueId(value: unknown, label: string): string {
  const id = requireString(value, label);
  if (!/^(0|[1-9][0-9]*)$/u.test(id)) {
    invalid(`${label} must be a canonical decimal string`);
  }
  if (BigInt(id) > MAX_U128) invalid(`${label} exceeds u128`);
  return id;
}

function array(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) invalid(`${label} must be an array`);
  return value;
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[] = [],
): void {
  for (const key of required) {
    if (!Object.hasOwn(value, key)) invalid(`result is missing required field ${key}`);
  }
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) invalid(`result contains unexpected field ${key}`);
  }
}

function expectEnum(value: unknown, allowed: ReadonlySet<string>, label: string): string {
  if (typeof value !== "string" || !allowed.has(value)) {
    invalid(`${label} has unknown value ${String(value)}`);
  }
  return value;
}

function stringSet(values: readonly string[]): ReadonlySet<string> {
  return new Set(values);
}

function invalid(message: string): never {
  throw new StasisTransportError("invalid_result", message);
}
