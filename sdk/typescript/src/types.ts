import type {
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  LegacySupportProfile,
  SelectableSessionProfile,
  SessionSupportProfile,
} from "./profile.js";

declare const documentStateTokenBrand: unique symbol;
declare const sessionStateTokenBrand: unique symbol;

/** Opaque, process-local authority for exactly one active session document state. */
export type DocumentStateToken = string & {
  readonly [documentStateTokenBrand]: "DocumentStateToken";
};

/** Opaque, process-local authority for cookie and Web Storage state owned by one session. */
export type SessionStateToken = string & {
  readonly [sessionStateTokenBrand]: "SessionStateToken";
};

export interface CommandOptions {
  signal?: AbortSignal;
  /** Wall-clock bound for a written native command. Defaults to the launch command timeout. */
  timeoutMs?: number;
}

/** Authoritative state after a mutating automation operation completes. */
export interface AutomationMutationResult {
  stateGeneration: bigint;
}

/** Bounded match count for a selector; this is not a persistent DOM handle. */
export interface QueryResult {
  count: bigint;
  stateGeneration: bigint;
}

export type ExtractRead = "text" | "html";

export interface ExtractField {
  name: string;
  selector: string;
  read: ExtractRead;
}

export interface ExtractPlan {
  rootSelector: string;
  fields: readonly ExtractField[];
}

export interface ExtractValue {
  name: string;
  value: string;
}

export interface ExtractRow {
  /** Field order is preserved exactly as requested. */
  fields: ExtractValue[];
}

export interface ExtractResult {
  /** Root match order and each row's field order are preserved. */
  rows: ExtractRow[];
  stateGeneration: bigint;
}

export interface LaunchOptions extends CommandOptions {
  /**
   * Highest-priority native runtime override. It is passed directly to spawn
   * with shell disabled. When omitted, the exact runtime bound to this SDK
   * version is acquired into a verified per-user cache.
   */
  executablePath?: string;
  /** Override the per-user managed-runtime cache. Ignored when executablePath is provided. */
  runtimeCacheDirectory?: string;
  args?: readonly string[];
  cwd?: string;
  env?: Record<string, string | undefined>;
  /** Maximum retained diagnostic bytes from the end of stderr. Defaults to 64 KiB. */
  maxStderrBytes?: number;
  /** Maximum stdout NDJSON payload bytes, excluding LF/CRLF. Defaults to 1 MiB. */
  maxFrameBytes?: number;
  /** Maximum wait after session.close succeeds for a clean child exit. Defaults to 30 seconds. */
  closeTimeoutMs?: number;
  /** Mandatory wall-clock bound for every written native command. Defaults to 30 seconds. */
  commandTimeoutMs?: number;
}

export type ClockOptions =
  | { mode: "real" }
  | {
      mode: "controlled";
      initialVirtualTimeNs?: bigint;
      /** The controlled MVP currently supports only the Unix epoch. */
      unixTimeOriginNs?: 0n;
    };

export interface OpenOptions extends CommandOptions {
  clock?: ClockOptions;
  /** Defaults to controlled-webapp-v1 for a controlled open and is forbidden for real time. */
  profile?: LegacySupportProfile;
}

export type ControlledClockOptions = Extract<ClockOptions, { mode: "controlled" }>;

export type NetworkMode = "fixtures_only" | "mixed" | "live";

export type NetworkUrlMatcher =
  | { readonly exact: string; readonly prefix?: never; readonly glob?: never }
  | { readonly exact?: never; readonly prefix: string; readonly glob?: never }
  | { readonly exact?: never; readonly prefix?: never; readonly glob: string };

export interface NetworkRouteMatch {
  readonly method: string;
  readonly url: NetworkUrlMatcher;
}

export type NetworkBody =
  | { readonly utf8: string; readonly base64?: never }
  | { readonly utf8?: never; readonly base64: string };

export interface NetworkFulfillment {
  readonly status: number;
  /** Ordered pairs preserve duplicate response fields such as Set-Cookie. */
  readonly headers?: readonly (readonly [string, string])[];
  readonly body?: NetworkBody;
}

export type NetworkAbortReason =
  | "blocked_by_fixture"
  | "connection_reset"
  | "network_error";

export type NetworkRoute =
  | {
      readonly match: NetworkRouteMatch;
      readonly fulfill: NetworkFulfillment;
      readonly abort?: never;
    }
  | {
      readonly match: NetworkRouteMatch;
      readonly fulfill?: never;
      readonly abort: { readonly reason: NetworkAbortReason };
    };

export interface SessionNetworkOptions {
  readonly mode: NetworkMode;
  /** First match wins. A fixtures_only miss is a typed failure, never ambient network. */
  readonly routes: readonly NetworkRoute[];
}

export type CookieSameSite = "unspecified" | "strict" | "lax" | "none";

export interface SessionCookie {
  readonly name: string;
  readonly value: string;
  readonly domain: string;
  readonly path: string;
  readonly hostOnly: boolean;
  readonly secure: boolean;
  readonly httpOnly: boolean;
  readonly sameSite: CookieSameSite;
  /** Session-profile state artifacts currently import only session cookies. */
  readonly expiresUnixTimeNs: null;
  readonly partitioned: false;
  readonly creationSequence: bigint;
  readonly lastAccessSequence: bigint;
}

export interface SessionStorageEntry {
  readonly key: string;
  readonly value: string;
}

export interface SessionOriginState {
  readonly origin: string;
  readonly localStorage: readonly SessionStorageEntry[];
  readonly sessionStorage: readonly SessionStorageEntry[];
}

export interface SessionState {
  readonly schemaVersion: 1;
  readonly profile: typeof CONTROLLED_WEB_SESSION_V1_PROFILE;
  readonly sensitive: true;
  readonly sessionStorageScope: "top_level_browsing_context";
  readonly cookies: readonly SessionCookie[];
  readonly origins: readonly SessionOriginState[];
}

export interface SessionOpenOptions<
  Profile extends SelectableSessionProfile = SessionSupportProfile,
> extends CommandOptions {
  clock?: ControlledClockOptions;
  /** Defaults to controlled-web-session-v1. Candidate profiles require explicit selection. */
  profile?: Profile;
  /**
   * Imported atomically before the initial navigation begins. The v2 execution profile deliberately
   * reuses the controlled-web-session-v1 state artifact and does not define an implicit migration.
   */
  state?: SessionState;
  /** Immutable interception policy for the lifetime of this session. */
  network?: SessionNetworkOptions;
}

export interface RuntimeInfo {
  protocolVersion: 1;
  implementation: {
    name: string;
    version: string;
    source: Record<string, string>;
  };
  capabilities: {
    methods: string[];
    clockModes: string[];
    profiles: string[];
    settlement: boolean;
    settlementLimits: string[];
  };
  limits: {
    maxInboundFrameBytes: number;
    maxActiveEngineRequests: number;
  };
}

export type PersistentWorkPolicy = "report" | "strict";

export interface SettlePolicy {
  persistentWork?: PersistentWorkPolicy;
  maxVirtualTimeNs?: bigint;
  maxControlTurns?: bigint;
  wallIoTimeoutNs?: bigint;
}

export type ClockMode = "real" | "controlled";

export type TimeSurface =
  | "window_timers"
  | "same_event_loop_iframe"
  | "java_script_date"
  | "performance"
  | "host_timestamp"
  | "update_rendering"
  | "animation_frame"
  | "document_timeline"
  | "worker"
  | "worklet"
  | "cross_event_loop_iframe"
  | "cross_event_loop_navigation"
  | "auxiliary_web_view"
  | "resource_thread_io"
  | "external_subscription"
  | "native_media"
  | "embedder_control"
  | "history_traversal";

export type ProducerStability =
  | "not_checkpointed"
  | "busy"
  | "first_empty"
  | "stable_empty"
  | "unqualified";

export type NetworkKind =
  | "navigation"
  | "fetch"
  | "xml_http_request"
  | "image"
  | "font"
  | "stylesheet"
  | "script"
  | "unclassified_producer_io"
  | "other";

export type ExternalIoPhase =
  | "queued"
  | "awaiting_response"
  | "streaming_body"
  | "terminal_task_queued";

export type ExternalIoOwner =
  | "top_level_navigation"
  | "document_parser"
  | "script"
  | "document_subresource"
  | "rendering_resource"
  | "other";

export type LoadBlocking = "blocking" | "non_blocking" | "unknown";

export interface ExternalIoSnapshot {
  /** Canonical-decimal, session-local alias. Treat as opaque; it is not an engine allocator ID. */
  sourceId: string;
  kind: NetworkKind;
  phase: ExternalIoPhase;
  owner: ExternalIoOwner;
  loadBlocking: LoadBlocking;
  startedAtNs: bigint;
}

export type SourceKind =
  | "task"
  | "microtask"
  | "timer"
  | "animation_frame"
  | "animation"
  | "network"
  | "parser"
  | "rendering_update"
  | "tracked_presence"
  | "other";

export type PersistentReason =
  | "interval"
  | "infinite_animation"
  | "infinite_animated_image";

export type OpenEndedReason =
  | "interval"
  | "infinite_animation"
  | "web_socket"
  | "event_source"
  | "broadcast_channel"
  | "message_port"
  | "embedder_control"
  | "media_session_action_handler"
  | "storage_event_listener";

export type UnsupportedReason =
  | "time_surface"
  | "unclassified_timer"
  | "unclassified_animation"
  | "animated_image"
  | "web_socket"
  | "event_source"
  | "broadcast_channel"
  | "message_port"
  | "embedder_control"
  | "media_session_action_handler"
  | "storage_event_listener"
  | "clock_not_controlled"
  | "canvas_upload"
  | "font_load"
  | "image_load"
  | "inactive_rendering"
  | "throttled_rendering"
  | "ineligible_logical_timer"
  | "throttled_task"
  | "inactive_task"
  | "cross_event_loop_document"
  | "worker"
  | "worklet"
  | "media_element"
  | "graphics_source"
  | "storage_backend"
  | "service_worker"
  | "external_subscription"
  | "untracked_callback"
  | "script_created_parser_input"
  | "suspended_parser";

export interface PersistentDescription {
  reason: PersistentReason;
  /** Registration-requested cadence, before any runtime clamping or throttling. */
  requestedPeriodNs?: bigint;
}

export interface OpenEndedDescription {
  reason: OpenEndedReason;
  /** Registration-requested cadence, before any runtime clamping or throttling. */
  requestedPeriodNs?: bigint;
}

export interface UnsupportedDescription {
  reason: UnsupportedReason;
  timeSurface?: TimeSurface;
}

export type SourceState =
  | { state: "inert" }
  | { state: "ready" }
  | { state: "finite_deadline"; deadlineNs: bigint }
  | { state: "finite_rendering_opportunity" }
  | {
      state: "awaiting_external_io";
      owner: ExternalIoOwner;
      loadBlocking: LoadBlocking;
    }
  | { state: "open_ended"; openEnded: OpenEndedDescription }
  | { state: "unsupported"; unsupported: UnsupportedDescription };

export type SourceSnapshot = {
  /** Canonical-decimal, session-local alias. Treat as opaque; it is not an engine allocator ID. */
  sourceId: string;
  kind: SourceKind;
} & SourceState;

export type RuntimeFailureComponent =
  | "clock"
  | "target_time"
  | "scheduler"
  | "producer"
  | "microtasks"
  | "input_revision"
  | "source_identity"
  | "logical_timer"
  | "animated_image_timer"
  | "dom_generation"
  | "state_generation"
  | "navigation_revision"
  | "pipeline_membership_revision"
  | "source_epoch";

export interface PendingSnapshot {
  stateGeneration: bigint;
  domEpoch: bigint;
  virtualTimeNs: bigint;
  clock: {
    mode: ClockMode;
    /** Normalized by the SDK from either current singular or planned plural wire spelling. */
    unsupportedSurfaces: TimeSurface[];
  };
  input: {
    readyEvents: bigint;
    intakeSaturated: boolean;
    tasks: { ready: bigint; throttled: bigint; inactive: bigint };
  };
  microtasks: {
    queued: bigint;
    checkpointInProgress: boolean;
    terminal: boolean;
  };
  producers: {
    pending: bigint;
    stability: ProducerStability;
    terminal: boolean;
  };
  timers: {
    ready: bigint;
    futureFinite: bigint;
    persistent: bigint;
    unsupported: bigint;
    nextDeadlineNs?: bigint;
  };
  parser: {
    total: bigint;
    ready: bigint;
    awaitingExternalIo: bigint;
    awaitingCommit: bigint;
    awaitingScriptInput: bigint;
    suspended: bigint;
  };
  network: {
    counts: {
      navigation: bigint;
      fetch: bigint;
      xmlHttpRequest: bigint;
      image: bigint;
      font: bigint;
      stylesheet: bigint;
      script: bigint;
      unclassifiedProducerIo: bigint;
      other: bigint;
    };
    active: ExternalIoSnapshot[];
  };
  rendering: {
    opportunityReady: boolean;
    nextOpportunityNs?: bigint;
    retainedAnimationFrames: bigint;
    runnableAnimationFrames: bigint;
    updateRequired: boolean;
    pendingAnimationEvents: bigint;
    finiteAnimations: bigint;
    persistentAnimations: bigint;
    unsupportedAnimations: bigint;
    finiteAnimatedImages: bigint;
    persistentAnimatedImages: bigint;
    unsupportedAnimatedImages: bigint;
    imageUpdateReady: boolean;
    dirtyCanvases: bigint;
    canvasUploadPending: boolean;
    unsupportedCanvases: bigint;
    pendingFonts: bigint;
    pendingImages: bigint;
  };
  sourceEpoch: bigint;
  sources: SourceSnapshot[];
  runtimeFailures: Array<{
    component: RuntimeFailureComponent;
    occurrences: bigint;
  }>;
}

export interface EffectiveSettlePolicy {
  persistentWork: PersistentWorkPolicy;
  maxVirtualTimeNs: bigint;
  maxControlTurns: bigint;
  wallIoTimeoutNs: bigint;
}

export interface PersistentWork {
  /** Canonical-decimal, session-local alias when this entry describes one concrete source. */
  sourceId?: string;
  kind: SourceKind;
  /** Present on aggregate wire projections; normalized to one for per-source projections. */
  count: bigint;
  reason: PersistentReason;
  /** Registration-requested cadence, before any runtime clamping or throttling. */
  requestedPeriodNs?: bigint;
}

export interface UnsupportedWork {
  /** Canonical-decimal, session-local alias when this entry describes one concrete source. */
  sourceId?: string;
  kind: SourceKind;
  /** Present on aggregate wire projections; normalized to one for per-source projections. */
  count: bigint;
  reason: UnsupportedReason;
  timeSurface?: TimeSurface;
}

export type SettleOutcome =
  | "quiescent"
  | "quiescent_with_persistent_work"
  | "blocked_on_external_io"
  | "blocked_on_open_ended_work"
  | "unsupported_work"
  | "virtual_time_limit_exceeded"
  | "task_limit_exceeded"
  | "microtask_limit_exceeded"
  | "rendering_limit_exceeded"
  | "mutation_limit_exceeded"
  | "control_turn_limit_exceeded"
  | "runtime_error";

export type SettleLimitKind =
  | "virtual_time"
  | "ordinary_tasks"
  | "microtasks"
  | "rendering_opportunities"
  | "mutations"
  | "control_turns";

export type SettleLimit =
  | {
      kind: "virtual_time";
      limit: bigint;
      startVirtualTimeNs: bigint;
      requestedVirtualTimeNs: bigint;
    }
  | {
      kind: "control_turns";
      limit: bigint;
      observed?: never;
      startVirtualTimeNs?: never;
      requestedVirtualTimeNs?: never;
    }
  | {
      kind: "ordinary_tasks" | "microtasks" | "rendering_opportunities" | "mutations";
      limit: bigint;
      observed: bigint;
      startVirtualTimeNs?: never;
      requestedVirtualTimeNs?: never;
    };

export type SettleFailureCode =
  | "runtime_terminals"
  | "execution_counter_overflow"
  | "clock_not_controlled"
  | "unsupported_clock_surface"
  | "web_view_identity_changed"
  | "clock_identity_changed"
  | "virtual_time_regressed"
  | "unsupported_source"
  | "unsupported_open_ended_source"
  | "unsupported_rendering"
  | "unsupported_retained_tasks"
  | "ineligible_logical_timer_head"
  | "inconsistent_pending_evidence"
  | "missing_finite_scheduler_head"
  | "unclassified_scheduler_head"
  | "missing_advance_authority"
  | "mismatched_advance_authority"
  | "quiet_checkpoint_did_not_advance";

interface SettleResultBase {
  virtualTimeNs: bigint;
  wallTimeNs: bigint;
  stateGeneration: bigint;
  domEpoch: bigint;
  effectivePolicy: EffectiveSettlePolicy;
  processed: {
    controlTurns: bigint;
    tasks: bigint;
    microtasks: bigint;
    renderingOpportunities: bigint;
    mutations: bigint;
  };
  snapshot: PendingSnapshot;
  persistentWork: PersistentWork[];
  externalIo: ExternalIoSnapshot[];
  unsupportedWork: UnsupportedWork[];
}

type SettleSuccessOutcome =
  | "quiescent"
  | "quiescent_with_persistent_work"
  | "blocked_on_external_io"
  | "blocked_on_open_ended_work";

export type SettleResult =
  | (SettleResultBase & { outcome: SettleSuccessOutcome; limit?: never; failure?: never })
  | (SettleResultBase & {
      outcome: "unsupported_work" | "runtime_error";
      failure: { code: SettleFailureCode };
      limit?: never;
    })
  | (SettleResultBase & {
      outcome: "virtual_time_limit_exceeded";
      limit: Extract<SettleLimit, { kind: "virtual_time" }>;
      failure?: never;
    })
  | (SettleResultBase & {
      outcome: "control_turn_limit_exceeded";
      limit: Extract<SettleLimit, { kind: "control_turns" }>;
      failure?: never;
    })
  | (SettleResultBase & {
      outcome:
        | "task_limit_exceeded"
        | "microtask_limit_exceeded"
        | "rendering_limit_exceeded"
        | "mutation_limit_exceeded";
      limit: Extract<
        SettleLimit,
        { kind: "ordinary_tasks" | "microtasks" | "rendering_opportunities" | "mutations" }
      >;
      failure?: never;
    });

export type AdvanceToNextResult =
  | {
      outcome: "advanced";
      fromVirtualTimeNs: bigint;
      virtualTimeNs: bigint;
      stateGeneration: bigint;
      snapshot: PendingSnapshot;
    }
  | {
      outcome: "no_finite_deadline";
      virtualTimeNs: bigint;
      stateGeneration: bigint;
      snapshot: PendingSnapshot;
    };

/** Authoritative state after a v0.2 session automation mutation. */
export interface SessionAutomationMutationResult extends AutomationMutationResult {
  stateToken: DocumentStateToken;
}

export interface SessionFocusResult extends SessionAutomationMutationResult {
  focused: boolean;
}

export interface SessionCheckResult extends SessionAutomationMutationResult {
  changed: boolean;
  checked: boolean;
}

export interface SessionSelectResult extends SessionAutomationMutationResult {
  changed: boolean;
  values: string[];
}

export interface SessionSubmitResult extends SessionAutomationMutationResult {
  submitted: true;
}

export interface SessionQueryResult extends QueryResult {
  stateToken: DocumentStateToken;
}

export interface SessionTextResult {
  value: string;
  stateGeneration: bigint;
  stateToken: DocumentStateToken;
}

export type SessionExtractRead = ExtractRead | "attribute" | "resolved_url";

export type SessionExtractField =
  | {
      readonly name: string;
      readonly selector: string;
      readonly read: ExtractRead;
      readonly attribute?: never;
    }
  | {
      readonly name: string;
      readonly selector: string;
      readonly read: "attribute" | "resolved_url";
      readonly attribute: string;
    };

export interface SessionExtractPlan {
  readonly rootSelector: string;
  readonly fields: readonly SessionExtractField[];
}

export interface SessionExtractValue {
  name: string;
  value: string | null;
}

export interface SessionExtractRow {
  /** Field order is preserved exactly as requested. */
  fields: SessionExtractValue[];
}

export interface SessionExtractResult {
  /** Root match order and each row's field order are preserved. */
  rows: SessionExtractRow[];
  stateGeneration: bigint;
  stateToken: DocumentStateToken;
}

export interface SessionPendingSnapshot extends PendingSnapshot {
  stateToken: DocumentStateToken;
}

declare const sessionSettleProfileBrand: unique symbol;

declare class BoundSessionSettleProfile<Profile extends SelectableSessionProfile> {
  private readonly sessionSettleProfileBrand: Profile;
}

type SessionSettleProfileIdentity<Profile extends SelectableSessionProfile> =
  Profile extends SessionSupportProfile
    ? { readonly [sessionSettleProfileBrand]?: Profile }
    : BoundSessionSettleProfile<Profile>;

export type SessionSettleResult<
  Profile extends SelectableSessionProfile = SessionSupportProfile,
> = SettleResult & {
  stateToken: DocumentStateToken;
  snapshot: SessionPendingSnapshot;
} & SessionSettleProfileIdentity<Profile>;

export type SessionAdvanceToNextResult = AdvanceToNextResult & {
  stateToken: DocumentStateToken;
  snapshot: SessionPendingSnapshot;
};

export interface SessionNavigateResult {
  requestedUrl: string;
  url: string;
  boundary: "controlled_ready";
  stateGeneration: bigint;
  domEpoch: bigint;
  documentEpoch: bigint;
  navigationId: bigint;
  historyRevision: bigint;
  stateToken: DocumentStateToken;
}

export interface SessionCookiesResult {
  cookies: SessionCookie[];
  sessionStateToken: SessionStateToken;
}

export interface SessionStorageResult {
  origins: SessionOriginState[];
  sessionStateToken: SessionStateToken;
}

export interface SessionStateExportResult {
  state: SessionState;
  sessionStateToken: SessionStateToken;
}

export interface SessionStateMutationResult {
  sessionStateToken: SessionStateToken;
}

export interface SessionAuditOptions extends CommandOptions {
  /** Return entries strictly after this session-local cursor. */
  afterSeq?: bigint;
  /** Bounded page request. The runtime may return fewer entries. */
  limit?: number;
}

export interface SessionAuditBounds {
  maxRecords: number;
  maxMetadataBytes: number;
  maxPageItems: number;
}

export interface SessionSafeUrl {
  origin: string;
  path: string;
  /** UTF-8-byte-sorted unique names only. Query values are intentionally unavailable. */
  queryKeys: string[];
}

/** Bounded request metadata. Header, query, cookie, credential, and body values are absent. */
export interface SessionRequestRecord {
  seq: bigint;
  requestId: string;
  redirectParentId?: string;
  method: string;
  url: SessionSafeUrl;
  resourceKind: NetworkKind;
  mainFrame: boolean;
  headerNames: string[];
  bodyBytes: bigint;
}

export interface SessionRequestsResult {
  records: SessionRequestRecord[];
  firstRetainedSeq?: bigint;
  nextAfterSeq?: bigint;
  latestSeq?: bigint;
  complete: boolean;
  hasMore: boolean;
  droppedThroughSeq?: bigint;
  bounds: SessionAuditBounds;
  stateToken: DocumentStateToken;
}

/** Stable failure vocabulary for schema-2 request and navigation evidence. */
export type SessionEvidenceFailureReason =
  | "blocked_by_fixture"
  | "fixture_miss"
  | "cancelled"
  | "connection_reset"
  | "network_error"
  | "navigation_error"
  | "document_transition_limit_exceeded"
  | "redirect_limit_exceeded"
  | "history_limit_exceeded";

export type SessionEvidenceEvent =
  | { kind: "request_started"; requestId: string }
  | {
      kind: "route_decided";
      requestId: string;
      decision: "fixture_fulfill" | "fixture_abort" | "live";
    }
  | { kind: "response_headers"; requestId: string; status: number }
  | { kind: "redirect"; requestId: string; nextRequestId: string }
  | { kind: "request_completed"; requestId: string }
  | {
      kind: "request_failed";
      requestId: string;
      reason: SessionEvidenceFailureReason;
    }
  | { kind: "navigation_started"; navigationId: bigint }
  | { kind: "navigation_committed"; navigationId: bigint }
  | {
      kind: "navigation_failed";
      navigationId: bigint;
      reason: SessionEvidenceFailureReason;
    }
  | { kind: "same_document_history_changed"; navigationId: bigint }
  | { kind: "settlement_terminal"; navigationId: bigint };

export type SessionEvidenceRecord = {
  seq: bigint;
  atVirtualNs: bigint;
} & SessionEvidenceEvent;

/** Bounded, redacted session audit evidence. Sensitive values never enter this shape. */
export interface SessionEvidenceResult {
  schemaVersion: 2;
  records: SessionEvidenceRecord[];
  firstRetainedSeq?: bigint;
  nextAfterSeq?: bigint;
  latestSeq?: bigint;
  complete: boolean;
  hasMore: boolean;
  droppedThroughSeq?: bigint;
  bounds: SessionAuditBounds;
  stateToken: DocumentStateToken;
}
