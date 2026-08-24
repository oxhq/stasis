import { StasisTransportError } from "./errors.js";
import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  type LegacySupportProfile,
} from "./profile.js";
import type {
  AdvanceToNextResult,
  AutomationMutationResult,
  ClockOptions,
  ControlledClockOptions,
  DocumentStateToken,
  EffectiveSettlePolicy,
  ExtractPlan,
  ExtractResult,
  NetworkRoute,
  PendingSnapshot,
  QueryResult,
  RuntimeInfo,
  SessionAdvanceToNextResult,
  SessionAuditOptions,
  SessionAutomationMutationResult,
  SessionCookie,
  SessionCookiesResult,
  SessionEvidenceEvent,
  SessionEvidenceFailureReason,
  SessionEvidenceResult,
  SessionExtractPlan,
  SessionExtractResult,
  SessionFocusResult,
  SessionCheckResult,
  SessionNavigateResult,
  SessionNetworkOptions,
  SessionOpenOptions,
  SessionOriginState,
  SessionPendingSnapshot,
  SessionQueryResult,
  SessionRequestRecord,
  SessionRequestsResult,
  SessionSelectResult,
  SessionSettleResult,
  SessionState,
  SessionStateExportResult,
  SessionStateMutationResult,
  SessionStateToken,
  SessionStorageResult,
  SessionSubmitResult,
  SessionTextResult,
  SettleOutcome,
  SettlePolicy,
  SettleResult,
} from "./types.js";

const MAX_U128 = (1n << 128n) - 1n;
const MAX_U64 = (1n << 64n) - 1n;
const MAX_SELECT_VALUES = 16;
const MAX_SELECT_VALUE_BYTES = 128 * 1024;
const MAX_NETWORK_FIXTURE_ENCODED_TABLE_BYTES = 384 * 1024;
const MAX_SESSION_STATE_BYTES = 512 * 1024;
const MAX_SESSION_COOKIES = 512;
const MAX_SESSION_COOKIE_BYTES = 4 * 1024;
const MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES = 250 * 1024;
const MAX_SESSION_STORAGE_ORIGINS = 64;
const MAX_SESSION_STORAGE_ENTRIES_PER_AREA = 1024;
const MAX_SESSION_STORAGE_KEY_BYTES = 4 * 1024;
const MAX_SESSION_STORAGE_VALUE_BYTES = 128 * 1024;
const MAX_SESSION_STORAGE_BYTES_PER_ORIGIN = 512 * 1024;
const MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES = 250 * 1024;
const MAX_OPAQUE_TOKEN_BYTES = 256;
const COOKIE_NAME_TOKEN_PUNCTUATION = new Set([
  0x21, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2a, 0x2b, 0x2d, 0x2e, 0x5e, 0x5f,
  0x60, 0x7c, 0x7e,
]);

/** All product/server spelling is confined to this module. */
export const METHOD = {
  initialize: "protocol.initialize",
  open: "session.open",
  evaluate: "dom.evaluate",
  query: "dom.query",
  text: "dom.text",
  extract: "dom.extract",
  fill: "action.fill",
  activate: "action.activate",
  focus: "action.focus",
  check: "action.check",
  uncheck: "action.uncheck",
  select: "action.select",
  submit: "action.submit",
  pending: "runtime.pending",
  settle: "runtime.settle",
  advanceToNext: "runtime.advance_to_next",
  navigate: "session.navigate",
  exportState: "session.state.export",
  importState: "session.state.import",
  getCookies: "session.cookies.get",
  setCookies: "session.cookies.set",
  getStorage: "session.storage.get",
  setStorage: "session.storage.set",
  requests: "session.requests",
  evidence: "session.evidence",
  close: "session.close",
} as const;

const SETTLE_OUTCOMES = new Set<SettleOutcome>([
  "quiescent",
  "quiescent_with_persistent_work",
  "blocked_on_external_io",
  "blocked_on_open_ended_work",
  "unsupported_work",
  "virtual_time_limit_exceeded",
  "task_limit_exceeded",
  "microtask_limit_exceeded",
  "rendering_limit_exceeded",
  "mutation_limit_exceeded",
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
  "history_traversal",
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
  "execution_counter_overflow",
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
const SESSION_EVIDENCE_FAILURE_REASONS = stringSet([
  "blocked_by_fixture",
  "fixture_miss",
  "cancelled",
  "connection_reset",
  "network_error",
  "navigation_error",
  "document_transition_limit_exceeded",
  "redirect_limit_exceeded",
  "history_limit_exceeded",
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
  "tasks",
  "microtasks",
  "renderingOpportunities",
  "mutations",
  "count",
  "fromVirtualTimeNs",
  "limit",
  "observed",
  "startVirtualTimeNs",
  "requestedVirtualTimeNs",
]);

export function encodeOpenParams(
  url: string | URL,
  clock: ClockOptions | undefined,
  profile: LegacySupportProfile | undefined,
): Record<string, unknown> {
  const serializedUrl = typeof url === "string" ? url : url.toString();
  if (serializedUrl.length === 0) throw new TypeError("url must not be empty");
  const params: Record<string, unknown> = { url: serializedUrl };
  if (clock?.mode === "real") {
    if (profile !== undefined) {
      throw new TypeError("profile is supported only with the controlled clock");
    }
    params.clockMode = "real";
    return params;
  }
  if (clock !== undefined && clock.mode !== "controlled") {
    throw new TypeError("clock.mode must be real or controlled");
  }
  if (profile !== undefined && profile !== CONTROLLED_WEBAPP_V1_PROFILE) {
    throw new TypeError(`profile must be ${CONTROLLED_WEBAPP_V1_PROFILE}`);
  }
  params.clockMode = "controlled";
  params.profile = profile ?? CONTROLLED_WEBAPP_V1_PROFILE;
  params.initialVirtualTimeNs = encodeU128(
    clock?.initialVirtualTimeNs ?? 0n,
    "initialVirtualTimeNs",
  );
  const unixTimeOriginNs = clock?.unixTimeOriginNs ?? 0n;
  if (unixTimeOriginNs !== 0n) {
    throw new RangeError("unixTimeOriginNs must be 0 in the controlled MVP");
  }
  params.unixTimeOriginNs = encodeU128(unixTimeOriginNs, "unixTimeOriginNs");
  return params;
}

export function encodeSessionOpenParams(
  url: string | URL,
  options: SessionOpenOptions,
): Record<string, unknown> {
  const serializedUrl = typeof url === "string" ? url : url.toString();
  if (serializedUrl.length === 0) throw new TypeError("url must not be empty");
  const clock = options.clock;
  if (clock !== undefined && clock.mode !== "controlled") {
    throw new TypeError("Runtime.openSession() requires the controlled clock");
  }
  const params: Record<string, unknown> = {
    url: serializedUrl,
    clockMode: "controlled",
    profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
    initialVirtualTimeNs: encodeU128(
      clock?.initialVirtualTimeNs ?? 0n,
      "initialVirtualTimeNs",
    ),
  };
  const unixTimeOriginNs = clock?.unixTimeOriginNs ?? 0n;
  if (unixTimeOriginNs !== 0n) {
    throw new RangeError("unixTimeOriginNs must be 0 in the controlled MVP");
  }
  params.unixTimeOriginNs = encodeU128(unixTimeOriginNs, "unixTimeOriginNs");
  if (options.state !== undefined) params.state = encodeSessionState(options.state);
  if (options.network !== undefined) params.network = encodeSessionNetwork(options.network);
  return params;
}

function encodeSessionNetwork(networkValue: SessionNetworkOptions): Record<string, unknown> {
  const network = inputRecord(networkValue, "network");
  inputExactKeys(network, ["mode", "routes"], "network");
  if (network.mode !== "fixtures_only" && network.mode !== "mixed" && network.mode !== "live") {
    throw new TypeError("network.mode must be fixtures_only, mixed, or live");
  }
  if (!Array.isArray(network.routes)) throw new TypeError("network.routes must be an array");
  const encoded = {
    mode: network.mode,
    routes: network.routes.map((route, index) => encodeNetworkRoute(route, index)),
  };
  const encodedBytes = new TextEncoder().encode(JSON.stringify(encoded)).byteLength;
  if (encodedBytes > MAX_NETWORK_FIXTURE_ENCODED_TABLE_BYTES) {
    throw new RangeError(
      `network must encode to at most ${MAX_NETWORK_FIXTURE_ENCODED_TABLE_BYTES} UTF-8 bytes`,
    );
  }
  return encoded;
}

function encodeNetworkRoute(routeValue: NetworkRoute, index: number): Record<string, unknown> {
  const label = `network.routes[${index}]`;
  const route = inputRecord(routeValue, label);
  const hasFulfill = Object.hasOwn(route, "fulfill") && route.fulfill !== undefined;
  const hasAbort = Object.hasOwn(route, "abort") && route.abort !== undefined;
  if (hasFulfill === hasAbort) {
    throw new TypeError(`${label} must contain exactly one of fulfill or abort`);
  }
  inputExactKeys(route, ["match", hasFulfill ? "fulfill" : "abort"], label);
  const match = inputRecord(route.match, `${label}.match`);
  inputExactKeys(match, ["method", "url"], `${label}.match`);
  const method = inputString(match.method, `${label}.match.method`);
  if (method.length === 0) throw new TypeError(`${label}.match.method must not be empty`);
  const url = inputRecord(match.url, `${label}.match.url`);
  const matcherKeys = ["exact", "prefix", "glob"].filter(
    (key) => Object.hasOwn(url, key) && url[key] !== undefined,
  );
  if (matcherKeys.length !== 1) {
    throw new TypeError(`${label}.match.url must contain exactly one of exact, prefix, or glob`);
  }
  inputExactKeys(url, matcherKeys, `${label}.match.url`);
  const matcherKey = matcherKeys[0]!;
  const matcher = inputString(url[matcherKey], `${label}.match.url.${matcherKey}`);
  if (matcher.length === 0) {
    throw new TypeError(`${label}.match.url.${matcherKey} must not be empty`);
  }
  const encoded: Record<string, unknown> = {
    match: { method, url: { [matcherKey]: matcher } },
  };
  if (hasAbort) {
    const abort = inputRecord(route.abort, `${label}.abort`);
    inputExactKeys(abort, ["reason"], `${label}.abort`);
    const reason = inputString(abort.reason, `${label}.abort.reason`);
    if (!new Set(["blocked_by_fixture", "connection_reset", "network_error"]).has(reason)) {
      throw new TypeError(`${label}.abort.reason is invalid`);
    }
    encoded.abort = { reason };
    return encoded;
  }
  const fulfill = inputRecord(route.fulfill, `${label}.fulfill`);
  inputExactKeys(fulfill, ["status"], `${label}.fulfill`, ["headers", "body"]);
  if (
    typeof fulfill.status !== "number" ||
    !Number.isSafeInteger(fulfill.status) ||
    fulfill.status < 200 ||
    fulfill.status > 599
  ) {
    throw new RangeError(`${label}.fulfill.status must be an integer between 200 and 599`);
  }
  const encodedFulfill: Record<string, unknown> = { status: fulfill.status };
  if (fulfill.headers !== undefined) {
    if (!Array.isArray(fulfill.headers)) {
      throw new TypeError(`${label}.fulfill.headers must be an array`);
    }
    encodedFulfill.headers = fulfill.headers.map((header, headerIndex) => {
      if (!Array.isArray(header) || header.length !== 2) {
        throw new TypeError(`${label}.fulfill.headers[${headerIndex}] must be a pair`);
      }
      return [
        inputString(header[0], `${label}.fulfill.headers[${headerIndex}][0]`),
        inputString(header[1], `${label}.fulfill.headers[${headerIndex}][1]`),
      ];
    });
  }
  if (fulfill.body !== undefined) {
    const body = inputRecord(fulfill.body, `${label}.fulfill.body`);
    const bodyKeys = ["utf8", "base64"].filter(
      (key) => Object.hasOwn(body, key) && body[key] !== undefined,
    );
    if (bodyKeys.length !== 1) {
      throw new TypeError(`${label}.fulfill.body must contain exactly one of utf8 or base64`);
    }
    inputExactKeys(body, bodyKeys, `${label}.fulfill.body`);
    const bodyKey = bodyKeys[0]!;
    const bodyValue = inputString(body[bodyKey], `${label}.fulfill.body.${bodyKey}`);
    if (bodyKey === "base64" && !isCanonicalBase64(bodyValue)) {
      throw new TypeError(`${label}.fulfill.body.base64 must be canonical base64`);
    }
    encodedFulfill.body = { [bodyKey]: bodyValue };
  }
  encoded.fulfill = encodedFulfill;
  return encoded;
}

export function encodeSessionState(stateValue: SessionState): Record<string, unknown> {
  const state = inputRecord(stateValue, "state");
  inputExactKeysSecretSafe(
    state,
    [
      "schemaVersion",
      "profile",
      "sensitive",
      "sessionStorageScope",
      "cookies",
      "origins",
    ],
    "state",
  );
  if (state.schemaVersion !== 1) throw new TypeError("state.schemaVersion must be 1");
  if (state.profile !== CONTROLLED_WEB_SESSION_V1_PROFILE) {
    throw new TypeError(`state.profile must be ${CONTROLLED_WEB_SESSION_V1_PROFILE}`);
  }
  if (state.sensitive !== true) throw new TypeError("state.sensitive must be true");
  if (state.sessionStorageScope !== "top_level_browsing_context") {
    throw new TypeError(
      "state.sessionStorageScope must be top_level_browsing_context",
    );
  }
  const cookies = encodeSessionCookieArray(state.cookies, "state.cookies");
  const origins = encodeSessionOriginArray(state.origins, "state.origins");
  const encoded = {
    schemaVersion: 1,
    profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
    sensitive: true,
    sessionStorageScope: "top_level_browsing_context",
    cookies,
    origins,
  };
  assertCompactJsonBytes(encoded, MAX_SESSION_STATE_BYTES, "state");
  return encoded;
}

function encodeSessionCookieArray(
  cookiesValue: unknown,
  label: string,
): Record<string, unknown>[] {
  const cookies = inputBoundedArray(cookiesValue, label, MAX_SESSION_COOKIES);
  const encoded: Record<string, unknown>[] = [];
  const identities = new Set<string>();
  const creationSequences = new Set<string>();
  const accessSequences = new Set<string>();
  let encodedBytes = 2;
  for (let index = 0; index < cookies.length; index += 1) {
    const cookie = encodeSessionCookie(cookies[index], index, label);
    const identity = JSON.stringify([cookie.domain, cookie.path, cookie.name]);
    if (identities.has(identity)) {
      throw new TypeError(`${label} must not contain duplicate cookie identities`);
    }
    identities.add(identity);
    const creationSequence = cookie.creationSequence as string;
    if (creationSequences.has(creationSequence)) {
      throw new TypeError(`${label} must not contain duplicate creation sequences`);
    }
    creationSequences.add(creationSequence);
    const accessSequence = cookie.lastAccessSequence as string;
    if (accessSequences.has(accessSequence)) {
      throw new TypeError(`${label} must not contain duplicate last-access sequences`);
    }
    accessSequences.add(accessSequence);
    encodedBytes = addCompactArrayItemBytes(
      encodedBytes,
      cookie,
      index,
      MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES,
      label,
    );
    encoded.push(cookie);
  }
  return encoded;
}

function encodeSessionCookie(
  cookieValue: unknown,
  index: number,
  parentLabel: string,
): Record<string, unknown> {
  const label = `${parentLabel}[${index}]`;
  const cookie = inputRecord(cookieValue, label);
  inputExactKeysSecretSafe(
    cookie,
    [
      "name",
      "value",
      "domain",
      "path",
      "hostOnly",
      "secure",
      "httpOnly",
      "sameSite",
      "expiresUnixTimeNs",
      "partitioned",
      "creationSequence",
      "lastAccessSequence",
    ],
    label,
  );
  const name = inputBoundedUtf8String(cookie.name, `${label}.name`, MAX_SESSION_COOKIE_BYTES);
  const value = inputBoundedUtf8String(cookie.value, `${label}.value`, MAX_SESSION_COOKIE_BYTES);
  const domain = inputBoundedUtf8String(
    cookie.domain,
    `${label}.domain`,
    MAX_SESSION_COOKIE_BYTES,
  );
  const path = inputBoundedUtf8String(cookie.path, `${label}.path`, MAX_SESSION_COOKIE_BYTES);
  if (name.length + value.length + domain.length + path.length > MAX_SESSION_COOKIE_BYTES) {
    throw new RangeError(`${label} string fields must total at most ${MAX_SESSION_COOKIE_BYTES} UTF-8 bytes`);
  }
  const cookieBytes =
    Buffer.byteLength(name, "utf8") +
    Buffer.byteLength(value, "utf8") +
    Buffer.byteLength(domain, "utf8") +
    Buffer.byteLength(path, "utf8");
  if (cookieBytes > MAX_SESSION_COOKIE_BYTES) {
    throw new RangeError(`${label} string fields must total at most ${MAX_SESSION_COOKIE_BYTES} UTF-8 bytes`);
  }
  if (!isValidCookieNameAndValue(name, value)) {
    throw new TypeError(`${label} name and value must use the RFC 6265 cookie wire shape`);
  }
  if (!isCanonicalCookieDomain(domain)) {
    throw new TypeError(`${label}.domain must be a non-empty canonical host`);
  }
  if (!path.startsWith("/")) {
    throw new TypeError(`${label}.path must start with /`);
  }
  const sameSite = inputString(cookie.sameSite, `${label}.sameSite`);
  if (!new Set(["unspecified", "strict", "lax", "none"]).has(sameSite)) {
    throw new TypeError(`${label}.sameSite is invalid`);
  }
  if (cookie.expiresUnixTimeNs !== null) {
    throw new TypeError(`${label}.expiresUnixTimeNs must be null in session state v1`);
  }
  if (cookie.partitioned !== false) {
    throw new TypeError(`${label}.partitioned must be false in session state v1`);
  }
  const secure = inputBoolean(cookie.secure, `${label}.secure`);
  if (sameSite === "none" && !secure) {
    throw new TypeError(`${label} SameSite=None cookies must be secure`);
  }
  const hostOnly = inputBoolean(cookie.hostOnly, `${label}.hostOnly`);
  if (name.toLowerCase().startsWith("__secure-") && !secure) {
    throw new TypeError(`${label} __Secure- cookies must be secure`);
  }
  if (
    name.toLowerCase().startsWith("__host-") &&
    (!secure || !hostOnly || path !== "/")
  ) {
    throw new TypeError(`${label} __Host- cookies must be secure, host-only, and use path /`);
  }
  return {
    name,
    value,
    domain,
    path,
    hostOnly,
    secure,
    httpOnly: inputBoolean(cookie.httpOnly, `${label}.httpOnly`),
    sameSite,
    expiresUnixTimeNs: null,
    partitioned: false,
    creationSequence: encodeU64(
      cookie.creationSequence as bigint,
      `${label}.creationSequence`,
    ),
    lastAccessSequence: encodeU64(
      cookie.lastAccessSequence as bigint,
      `${label}.lastAccessSequence`,
    ),
  };
}

function encodeSessionOriginArray(
  originsValue: unknown,
  label: string,
): Record<string, unknown>[] {
  const origins = inputBoundedArray(originsValue, label, MAX_SESSION_STORAGE_ORIGINS);
  const encoded: Record<string, unknown>[] = [];
  const canonicalOrigins = new Set<string>();
  let encodedBytes = 2;
  for (let index = 0; index < origins.length; index += 1) {
    const origin = encodeSessionOrigin(origins[index], index, label);
    const canonicalOrigin = origin.origin as string;
    if (canonicalOrigins.has(canonicalOrigin)) {
      throw new TypeError(`${label} must not contain duplicate origins`);
    }
    canonicalOrigins.add(canonicalOrigin);
    encodedBytes = addCompactArrayItemBytes(
      encodedBytes,
      origin,
      index,
      MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES,
      label,
    );
    encoded.push(origin);
  }
  return encoded;
}

function encodeSessionOrigin(
  originValue: unknown,
  index: number,
  parentLabel: string,
): Record<string, unknown> {
  const label = `${parentLabel}[${index}]`;
  const origin = inputRecord(originValue, label);
  inputExactKeysSecretSafe(origin, ["origin", "localStorage", "sessionStorage"], label);
  const canonicalOrigin = inputCanonicalOrigin(origin.origin, `${label}.origin`);
  let originBytes = Buffer.byteLength(canonicalOrigin, "utf8");
  const encodeEntries = (valuesValue: unknown, storageLabel: string) => {
    const values = inputBoundedArray(
      valuesValue,
      storageLabel,
      MAX_SESSION_STORAGE_ENTRIES_PER_AREA,
    );
    const entries: Record<string, string>[] = [];
    const keys = new Set<string>();
    for (let entryIndex = 0; entryIndex < values.length; entryIndex += 1) {
      const entryValue = values[entryIndex];
      const entryLabel = `${storageLabel}[${entryIndex}]`;
      const entry = inputRecord(entryValue, entryLabel);
      inputExactKeysSecretSafe(entry, ["key", "value"], entryLabel);
      const key = inputBoundedUtf8String(
        entry.key,
        `${entryLabel}.key`,
        MAX_SESSION_STORAGE_KEY_BYTES,
      );
      const value = inputBoundedUtf8String(
        entry.value,
        `${entryLabel}.value`,
        MAX_SESSION_STORAGE_VALUE_BYTES,
      );
      if (keys.has(key)) {
        throw new TypeError(`${storageLabel} must not contain duplicate keys`);
      }
      keys.add(key);
      originBytes += Buffer.byteLength(key, "utf8") + Buffer.byteLength(value, "utf8");
      if (originBytes > MAX_SESSION_STORAGE_BYTES_PER_ORIGIN) {
        throw new RangeError(
          `${label} string fields must total at most ${MAX_SESSION_STORAGE_BYTES_PER_ORIGIN} UTF-8 bytes`,
        );
      }
      entries.push({
        key,
        value,
      });
    }
    return entries;
  };
  return {
    origin: canonicalOrigin,
    localStorage: encodeEntries(origin.localStorage, `${label}.localStorage`),
    sessionStorage: encodeEntries(origin.sessionStorage, `${label}.sessionStorage`),
  };
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

export function encodeFillParams(
  selector: string,
  value: string,
  expectedGeneration: bigint,
): Record<string, unknown> {
  if (typeof value !== "string") throw new TypeError("value must be a string");
  return {
    ...encodeDocumentTargetParams(selector, expectedGeneration),
    value,
  };
}

export function encodeExtractParams(
  plan: ExtractPlan,
  expectedGeneration: bigint,
): Record<string, unknown> {
  if (typeof plan !== "object" || plan === null || Array.isArray(plan)) {
    throw new TypeError("plan must be an object");
  }
  if (typeof plan.rootSelector !== "string") {
    throw new TypeError("plan.rootSelector must be a string");
  }
  if (!Array.isArray(plan.fields)) {
    throw new TypeError("plan.fields must be an array");
  }
  const fields = plan.fields.map((field, index) => {
    if (typeof field !== "object" || field === null || Array.isArray(field)) {
      throw new TypeError(`plan.fields[${index}] must be an object`);
    }
    if (typeof field.name !== "string") {
      throw new TypeError(`plan.fields[${index}].name must be a string`);
    }
    if (typeof field.selector !== "string") {
      throw new TypeError(`plan.fields[${index}].selector must be a string`);
    }
    if (field.read !== "text" && field.read !== "html") {
      throw new TypeError(`plan.fields[${index}].read must be text or html`);
    }
    return { name: field.name, selector: field.selector, read: field.read };
  });
  return {
    rootSelector: plan.rootSelector,
    fields,
    expectedGeneration: encodeU64(expectedGeneration, "expectedGeneration"),
  };
}

export function encodeSessionDocumentTargetParams(
  selector: string,
  expectedStateToken: DocumentStateToken,
): Record<string, unknown> {
  if (typeof selector !== "string") throw new TypeError("selector must be a string");
  return {
    selector,
    expectedStateToken: encodeDocumentStateToken(expectedStateToken, "expectedStateToken"),
  };
}

export function encodeSessionFillParams(
  selector: string,
  value: string,
  expectedStateToken: DocumentStateToken,
): Record<string, unknown> {
  if (typeof value !== "string") throw new TypeError("value must be a string");
  return {
    ...encodeSessionDocumentTargetParams(selector, expectedStateToken),
    value,
  };
}

export function encodeSessionExtractParams(
  planValue: SessionExtractPlan,
  expectedStateToken: DocumentStateToken,
): Record<string, unknown> {
  const plan = inputRecord(planValue, "plan");
  inputExactKeys(plan, ["rootSelector", "fields"], "plan");
  if (!Array.isArray(plan.fields)) throw new TypeError("plan.fields must be an array");
  const fields = plan.fields.map((fieldValue, index) => {
    const label = `plan.fields[${index}]`;
    const field = inputRecord(fieldValue, label);
    const read = inputString(field.read, `${label}.read`);
    const readsAttribute = read === "attribute" || read === "resolved_url";
    const required = readsAttribute
      ? ["name", "selector", "read", "attribute"]
      : ["name", "selector", "read"];
    inputExactKeys(field, required, label);
    if (
      read !== "text" &&
      read !== "html" &&
      read !== "attribute" &&
      read !== "resolved_url"
    ) {
      throw new TypeError(`${label}.read must be text, html, attribute, or resolved_url`);
    }
    const encoded: Record<string, unknown> = {
      name: inputString(field.name, `${label}.name`),
      selector: inputString(field.selector, `${label}.selector`),
      read,
    };
    if (readsAttribute) {
      encoded.attribute = inputString(field.attribute, `${label}.attribute`);
    }
    return encoded;
  });
  return {
    rootSelector: inputString(plan.rootSelector, "plan.rootSelector"),
    fields,
    expectedStateToken: encodeDocumentStateToken(expectedStateToken, "expectedStateToken"),
  };
}

export function encodeSessionSelectParams(
  selector: string,
  values: readonly string[],
  expectedStateToken: DocumentStateToken,
): Record<string, unknown> {
  if (!Array.isArray(values) || values.some((value) => typeof value !== "string")) {
    throw new TypeError("values must be an array of strings");
  }
  if (values.length > MAX_SELECT_VALUES) {
    throw new RangeError(`values must contain at most ${MAX_SELECT_VALUES} entries`);
  }
  if (new Set(values).size !== values.length) {
    throw new TypeError("values must not contain duplicates");
  }
  const valueBytes = values.reduce(
    (total, value) => total + new TextEncoder().encode(value).byteLength,
    0,
  );
  if (valueBytes > MAX_SELECT_VALUE_BYTES) {
    throw new RangeError(
      `values must contain at most ${MAX_SELECT_VALUE_BYTES} UTF-8 bytes`,
    );
  }
  return {
    ...encodeSessionDocumentTargetParams(selector, expectedStateToken),
    values: [...values],
  };
}

export function encodeExpectedStateTokenParams(
  expectedStateToken: DocumentStateToken,
): Record<string, unknown> {
  return {
    expectedStateToken: encodeDocumentStateToken(expectedStateToken, "expectedStateToken"),
  };
}

export function encodeSessionSettleParams(
  expectedStateToken: DocumentStateToken,
  policy: SettlePolicy,
): Record<string, unknown> {
  return {
    ...encodeSettleParams(policy),
    expectedStateToken: encodeDocumentStateToken(expectedStateToken, "expectedStateToken"),
  };
}

export function encodeSessionNavigateParams(
  url: string | URL,
  expectedStateToken: DocumentStateToken,
): Record<string, unknown> {
  const serializedUrl = typeof url === "string" ? url : url.toString();
  if (serializedUrl.length === 0) throw new TypeError("url must not be empty");
  return {
    url: serializedUrl,
    expectedStateToken: encodeDocumentStateToken(expectedStateToken, "expectedStateToken"),
  };
}

export function encodeSessionStateTokenParams(
  expectedSessionStateToken: SessionStateToken,
): Record<string, unknown> {
  return {
    expectedSessionStateToken: encodeSessionStateToken(
      expectedSessionStateToken,
      "expectedSessionStateToken",
    ),
  };
}

export function encodeSessionCookiesSetParams(
  cookies: readonly SessionCookie[],
  expectedSessionStateToken: SessionStateToken,
): Record<string, unknown> {
  const token = encodeSessionStateTokenParams(expectedSessionStateToken);
  return {
    cookies: encodeSessionCookieArray(cookies, "cookies"),
    ...token,
  };
}

export function encodeSessionStorageSetParams(
  origins: readonly SessionOriginState[],
  expectedSessionStateToken: SessionStateToken,
): Record<string, unknown> {
  const token = encodeSessionStateTokenParams(expectedSessionStateToken);
  return {
    origins: encodeSessionOriginArray(origins, "origins"),
    ...token,
  };
}

export function encodeSessionStateImportParams(
  state: SessionState,
  expectedSessionStateToken: SessionStateToken,
): Record<string, unknown> {
  const token = encodeSessionStateTokenParams(expectedSessionStateToken);
  return {
    state: encodeSessionState(state),
    ...token,
  };
}

export function encodeSessionAuditParams(options: SessionAuditOptions): Record<string, unknown> {
  const params: Record<string, unknown> = {};
  if (options.afterSeq !== undefined) {
    params.afterSeq = encodeU64(options.afterSeq, "afterSeq");
  }
  if (options.limit !== undefined) {
    if (!Number.isSafeInteger(options.limit) || options.limit < 1) {
      throw new RangeError("limit must be a positive safe integer");
    }
    params.limit = options.limit;
  }
  return params;
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
  profile: LegacySupportProfile | null;
}

export function decodeOpenResult(
  value: unknown,
  envelopeSessionId: string | null,
  expectedClockMode: "real" | "controlled",
  expectedProfile: LegacySupportProfile | null,
): OpenResult {
  const result = record(value, "session.open result");
  exactKeys(result, ["sessionId", "requestedUrl", "url", "boundary", "clockMode", "profile"]);
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
  const profile =
    result.profile === null
      ? null
      : (expectEnum(
          result.profile,
          new Set([CONTROLLED_WEBAPP_V1_PROFILE]),
          "session.open result.profile",
        ) as LegacySupportProfile);
  if (profile !== expectedProfile) {
    invalid(
      `session.open requested profile ${String(expectedProfile)} but the runtime returned ${String(profile)}`,
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
    profile,
  };
}

export interface SessionOpenResult {
  sessionId: string;
  requestedUrl: string;
  url: string;
  boundary: "controlled_ready";
  clockMode: "controlled";
  profile: typeof CONTROLLED_WEB_SESSION_V1_PROFILE;
  stateToken: DocumentStateToken;
  sessionStateToken: SessionStateToken;
}

export function decodeSessionOpenResult(
  value: unknown,
  envelopeSessionId: string | null,
): SessionOpenResult {
  const result = record(value, "session.open result");
  exactKeys(result, [
    "sessionId",
    "requestedUrl",
    "url",
    "boundary",
    "clockMode",
    "profile",
    "stateToken",
    "sessionStateToken",
  ]);
  const sessionId = requireString(result.sessionId, "session.open result.sessionId");
  if (sessionId.length === 0 || sessionId !== envelopeSessionId) {
    invalid("session.open result and response envelope disagree on sessionId");
  }
  if (result.boundary !== "controlled_ready") {
    invalid("session.open session profile did not stop at controlled_ready");
  }
  if (result.clockMode !== "controlled") {
    invalid("session.open session profile did not return the controlled clock");
  }
  if (result.profile !== CONTROLLED_WEB_SESSION_V1_PROFILE) {
    invalid(`session.open did not return profile ${CONTROLLED_WEB_SESSION_V1_PROFILE}`);
  }
  return {
    sessionId,
    requestedUrl: requireString(result.requestedUrl, "session.open result.requestedUrl"),
    url: requireString(result.url, "session.open result.url"),
    boundary: "controlled_ready",
    clockMode: "controlled",
    profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
    stateToken: decodeDocumentStateToken(
      result.stateToken,
      "session.open result.stateToken",
    ),
    sessionStateToken: decodeSessionStateToken(
      result.sessionStateToken,
      "session.open result.sessionStateToken",
    ),
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

export function decodeActivation(value: unknown): AutomationMutationResult {
  return decodeAutomationMutation(value, "action.activate result");
}

export function decodeFill(value: unknown): AutomationMutationResult {
  return decodeAutomationMutation(value, "action.fill result");
}

export function decodeQuery(value: unknown): QueryResult {
  const result = record(value, "dom.query result");
  exactKeys(result, ["count", "stateGeneration"]);
  return {
    count: decodeU128(result.count, "dom.query result.count"),
    stateGeneration: decodeU64(
      result.stateGeneration,
      "dom.query result.stateGeneration",
    ),
  };
}

export function decodeExtract(value: unknown): ExtractResult {
  const result = record(value, "dom.extract result");
  exactKeys(result, ["rows", "stateGeneration"]);
  const rows = array(result.rows, "dom.extract result.rows").map((rowValue, rowIndex) => {
    const row = record(rowValue, `dom.extract result.rows[${rowIndex}]`);
    exactKeys(row, ["fields"]);
    const fields = array(row.fields, `dom.extract result.rows[${rowIndex}].fields`).map(
      (fieldValue, fieldIndex) => {
        const field = record(
          fieldValue,
          `dom.extract result.rows[${rowIndex}].fields[${fieldIndex}]`,
        );
        exactKeys(field, ["name", "value"]);
        return {
          name: requireString(
            field.name,
            `dom.extract result.rows[${rowIndex}].fields[${fieldIndex}].name`,
          ),
          value: requireString(
            field.value,
            `dom.extract result.rows[${rowIndex}].fields[${fieldIndex}].value`,
          ),
        };
      },
    );
    return { fields };
  });
  return {
    rows,
    stateGeneration: decodeU64(
      result.stateGeneration,
      "dom.extract result.stateGeneration",
    ),
  };
}

export function decodeSessionActivation(value: unknown): SessionAutomationMutationResult {
  return decodeSessionAutomationMutation(value, "action.activate result");
}

export function decodeSessionFill(value: unknown): SessionAutomationMutationResult {
  return decodeSessionAutomationMutation(value, "action.fill result");
}

export function decodeSessionFocus(value: unknown): SessionFocusResult {
  const result = record(value, "action.focus result");
  exactKeys(result, ["focused", "stateGeneration", "stateToken"]);
  return {
    focused: requireBoolean(result.focused, "action.focus result.focused"),
    ...decodeSessionActionAuthority(result, "action.focus result"),
  };
}

export function decodeSessionCheck(value: unknown): SessionCheckResult {
  return decodeSessionCheckResult(value, "action.check result");
}

export function decodeSessionUncheck(value: unknown): SessionCheckResult {
  return decodeSessionCheckResult(value, "action.uncheck result");
}

export function decodeSessionSelect(value: unknown): SessionSelectResult {
  const result = record(value, "action.select result");
  exactKeys(result, ["changed", "values", "stateGeneration", "stateToken"]);
  return {
    changed: requireBoolean(result.changed, "action.select result.changed"),
    values: requireStringArray(result.values, "action.select result.values"),
    ...decodeSessionActionAuthority(result, "action.select result"),
  };
}

export function decodeSessionSubmit(value: unknown): SessionSubmitResult {
  const result = record(value, "action.submit result");
  exactKeys(result, ["submitted", "stateGeneration", "stateToken"]);
  if (result.submitted !== true) invalid("action.submit result.submitted must be true");
  return {
    submitted: true,
    ...decodeSessionActionAuthority(result, "action.submit result"),
  };
}

export function decodeSessionQuery(value: unknown): SessionQueryResult {
  const result = record(value, "dom.query result");
  exactKeys(result, ["count", "stateGeneration", "stateToken"]);
  return {
    count: decodeU128(result.count, "dom.query result.count"),
    stateGeneration: decodeU64(
      result.stateGeneration,
      "dom.query result.stateGeneration",
    ),
    stateToken: decodeDocumentStateToken(result.stateToken, "dom.query result.stateToken"),
  };
}

export function decodeSessionText(value: unknown): SessionTextResult {
  const result = record(value, "dom.text result");
  exactKeys(result, ["value", "stateGeneration", "stateToken"]);
  return {
    value: requireString(result.value, "dom.text result.value"),
    stateGeneration: decodeU64(
      result.stateGeneration,
      "dom.text result.stateGeneration",
    ),
    stateToken: decodeDocumentStateToken(result.stateToken, "dom.text result.stateToken"),
  };
}

export function decodeSessionExtract(value: unknown): SessionExtractResult {
  const result = record(value, "dom.extract result");
  exactKeys(result, ["rows", "stateGeneration", "stateToken"]);
  const rows = array(result.rows, "dom.extract result.rows").map((rowValue, rowIndex) => {
    const rowLabel = `dom.extract result.rows[${rowIndex}]`;
    const row = record(rowValue, rowLabel);
    exactKeys(row, ["fields"]);
    const fields = array(row.fields, `${rowLabel}.fields`).map(
      (fieldValue, fieldIndex) => {
        const fieldLabel = `${rowLabel}.fields[${fieldIndex}]`;
        const field = record(fieldValue, fieldLabel);
        exactKeys(field, ["name", "value"]);
        if (field.value !== null && typeof field.value !== "string") {
          invalid(`${fieldLabel}.value must be a string or null`);
        }
        return {
          name: requireString(field.name, `${fieldLabel}.name`),
          value: field.value,
        };
      },
    );
    return { fields };
  });
  return {
    rows,
    stateGeneration: decodeU64(
      result.stateGeneration,
      "dom.extract result.stateGeneration",
    ),
    stateToken: decodeDocumentStateToken(
      result.stateToken,
      "dom.extract result.stateToken",
    ),
  };
}

function decodeSessionAutomationMutation(
  value: unknown,
  label: string,
): SessionAutomationMutationResult {
  const result = record(value, label);
  exactKeys(result, ["stateGeneration", "stateToken"]);
  return {
    stateGeneration: decodeU64(result.stateGeneration, `${label}.stateGeneration`),
    stateToken: decodeDocumentStateToken(result.stateToken, `${label}.stateToken`),
  };
}

function decodeSessionCheckResult(value: unknown, label: string): SessionCheckResult {
  const result = record(value, label);
  exactKeys(result, ["changed", "checked", "stateGeneration", "stateToken"]);
  return {
    changed: requireBoolean(result.changed, `${label}.changed`),
    checked: requireBoolean(result.checked, `${label}.checked`),
    ...decodeSessionActionAuthority(result, label),
  };
}

function decodeSessionActionAuthority(
  result: Record<string, unknown>,
  label: string,
): SessionAutomationMutationResult {
  return {
    stateGeneration: decodeU64(result.stateGeneration, `${label}.stateGeneration`),
    stateToken: decodeDocumentStateToken(result.stateToken, `${label}.stateToken`),
  };
}

function decodeAutomationMutation(value: unknown, label: string): AutomationMutationResult {
  const result = record(value, label);
  exactKeys(result, ["stateGeneration"]);
  return {
    stateGeneration: decodeU64(result.stateGeneration, `${label}.stateGeneration`),
  };
}

export function decodePending(value: unknown): PendingSnapshot {
  const decoded = decodeWideIntegers(value);
  return normalizePending(decoded);
}

export function decodeSessionPending(value: unknown): SessionPendingSnapshot {
  const pending = record(value, "runtime.pending result");
  const stateToken = decodeDocumentStateToken(
    pending.stateToken,
    "runtime.pending result.stateToken",
  );
  const legacyValue = { ...pending };
  delete legacyValue.stateToken;
  return { ...decodePending(legacyValue), stateToken };
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
  exactKeys(processed, [
    "controlTurns",
    "tasks",
    "microtasks",
    "renderingOpportunities",
    "mutations",
  ]);
  requireBigInt(processed.controlTurns, "processed.controlTurns");
  requireBigInt(processed.tasks, "processed.tasks");
  requireBigInt(processed.microtasks, "processed.microtasks");
  requireBigInt(processed.renderingOpportunities, "processed.renderingOpportunities");
  requireBigInt(processed.mutations, "processed.mutations");
  validateClassifiedWork(result.persistentWork, "persistentWork", true);
  validateClassifiedWork(result.unsupportedWork, "unsupportedWork", false);
  array(result.externalIo, "externalIo").forEach(validateExternalIo);
  validateSettleOutcomePayload(result);
  return result as unknown as SettleResult;
}

export function decodeSessionSettle(value: unknown): SessionSettleResult {
  const result = record(value, "runtime.settle result");
  const stateToken = decodeDocumentStateToken(
    result.stateToken,
    "runtime.settle result.stateToken",
  );
  const snapshot = record(result.snapshot, "runtime.settle result.snapshot");
  const snapshotStateToken = decodeDocumentStateToken(
    snapshot.stateToken,
    "runtime.settle result.snapshot.stateToken",
  );
  if (snapshotStateToken !== stateToken) {
    invalid("runtime.settle stateToken disagrees with its snapshot");
  }
  const legacySnapshot = { ...snapshot };
  delete legacySnapshot.stateToken;
  const legacyValue: Record<string, unknown> = { ...result, snapshot: legacySnapshot };
  delete legacyValue.stateToken;
  const decoded = decodeSettle(legacyValue);
  return {
    ...decoded,
    stateToken,
    snapshot: { ...decoded.snapshot, stateToken },
  } as SessionSettleResult;
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

export function decodeSessionAdvanceToNext(value: unknown): SessionAdvanceToNextResult {
  const result = record(value, "runtime.advance_to_next result");
  const stateToken = decodeDocumentStateToken(
    result.stateToken,
    "runtime.advance_to_next result.stateToken",
  );
  const snapshot = record(result.snapshot, "runtime.advance_to_next result.snapshot");
  const snapshotStateToken = decodeDocumentStateToken(
    snapshot.stateToken,
    "runtime.advance_to_next result.snapshot.stateToken",
  );
  if (snapshotStateToken !== stateToken) {
    invalid("runtime.advance_to_next stateToken disagrees with its snapshot");
  }
  const legacySnapshot = { ...snapshot };
  delete legacySnapshot.stateToken;
  const legacyValue: Record<string, unknown> = { ...result, snapshot: legacySnapshot };
  delete legacyValue.stateToken;
  const decoded = decodeAdvanceToNext(legacyValue);
  return {
    ...decoded,
    stateToken,
    snapshot: { ...decoded.snapshot, stateToken },
  } as SessionAdvanceToNextResult;
}

export function decodeSessionNavigate(value: unknown): SessionNavigateResult {
  const result = record(value, "session.navigate result");
  exactKeys(result, [
    "requestedUrl",
    "url",
    "boundary",
    "stateGeneration",
    "domEpoch",
    "documentEpoch",
    "navigationId",
    "historyRevision",
    "stateToken",
  ]);
  if (result.boundary !== "controlled_ready") {
    invalid("session.navigate did not stop at controlled_ready");
  }
  return {
    requestedUrl: requireString(result.requestedUrl, "session.navigate result.requestedUrl"),
    url: requireString(result.url, "session.navigate result.url"),
    boundary: "controlled_ready",
    stateGeneration: decodeU64(
      result.stateGeneration,
      "session.navigate result.stateGeneration",
    ),
    domEpoch: decodeU64(result.domEpoch, "session.navigate result.domEpoch"),
    documentEpoch: decodeU64(
      result.documentEpoch,
      "session.navigate result.documentEpoch",
    ),
    navigationId: decodeU64(
      result.navigationId,
      "session.navigate result.navigationId",
    ),
    historyRevision: decodeU64(
      result.historyRevision,
      "session.navigate result.historyRevision",
    ),
    stateToken: decodeDocumentStateToken(
      result.stateToken,
      "session.navigate result.stateToken",
    ),
  };
}

export function decodeSessionCookies(value: unknown): SessionCookiesResult {
  const result = record(value, "session.cookies.get result");
  exactKeysSecretSafe(result, ["cookies", "sessionStateToken"]);
  const cookieValues = decodedBoundedArray(
    result.cookies,
    "session.cookies.get result.cookies",
    MAX_SESSION_COOKIES,
  );
  const cookies = cookieValues.map((cookie, index) =>
    decodeSessionCookie(cookie, index, "session.cookies.get result.cookies"),
  );
  validateDecodedSessionStateInput(() => {
    encodeSessionCookieArray(cookies, "session.cookies.get result.cookies");
  });
  return {
    cookies,
    sessionStateToken: decodeSessionStateToken(
      result.sessionStateToken,
      "session.cookies.get result.sessionStateToken",
    ),
  };
}

export function decodeSessionStorage(value: unknown): SessionStorageResult {
  const result = record(value, "session.storage.get result");
  exactKeysSecretSafe(result, ["origins", "sessionStateToken"]);
  const originValues = decodedBoundedArray(
    result.origins,
    "session.storage.get result.origins",
    MAX_SESSION_STORAGE_ORIGINS,
  );
  const origins = originValues.map((origin, index) =>
    decodeSessionOrigin(origin, index, "session.storage.get result.origins"),
  );
  validateDecodedSessionStateInput(() => {
    encodeSessionOriginArray(origins, "session.storage.get result.origins");
  });
  return {
    origins,
    sessionStateToken: decodeSessionStateToken(
      result.sessionStateToken,
      "session.storage.get result.sessionStateToken",
    ),
  };
}

export function decodeSessionStateExport(value: unknown): SessionStateExportResult {
  const result = record(value, "session.state.export result");
  exactKeysSecretSafe(result, ["state", "sessionStateToken"]);
  return {
    state: decodeSessionState(result.state, "session.state.export result.state"),
    sessionStateToken: decodeSessionStateToken(
      result.sessionStateToken,
      "session.state.export result.sessionStateToken",
    ),
  };
}

export function decodeSessionStateMutation(
  value: unknown,
  label = "session state mutation result",
): SessionStateMutationResult {
  const result = record(value, label);
  exactKeysSecretSafe(result, ["sessionStateToken"]);
  return {
    sessionStateToken: decodeSessionStateToken(
      result.sessionStateToken,
      `${label}.sessionStateToken`,
    ),
  };
}

export function decodeUnexpectedSessionStateImportSuccess(_value: unknown): never {
  invalid("session.state.import unexpectedly succeeded after session publication");
}

export function decodeSessionRequests(value: unknown): SessionRequestsResult {
  const result = record(value, "session.requests result");
  exactKeys(
    result,
    ["records", "complete", "hasMore", "bounds", "stateToken"],
    ["firstRetainedSeq", "nextAfterSeq", "latestSeq", "droppedThroughSeq"],
  );
  const records = array(result.records, "session.requests result.records").map(
    decodeSessionRequestRecord,
  );
  const bounds = decodeAuditBounds(result.bounds, "session.requests result.bounds");
  if (records.length > bounds.maxPageItems) {
    invalid("session.requests returned more records than bounds.maxPageItems");
  }
  validateIncreasingAuditSequence(records, "session.requests result.records");
  const cursor = decodeAuditCursorFields(result, "session.requests result");
  const complete = requireBoolean(result.complete, "session.requests result.complete");
  const hasMore = requireBoolean(result.hasMore, "session.requests result.hasMore");
  validateAuditPageInvariants(
    records,
    cursor,
    complete,
    hasMore,
    "session.requests result",
  );
  return {
    records,
    ...cursor,
    complete,
    hasMore,
    bounds,
    stateToken: decodeDocumentStateToken(
      result.stateToken,
      "session.requests result.stateToken",
    ),
  };
}

export function decodeSessionEvidence(value: unknown): SessionEvidenceResult {
  const result = record(value, "session.evidence result");
  exactKeys(
    result,
    ["schemaVersion", "records", "complete", "hasMore", "bounds", "stateToken"],
    ["firstRetainedSeq", "nextAfterSeq", "latestSeq", "droppedThroughSeq"],
  );
  if (result.schemaVersion !== 2) invalid("session.evidence schemaVersion must be 2");
  const records = array(result.records, "session.evidence result.records").map(
    (recordValue, index) => {
      const label = `session.evidence result.records[${index}]`;
      const evidence = record(recordValue, label);
      if (!Object.hasOwn(evidence, "seq") || !Object.hasOwn(evidence, "atVirtualNs")) {
        invalid(`${label} must contain seq and atVirtualNs`);
      }
      const eventValue = { ...evidence };
      delete eventValue.seq;
      delete eventValue.atVirtualNs;
      return {
        seq: decodeU64(evidence.seq, `${label}.seq`),
        atVirtualNs: decodeU128(evidence.atVirtualNs, `${label}.atVirtualNs`),
        ...decodeSessionEvidenceEvent(eventValue, label),
      };
    },
  );
  const bounds = decodeAuditBounds(result.bounds, "session.evidence result.bounds");
  if (records.length > bounds.maxPageItems) {
    invalid("session.evidence returned more records than bounds.maxPageItems");
  }
  validateIncreasingAuditSequence(records, "session.evidence result.records");
  const cursor = decodeAuditCursorFields(result, "session.evidence result");
  const complete = requireBoolean(result.complete, "session.evidence result.complete");
  const hasMore = requireBoolean(result.hasMore, "session.evidence result.hasMore");
  validateAuditPageInvariants(
    records,
    cursor,
    complete,
    hasMore,
    "session.evidence result",
  );
  return {
    schemaVersion: 2,
    records,
    ...cursor,
    complete,
    hasMore,
    bounds,
    stateToken: decodeDocumentStateToken(
      result.stateToken,
      "session.evidence result.stateToken",
    ),
  };
}

function decodeSessionState(value: unknown, label: string): SessionState {
  const state = record(value, label);
  exactKeysSecretSafe(state, [
    "schemaVersion",
    "profile",
    "sensitive",
    "sessionStorageScope",
    "cookies",
    "origins",
  ]);
  if (state.schemaVersion !== 1) invalid(`${label}.schemaVersion must be 1`);
  if (state.profile !== CONTROLLED_WEB_SESSION_V1_PROFILE) {
    invalid(`${label}.profile must be ${CONTROLLED_WEB_SESSION_V1_PROFILE}`);
  }
  if (state.sensitive !== true) invalid(`${label}.sensitive must be true`);
  if (state.sessionStorageScope !== "top_level_browsing_context") {
    invalid(`${label}.sessionStorageScope must be top_level_browsing_context`);
  }
  const cookieValues = decodedBoundedArray(
    state.cookies,
    `${label}.cookies`,
    MAX_SESSION_COOKIES,
  );
  const originValues = decodedBoundedArray(
    state.origins,
    `${label}.origins`,
    MAX_SESSION_STORAGE_ORIGINS,
  );
  const decoded: SessionState = {
    schemaVersion: 1,
    profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
    sensitive: true,
    sessionStorageScope: "top_level_browsing_context",
    cookies: cookieValues.map((cookie, index) =>
      decodeSessionCookie(cookie, index, `${label}.cookies`),
    ),
    origins: originValues.map((origin, index) =>
      decodeSessionOrigin(origin, index, `${label}.origins`),
    ),
  };
  validateDecodedSessionStateInput(() => {
    encodeSessionState(decoded);
  });
  return decoded;
}

function decodeSessionCookie(
  value: unknown,
  index: number,
  parentLabel: string,
): SessionCookie {
  const label = `${parentLabel}[${index}]`;
  const cookie = record(value, label);
  exactKeysSecretSafe(cookie, [
    "name",
    "value",
    "domain",
    "path",
    "hostOnly",
    "secure",
    "httpOnly",
    "sameSite",
    "expiresUnixTimeNs",
    "partitioned",
    "creationSequence",
    "lastAccessSequence",
  ]);
  if (
    typeof cookie.sameSite !== "string" ||
    !new Set(["unspecified", "strict", "lax", "none"]).has(cookie.sameSite)
  ) {
    invalid(`${label}.sameSite is invalid`);
  }
  const sameSite = cookie.sameSite as SessionCookie["sameSite"];
  if (cookie.expiresUnixTimeNs !== null) {
    invalid(`${label}.expiresUnixTimeNs must be null in session state v1`);
  }
  if (cookie.partitioned !== false) {
    invalid(`${label}.partitioned must be false in session state v1`);
  }
  return {
    name: requireString(cookie.name, `${label}.name`),
    value: requireString(cookie.value, `${label}.value`),
    domain: requireString(cookie.domain, `${label}.domain`),
    path: requireString(cookie.path, `${label}.path`),
    hostOnly: requireBoolean(cookie.hostOnly, `${label}.hostOnly`),
    secure: requireBoolean(cookie.secure, `${label}.secure`),
    httpOnly: requireBoolean(cookie.httpOnly, `${label}.httpOnly`),
    sameSite,
    expiresUnixTimeNs: null,
    partitioned: false,
    creationSequence: decodeU64(cookie.creationSequence, `${label}.creationSequence`),
    lastAccessSequence: decodeU64(cookie.lastAccessSequence, `${label}.lastAccessSequence`),
  };
}

function decodeSessionOrigin(
  value: unknown,
  index: number,
  parentLabel: string,
): SessionOriginState {
  const label = `${parentLabel}[${index}]`;
  const origin = record(value, label);
  exactKeysSecretSafe(origin, ["origin", "localStorage", "sessionStorage"]);
  const decodeEntries = (entryValue: unknown, storageLabel: string) => {
    const values = decodedBoundedArray(
      entryValue,
      storageLabel,
      MAX_SESSION_STORAGE_ENTRIES_PER_AREA,
    );
    return values.map((itemValue, entryIndex) => {
      const entryLabel = `${storageLabel}[${entryIndex}]`;
      const entry = record(itemValue, entryLabel);
      exactKeysSecretSafe(entry, ["key", "value"]);
      return {
        key: requireString(entry.key, `${entryLabel}.key`),
        value: requireString(entry.value, `${entryLabel}.value`),
      };
    });
  };
  return {
    origin: requireString(origin.origin, `${label}.origin`),
    localStorage: decodeEntries(origin.localStorage, `${label}.localStorage`),
    sessionStorage: decodeEntries(origin.sessionStorage, `${label}.sessionStorage`),
  };
}

function decodedBoundedArray(value: unknown, label: string, maximum: number): unknown[] {
  const values = array(value, label);
  if (values.length > maximum) {
    invalid(`${label} must contain at most ${maximum} items`);
  }
  return values;
}

function validateDecodedSessionStateInput(validation: () => void): void {
  try {
    validation();
  } catch (error) {
    if (error instanceof TypeError || error instanceof RangeError) {
      invalid(error.message);
    }
    invalid("session state result violates the bounded state contract");
  }
}

function decodeSessionRequestRecord(value: unknown, index: number): SessionRequestRecord {
  const label = `session.requests result.records[${index}]`;
  const request = record(value, label);
  exactKeys(
    request,
    ["seq", "requestId", "method", "url", "resourceKind", "mainFrame", "headerNames", "bodyBytes"],
    ["redirectParentId"],
  );
  const url = record(request.url, `${label}.url`);
  exactKeys(url, ["origin", "path", "queryKeys"]);
  const decoded: SessionRequestRecord = {
    seq: decodeU64(request.seq, `${label}.seq`),
    requestId: requireOpaqueId(request.requestId, `${label}.requestId`),
    method: requireString(request.method, `${label}.method`),
    url: {
      origin: requireString(url.origin, `${label}.url.origin`),
      path: requireString(url.path, `${label}.url.path`),
      queryKeys: requireSortedUniqueStringArray(url.queryKeys, `${label}.url.queryKeys`),
    },
    resourceKind: expectEnum(
      request.resourceKind,
      NETWORK_KINDS,
      `${label}.resourceKind`,
    ) as SessionRequestRecord["resourceKind"],
    mainFrame: requireBoolean(request.mainFrame, `${label}.mainFrame`),
    headerNames: requireStringArray(request.headerNames, `${label}.headerNames`),
    bodyBytes: decodeU64(request.bodyBytes, `${label}.bodyBytes`),
  };
  if (request.redirectParentId !== undefined) {
    decoded.redirectParentId = requireOpaqueId(
      request.redirectParentId,
      `${label}.redirectParentId`,
    );
  }
  return decoded;
}

function decodeSessionEvidenceEvent(value: unknown, label: string): SessionEvidenceEvent {
  const event = record(value, label);
  const requestId = () => requireOpaqueId(event.requestId, `${label}.requestId`);
  switch (event.kind) {
    case "request_started":
    case "request_completed":
      exactKeys(event, ["kind", "requestId"]);
      return { kind: event.kind, requestId: requestId() };
    case "route_decided": {
      exactKeys(event, ["kind", "requestId", "decision"]);
      const decision = expectEnum(
        event.decision,
        new Set(["fixture_fulfill", "fixture_abort", "live"]),
        `${label}.decision`,
      ) as "fixture_fulfill" | "fixture_abort" | "live";
      return { kind: "route_decided", requestId: requestId(), decision };
    }
    case "response_headers":
      exactKeys(event, ["kind", "requestId", "status"]);
      return {
        kind: "response_headers",
        requestId: requestId(),
        status: requireHttpStatus(event.status, `${label}.status`),
      };
    case "redirect":
      exactKeys(event, ["kind", "requestId", "nextRequestId"]);
      return {
        kind: "redirect",
        requestId: requestId(),
        nextRequestId: requireOpaqueId(event.nextRequestId, `${label}.nextRequestId`),
      };
    case "request_failed":
      exactKeys(event, ["kind", "requestId", "reason"]);
      return {
        kind: "request_failed",
        requestId: requestId(),
        reason: expectEnum(
          event.reason,
          SESSION_EVIDENCE_FAILURE_REASONS,
          `${label}.reason`,
        ) as SessionEvidenceFailureReason,
      };
    case "navigation_started":
    case "navigation_committed":
    case "same_document_history_changed":
    case "settlement_terminal":
      exactKeys(event, ["kind", "navigationId"]);
      return {
        kind: event.kind,
        navigationId: decodeU64(event.navigationId, `${label}.navigationId`),
      };
    case "navigation_failed":
      exactKeys(event, ["kind", "navigationId", "reason"]);
      return {
        kind: "navigation_failed",
        navigationId: decodeU64(event.navigationId, `${label}.navigationId`),
        reason: expectEnum(
          event.reason,
          SESSION_EVIDENCE_FAILURE_REASONS,
          `${label}.reason`,
        ) as SessionEvidenceFailureReason,
      };
    default:
      invalid(`${label}.kind has unknown value ${String(event.kind)}`);
  }
}

function decodeAuditBounds(value: unknown, label: string): SessionRequestsResult["bounds"] {
  const bounds = record(value, label);
  exactKeys(bounds, ["maxRecords", "maxMetadataBytes", "maxPageItems"]);
  return {
    maxRecords: requireSafeInteger(bounds.maxRecords, `${label}.maxRecords`),
    maxMetadataBytes: requireSafeInteger(
      bounds.maxMetadataBytes,
      `${label}.maxMetadataBytes`,
    ),
    maxPageItems: requireSafeInteger(bounds.maxPageItems, `${label}.maxPageItems`),
  };
}

function decodeAuditCursorFields(
  value: Record<string, unknown>,
  label: string,
): Pick<
  SessionRequestsResult,
  "firstRetainedSeq" | "nextAfterSeq" | "latestSeq" | "droppedThroughSeq"
> {
  return {
    ...(value.firstRetainedSeq === undefined
      ? {}
      : { firstRetainedSeq: decodeU64(value.firstRetainedSeq, `${label}.firstRetainedSeq`) }),
    ...(value.nextAfterSeq === undefined
      ? {}
      : { nextAfterSeq: decodeU64(value.nextAfterSeq, `${label}.nextAfterSeq`) }),
    ...(value.latestSeq === undefined
      ? {}
      : { latestSeq: decodeU64(value.latestSeq, `${label}.latestSeq`) }),
    ...(value.droppedThroughSeq === undefined
      ? {}
      : {
          droppedThroughSeq: decodeU64(
            value.droppedThroughSeq,
            `${label}.droppedThroughSeq`,
          ),
        }),
  };
}

function validateIncreasingAuditSequence(
  records: readonly { seq: bigint }[],
  label: string,
): void {
  for (let index = 1; index < records.length; index += 1) {
    if (records[index]!.seq <= records[index - 1]!.seq) {
      invalid(`${label} must be strictly increasing by seq`);
    }
  }
}

function validateAuditPageInvariants(
  records: readonly { seq: bigint }[],
  cursor: Pick<
    SessionRequestsResult,
    "firstRetainedSeq" | "nextAfterSeq" | "latestSeq" | "droppedThroughSeq"
  >,
  complete: boolean,
  hasMore: boolean,
  label: string,
): void {
  if (!complete && cursor.droppedThroughSeq === undefined) {
    invalid(`${label}.complete may be false only when droppedThroughSeq is present`);
  }
  if (hasMore && records.length === 0) {
    invalid(`${label}.hasMore cannot be true without a returned record`);
  }
  if (
    cursor.droppedThroughSeq !== undefined &&
    cursor.firstRetainedSeq !== undefined &&
    cursor.droppedThroughSeq >= cursor.firstRetainedSeq
  ) {
    invalid(`${label}.droppedThroughSeq must precede firstRetainedSeq`);
  }

  const first = records[0];
  const last = records.at(-1);
  if (first !== undefined) {
    if (cursor.firstRetainedSeq === undefined) {
      invalid(`${label}.firstRetainedSeq is required when records are returned`);
    }
    if (first.seq < cursor.firstRetainedSeq) {
      invalid(`${label}.records cannot precede firstRetainedSeq`);
    }
  }
  if (last !== undefined) {
    if (cursor.nextAfterSeq !== last.seq) {
      invalid(`${label}.nextAfterSeq must equal the last returned record seq`);
    }
    if (cursor.latestSeq === undefined || last.seq > cursor.latestSeq) {
      invalid(`${label}.latestSeq must include every returned record`);
    }
  }
}

function requireHttpStatus(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 100 || value > 599) {
    invalid(`${label} must be an integer between 100 and 599`);
  }
  return value;
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
  const executionLimitKind =
    outcome === "task_limit_exceeded"
      ? "ordinary_tasks"
      : outcome === "microtask_limit_exceeded"
        ? "microtasks"
        : outcome === "rendering_limit_exceeded"
          ? "rendering_opportunities"
          : outcome === "mutation_limit_exceeded"
            ? "mutations"
            : null;
  if (executionLimitKind !== null) {
    if (result.failure !== undefined) invalid(`${outcome} must omit failure`);
    const limit = record(result.limit, "limit");
    exactKeys(limit, ["kind", "limit", "observed"]);
    if (limit.kind !== executionLimitKind) {
      invalid(`${outcome} requires a ${executionLimitKind} limit`);
    }
    const configured = requireBigInt(limit.limit, "limit.limit");
    const observed = requireBigInt(limit.observed, "limit.observed");
    if (observed <= configured) invalid(`${outcome} observed work must exceed its limit`);
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

function encodeDocumentStateToken(value: DocumentStateToken, label: string): string {
  return encodeOpaqueToken(value, label);
}

function encodeSessionStateToken(value: SessionStateToken, label: string): string {
  return encodeOpaqueToken(value, label);
}

function encodeOpaqueToken(value: unknown, label: string): string {
  if (!isBoundedOpaqueToken(value)) {
    throw new TypeError(
      `${label} must be a non-empty opaque token of at most ${MAX_OPAQUE_TOKEN_BYTES} UTF-8 bytes`,
    );
  }
  return value;
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

function decodeDocumentStateToken(value: unknown, label: string): DocumentStateToken {
  return decodeOpaqueToken(value, label) as DocumentStateToken;
}

function decodeSessionStateToken(value: unknown, label: string): SessionStateToken {
  return decodeOpaqueToken(value, label) as SessionStateToken;
}

function decodeOpaqueToken(value: unknown, label: string): string {
  if (!isBoundedOpaqueToken(value)) {
    invalid(
      `${label} must be a non-empty opaque token of at most ${MAX_OPAQUE_TOKEN_BYTES} UTF-8 bytes`,
    );
  }
  return value;
}

function isBoundedOpaqueToken(value: unknown): value is string {
  if (typeof value !== "string") return false;
  if (value.length === 0 || value.length > MAX_OPAQUE_TOKEN_BYTES) return false;
  if (!hasWellFormedUtf16(value)) return false;
  if (Buffer.byteLength(value, "utf8") > MAX_OPAQUE_TOKEN_BYTES) return false;
  return true;
}

function inputRecord(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function inputExactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  label: string,
  optional: readonly string[] = [],
): void {
  for (const key of required) {
    if (!Object.hasOwn(value, key)) throw new TypeError(`${label} is missing required field ${key}`);
  }
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new TypeError(`${label} contains unexpected field ${key}`);
  }
}

function inputExactKeysSecretSafe(
  value: Record<string, unknown>,
  required: readonly string[],
  label: string,
  optional: readonly string[] = [],
): void {
  for (const key of required) {
    if (!Object.hasOwn(value, key)) {
      throw new TypeError(`${label} is missing a required field`);
    }
  }
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new TypeError(`${label} contains an unexpected field`);
  }
}

function inputString(value: unknown, label: string): string {
  if (typeof value !== "string") throw new TypeError(`${label} must be a string`);
  return value;
}

function inputBoolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") throw new TypeError(`${label} must be a boolean`);
  return value;
}

function inputBoundedArray(value: unknown, label: string, maximum: number): unknown[] {
  if (!Array.isArray(value)) throw new TypeError(`${label} must be an array`);
  if (value.length > maximum) {
    throw new RangeError(`${label} must contain at most ${maximum} items`);
  }
  return value;
}

function inputBoundedUtf8String(value: unknown, label: string, maximumBytes: number): string {
  const input = inputString(value, label);
  // Every UTF-8 encoding uses at least one byte per UTF-16 code unit after paired-surrogate
  // accounting. This cheap check rejects adversarially large strings before byte encoding.
  if (input.length > maximumBytes) {
    throw new RangeError(`${label} must encode to at most ${maximumBytes} UTF-8 bytes`);
  }
  if (!hasWellFormedUtf16(input)) {
    throw new TypeError(`${label} must contain well-formed Unicode`);
  }
  if (Buffer.byteLength(input, "utf8") > maximumBytes) {
    throw new RangeError(`${label} must encode to at most ${maximumBytes} UTF-8 bytes`);
  }
  return input;
}

function hasWellFormedUtf16(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      if (index + 1 >= value.length) return false;
      const next = value.charCodeAt(index + 1);
      if (next < 0xdc00 || next > 0xdfff) return false;
      index += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return false;
    }
  }
  return true;
}

function inputCanonicalOrigin(value: unknown, label: string): string {
  const origin = inputBoundedUtf8String(
    value,
    label,
    MAX_SESSION_STORAGE_BYTES_PER_ORIGIN,
  );
  let parsed: URL;
  try {
    parsed = new URL(origin);
  } catch {
    throw new TypeError(`${label} must be a canonical HTTP(S) origin`);
  }
  if (
    (parsed.protocol !== "http:" && parsed.protocol !== "https:") ||
    parsed.origin !== origin
  ) {
    throw new TypeError(`${label} must be a canonical HTTP(S) origin`);
  }
  return origin;
}

function isCanonicalCookieDomain(domain: string): boolean {
  if (
    domain.length === 0 ||
    domain.startsWith(".") ||
    domain.endsWith(".") ||
    domain.startsWith("[")
  ) {
    return false;
  }
  const ipv6 = domain.includes(":");
  let parsed: URL;
  try {
    parsed = new URL(`http://${ipv6 ? `[${domain}]` : domain}/`);
  } catch {
    return false;
  }
  const parsedHostname =
    ipv6 && parsed.hostname.startsWith("[") && parsed.hostname.endsWith("]")
      ? parsed.hostname.slice(1, -1)
      : parsed.hostname;
  return (
    parsedHostname === domain &&
    parsed.username.length === 0 &&
    parsed.password.length === 0 &&
    parsed.port.length === 0 &&
    parsed.pathname === "/" &&
    parsed.search.length === 0 &&
    parsed.hash.length === 0
  );
}

function isValidCookieNameAndValue(name: string, value: string): boolean {
  const isTokenByte = (byte: number): boolean =>
    (byte >= 0x30 && byte <= 0x39) ||
    (byte >= 0x41 && byte <= 0x5a) ||
    (byte >= 0x61 && byte <= 0x7a) ||
    COOKIE_NAME_TOKEN_PUNCTUATION.has(byte);
  const isCookieOctet = (byte: number): boolean =>
    byte === 0x21 ||
    (byte >= 0x23 && byte <= 0x2b) ||
    (byte >= 0x2d && byte <= 0x3a) ||
    (byte >= 0x3c && byte <= 0x5b) ||
    (byte >= 0x5d && byte <= 0x7e);
  if (name.length === 0) return false;
  for (let index = 0; index < name.length; index += 1) {
    if (!isTokenByte(name.charCodeAt(index))) return false;
  }
  const quoted = value.startsWith('"') || value.endsWith('"');
  if (quoted && !(value.length >= 2 && value.startsWith('"') && value.endsWith('"'))) {
    return false;
  }
  const start = quoted ? 1 : 0;
  const end = quoted ? value.length - 1 : value.length;
  for (let index = start; index < end; index += 1) {
    if (!isCookieOctet(value.charCodeAt(index))) return false;
  }
  return true;
}

function addCompactArrayItemBytes(
  currentBytes: number,
  item: Record<string, unknown>,
  index: number,
  maximumBytes: number,
  label: string,
): number {
  const nextBytes =
    currentBytes +
    (index === 0 ? 0 : 1) +
    Buffer.byteLength(JSON.stringify(item), "utf8");
  if (nextBytes > maximumBytes) {
    throw new RangeError(`${label} must encode to at most ${maximumBytes} UTF-8 bytes`);
  }
  return nextBytes;
}

function assertCompactJsonBytes(value: unknown, maximumBytes: number, label: string): void {
  if (Buffer.byteLength(JSON.stringify(value), "utf8") > maximumBytes) {
    throw new RangeError(`${label} must encode to at most ${maximumBytes} UTF-8 bytes`);
  }
}

function isCanonicalBase64(value: string): boolean {
  if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/u.test(value)) {
    return false;
  }
  return Buffer.from(value, "base64").toString("base64") === value;
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

function requireSortedUniqueStringArray(value: unknown, label: string): string[] {
  const strings = requireStringArray(value, label);
  for (let index = 1; index < strings.length; index += 1) {
    if (
      Buffer.compare(
        Buffer.from(strings[index - 1]!, "utf8"),
        Buffer.from(strings[index]!, "utf8"),
      ) >= 0
    ) {
      invalid(`${label} must be sorted and unique by UTF-8 bytes`);
    }
  }
  return strings;
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

function exactKeysSecretSafe(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[] = [],
): void {
  for (const key of required) {
    if (!Object.hasOwn(value, key)) invalid("result is missing a required field");
  }
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) invalid("result contains an unexpected field");
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
