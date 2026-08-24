import { readFileSync } from "node:fs";

import { StasisAbortError, StasisProcessError, StasisStateError } from "./errors.js";
import { ProtocolClient } from "./protocol.js";
import { assertManagedRuntimeIdentity, resolveRuntimeExecutable } from "./runtime-resolver.js";
import { CONTROLLED_WEBAPP_V1_PROFILE, type SupportProfile } from "./profile.js";
import {
  METHOD,
  decodeActivation,
  decodeAdvanceToNext,
  decodeClose,
  decodeEvaluation,
  decodeExtract,
  decodeFill,
  decodeOpenResult,
  decodePending,
  decodeQuery,
  decodeRuntimeInfo,
  decodeSettle,
  decodeText,
  encodeDocumentTargetParams,
  encodeExtractParams,
  encodeFillParams,
  encodeOpenParams,
  encodeSettleParams,
  type OpenResult,
} from "./wire.js";
import type {
  AdvanceToNextResult,
  AutomationMutationResult,
  CommandOptions,
  ExtractPlan,
  ExtractResult,
  LaunchOptions,
  OpenOptions,
  PendingSnapshot,
  QueryResult,
  RuntimeInfo,
  SettlePolicy,
  SettleResult,
} from "./types.js";

const DEFAULT_MAX_STDERR_BYTES = 64 * 1024;
const DEFAULT_MAX_FRAME_BYTES = 1024 * 1024;
const DEFAULT_CLOSE_TIMEOUT_MS = 30_000;
const DEFAULT_COMMAND_TIMEOUT_MS = 30_000;
const MAX_TIMEOUT_MS = 24 * 60 * 60 * 1_000;
const SDK_VERSION = readPackageVersion();

type RuntimeState = "ready" | "opening" | "open" | "closed";

export async function launch(options: LaunchOptions = {}): Promise<Runtime> {
  if (options.signal?.aborted === true) {
    throw new StasisAbortError(options.signal.reason);
  }
  const maxStderrBytes = boundedSize(
    options.maxStderrBytes ?? DEFAULT_MAX_STDERR_BYTES,
    "maxStderrBytes",
    true,
  );
  const maxFrameBytes = boundedSize(
    options.maxFrameBytes ?? DEFAULT_MAX_FRAME_BYTES,
    "maxFrameBytes",
    false,
  );
  const closeTimeoutMs = boundedTimeoutMs(
    options.closeTimeoutMs ?? DEFAULT_CLOSE_TIMEOUT_MS,
    "closeTimeoutMs",
  );
  const commandTimeoutMs = boundedTimeoutMs(
    options.commandTimeoutMs ?? DEFAULT_COMMAND_TIMEOUT_MS,
    "commandTimeoutMs",
  );
  const managedRuntime = options.executablePath === undefined;
  let executablePath: string;
  if (managedRuntime) {
    executablePath = await resolveRuntimeExecutable(SDK_VERSION, {
      ...(options.runtimeCacheDirectory === undefined
        ? {}
        : { cacheDirectory: options.runtimeCacheDirectory }),
      ...(options.signal === undefined ? {} : { signal: options.signal }),
    });
  } else {
    if (typeof options.executablePath !== "string" || options.executablePath.length === 0) {
      throw new TypeError("executablePath must be a non-empty string when provided");
    }
    executablePath = options.executablePath;
  }

  let client: ProtocolClient;
  try {
    client = ProtocolClient.spawn({
      executablePath,
      args: options.args ?? [],
      ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
      ...(options.env === undefined ? {} : { env: options.env }),
      maxStderrBytes,
      maxFrameBytes,
      closeTimeoutMs,
      commandTimeoutMs,
    });
  } catch (error) {
    throw new StasisProcessError("Could not spawn Stasis", "", null, null, { cause: error });
  }

  try {
    const { result } = await client.request(
      METHOD.initialize,
      { client: { name: "@oxhq/stasis", version: SDK_VERSION } },
      {
        sessionId: null,
        expectedResponseSessionId: null,
        timeoutStateEffect: "none",
        ...(options.timeoutMs === undefined
          ? {}
          : { timeoutMs: boundedTimeoutMs(options.timeoutMs, "timeoutMs") }),
        ...(options.signal === undefined ? {} : { signal: options.signal }),
      },
      decodeRuntimeInfo,
    );
    if (managedRuntime) assertManagedRuntimeIdentity(SDK_VERSION, result);
    return Runtime.create(client, result);
  } catch (error) {
    await client.terminate().catch(() => undefined);
    throw error;
  }
}

export class Runtime {
  readonly #client: ProtocolClient;
  readonly info: RuntimeInfo;
  #state: RuntimeState = "ready";

  private constructor(client: ProtocolClient, info: RuntimeInfo) {
    this.#client = client;
    this.info = info;
  }

  /** @internal */
  static create(client: ProtocolClient, info: RuntimeInfo): Runtime {
    return new Runtime(client, info);
  }

  get pid(): number | undefined {
    return this.#client.pid;
  }

  get stderrTail(): string {
    return this.#client.stderrTail;
  }

  async open(url: string | URL, options: OpenOptions = {}): Promise<App> {
    if (this.#state !== "ready") {
      throw new StasisStateError("Runtime.open() may be called exactly once", this.stderrTail);
    }
    this.#state = "opening";
    try {
      const params = encodeOpenParams(url, options.clock, options.profile);
      const expectedClockMode = params.clockMode === "controlled" ? "controlled" : "real";
      const expectedProfile =
        expectedClockMode === "controlled" ? CONTROLLED_WEBAPP_V1_PROFILE : null;
      if (
        expectedProfile !== null &&
        !this.info.capabilities.profiles.includes(expectedProfile)
      ) {
        throw new StasisStateError(
          `The Stasis runtime did not advertise profile ${expectedProfile}`,
          this.stderrTail,
        );
      }
      const response = await this.#client.request(
        METHOD.open,
        params,
        {
          sessionId: null,
          expectedResponseSessionId: "<open>",
          timeoutStateEffect: "indeterminate",
          ...(options.timeoutMs === undefined
            ? {}
            : { timeoutMs: boundedTimeoutMs(options.timeoutMs, "timeoutMs") }),
          ...(options.signal === undefined ? {} : { signal: options.signal }),
        },
        (value, sessionId) =>
          decodeOpenResult(value, sessionId, expectedClockMode, expectedProfile),
      );
      this.#state = "open";
      return App.create(this, this.#client, response.result);
    } catch (error) {
      this.#state = this.#client.isUsable ? "ready" : "closed";
      throw error;
    }
  }

  /** Abruptly terminates the owned process. Use App.close() for a graceful session close. */
  async close(): Promise<void> {
    this.#state = "closed";
    await this.#client.terminate();
  }

  /** @internal */
  appDidClose(): void {
    this.#state = "closed";
  }
}

export class App {
  readonly #runtime: Runtime;
  readonly #client: ProtocolClient;
  readonly #sessionId: string;
  #closePromise: Promise<void> | null = null;

  readonly requestedUrl: string;
  readonly url: string;
  readonly boundary: "load_complete" | "controlled_ready";
  readonly clockMode: "real" | "controlled";
  readonly profile: SupportProfile | null;

  private constructor(runtime: Runtime, client: ProtocolClient, open: OpenResult) {
    this.#runtime = runtime;
    this.#client = client;
    this.#sessionId = open.sessionId;
    this.requestedUrl = open.requestedUrl;
    this.url = open.url;
    this.boundary = open.boundary;
    this.clockMode = open.clockMode;
    this.profile = open.profile;
  }

  /** @internal */
  static create(runtime: Runtime, client: ProtocolClient, open: OpenResult): App {
    return new App(runtime, client, open);
  }

  get stderrTail(): string {
    return this.#client.stderrTail;
  }

  async evaluate(expression: string, options: CommandOptions = {}): Promise<unknown> {
    this.#assertOpen();
    if (typeof expression !== "string") throw new TypeError("expression must be a string");
    const { result } = await this.#client.request(
      METHOD.evaluate,
      { expression },
      this.#requestOptions(options, "indeterminate"),
      decodeEvaluation,
    );
    return result;
  }

  /** Activate the exact-one element matched by a native CSS selector. */
  async activate(
    selector: string,
    expectedGeneration: bigint,
    options: CommandOptions = {},
  ): Promise<AutomationMutationResult> {
    this.#assertOpen();
    this.#assertMethod(METHOD.activate);
    const { result } = await this.#client.request(
      METHOD.activate,
      encodeDocumentTargetParams(selector, expectedGeneration),
      this.#requestOptions(options, "indeterminate"),
      decodeActivation,
    );
    return result;
  }

  /** Replace the value of the exact-one supported form control matched by a native CSS selector. */
  async fill(
    selector: string,
    value: string,
    expectedGeneration: bigint,
    options: CommandOptions = {},
  ): Promise<AutomationMutationResult> {
    this.#assertOpen();
    this.#assertMethod(METHOD.fill);
    const { result } = await this.#client.request(
      METHOD.fill,
      encodeFillParams(selector, value, expectedGeneration),
      this.#requestOptions(options, "indeterminate"),
      decodeFill,
    );
    return result;
  }

  /** Count selector matches without creating persistent DOM handles. */
  async query(
    selector: string,
    expectedGeneration: bigint,
    options: CommandOptions = {},
  ): Promise<QueryResult> {
    this.#assertOpen();
    this.#assertMethod(METHOD.query);
    const { result } = await this.#client.request(
      METHOD.query,
      encodeDocumentTargetParams(selector, expectedGeneration),
      this.#requestOptions(options, "none"),
      decodeQuery,
    );
    return result;
  }

  /** Read raw textContent from the exact-one element matched by a native CSS selector. */
  async text(
    selector: string,
    expectedGeneration: bigint,
    options: CommandOptions = {},
  ): Promise<string> {
    this.#assertOpen();
    this.#assertMethod(METHOD.text);
    const { result } = await this.#client.request(
      METHOD.text,
      encodeDocumentTargetParams(selector, expectedGeneration),
      this.#requestOptions(options, "none"),
      decodeText,
    );
    return result;
  }

  /** Extract ordered text/HTML fields from every root matched by a native CSS selector. */
  async extract(
    plan: ExtractPlan,
    expectedGeneration: bigint,
    options: CommandOptions = {},
  ): Promise<ExtractResult> {
    this.#assertOpen();
    this.#assertMethod(METHOD.extract);
    const { result } = await this.#client.request(
      METHOD.extract,
      encodeExtractParams(plan, expectedGeneration),
      this.#requestOptions(options, "none"),
      decodeExtract,
    );
    return result;
  }

  async pending(options: CommandOptions = {}): Promise<PendingSnapshot> {
    this.#assertOpen();
    const { result } = await this.#client.request(
      METHOD.pending,
      {},
      this.#requestOptions(options, "none"),
      decodePending,
    );
    return result;
  }

  async settle(policy: SettlePolicy = {}, options: CommandOptions = {}): Promise<SettleResult> {
    this.#assertOpen();
    const params = encodeSettleParams(policy);
    const { result } = await this.#client.request(
      METHOD.settle,
      params,
      this.#requestOptions(options, "indeterminate"),
      decodeSettle,
    );
    return result;
  }

  async advanceToNext(options: CommandOptions = {}): Promise<AdvanceToNextResult> {
    this.#assertOpen();
    const { result } = await this.#client.request(
      METHOD.advanceToNext,
      {},
      this.#requestOptions(options, "indeterminate"),
      decodeAdvanceToNext,
    );
    return result;
  }

  close(options: CommandOptions = {}): Promise<void> {
    if (this.#closePromise !== null) return this.#closePromise;
    this.#closePromise = this.#close(options);
    return this.#closePromise;
  }

  async #close(options: CommandOptions): Promise<void> {
    try {
      const { result } = await this.#client.request(
        METHOD.close,
        {},
        {
          ...this.#requestOptions(options, "indeterminate"),
          terminatesProcess: true,
        },
        (value) => {
          decodeClose(value);
        },
      );
      void result;
      await this.#client.waitForCleanExit(options.signal);
      this.#runtime.appDidClose();
    } catch (error) {
      this.#closePromise = null;
      throw error;
    }
  }

  #assertOpen(): void {
    if (this.#closePromise !== null) {
      throw new StasisStateError("The Stasis app is closing or closed", this.stderrTail);
    }
  }

  #assertMethod(method: string): void {
    if (!this.#runtime.info.capabilities.methods.includes(method)) {
      throw new StasisStateError(
        `The Stasis runtime did not advertise ${method}`,
        this.stderrTail,
      );
    }
  }

  #requestOptions(
    options: CommandOptions,
    timeoutStateEffect: "none" | "indeterminate",
  ): {
    sessionId: string;
    expectedResponseSessionId: string;
    signal?: AbortSignal;
    timeoutMs?: number;
    timeoutStateEffect: "none" | "indeterminate";
  } {
    return {
      sessionId: this.#sessionId,
      expectedResponseSessionId: this.#sessionId,
      timeoutStateEffect,
      ...(options.signal === undefined ? {} : { signal: options.signal }),
      ...(options.timeoutMs === undefined
        ? {}
        : { timeoutMs: boundedTimeoutMs(options.timeoutMs, "timeoutMs") }),
    };
  }
}

function boundedTimeoutMs(value: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > MAX_TIMEOUT_MS) {
    throw new RangeError(`${label} must be a safe integer between 1 and ${MAX_TIMEOUT_MS} ms`);
  }
  return value;
}

function boundedSize(value: number, label: string, allowZero: boolean): number {
  if (
    !Number.isSafeInteger(value) ||
    value < (allowZero ? 0 : 1) ||
    value > 1024 * 1024 * 1024
  ) {
    throw new RangeError(`${label} must be a safe integer between ${allowZero ? 0 : 1} and 1 GiB`);
  }
  return value;
}

function readPackageVersion(): string {
  const value = JSON.parse(
    readFileSync(new URL("../package.json", import.meta.url), "utf8"),
  ) as unknown;
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError("@oxhq/stasis package metadata must be an object");
  }
  const version = (value as Record<string, unknown>).version;
  if (typeof version !== "string" || version.length === 0) {
    throw new TypeError("@oxhq/stasis package metadata must contain a version");
  }
  return version;
}
