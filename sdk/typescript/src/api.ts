import { readFileSync } from "node:fs";

import { StasisAbortError, StasisProcessError, StasisStateError } from "./errors.js";
import { ProtocolClient } from "./protocol.js";
import {
  METHOD,
  decodeActivation,
  decodeAdvanceToNext,
  decodeClose,
  decodeEvaluation,
  decodeOpenResult,
  decodePending,
  decodeRuntimeInfo,
  decodeSettle,
  decodeText,
  encodeDocumentTargetParams,
  encodeOpenParams,
  encodeSettleParams,
  type OpenResult,
} from "./wire.js";
import type {
  AdvanceToNextResult,
  CommandOptions,
  LaunchOptions,
  OpenOptions,
  PendingSnapshot,
  RuntimeInfo,
  SettlePolicy,
  SettleResult,
} from "./types.js";

const DEFAULT_MAX_STDERR_BYTES = 64 * 1024;
const DEFAULT_MAX_FRAME_BYTES = 1024 * 1024;
const DEFAULT_CLOSE_TIMEOUT_MS = 30_000;
const SDK_VERSION = readPackageVersion();

type RuntimeState = "ready" | "opening" | "open" | "closed";

export async function launch(options: LaunchOptions): Promise<Runtime> {
  if (typeof options.executablePath !== "string" || options.executablePath.length === 0) {
    throw new TypeError("executablePath must be a non-empty string");
  }
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
  const closeTimeoutMs = boundedSize(
    options.closeTimeoutMs ?? DEFAULT_CLOSE_TIMEOUT_MS,
    "closeTimeoutMs",
    false,
  );

  let client: ProtocolClient;
  try {
    client = ProtocolClient.spawn({
      executablePath: options.executablePath,
      args: options.args ?? [],
      ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
      ...(options.env === undefined ? {} : { env: options.env }),
      maxStderrBytes,
      maxFrameBytes,
      closeTimeoutMs,
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
        ...(options.signal === undefined ? {} : { signal: options.signal }),
      },
      decodeRuntimeInfo,
    );
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
      const params = encodeOpenParams(url, options.clock);
      const expectedClockMode = params.clockMode === "controlled" ? "controlled" : "real";
      const response = await this.#client.request(
        METHOD.open,
        params,
        {
          sessionId: null,
          expectedResponseSessionId: "<open>",
          ...(options.signal === undefined ? {} : { signal: options.signal }),
        },
        (value, sessionId) => decodeOpenResult(value, sessionId, expectedClockMode),
      );
      this.#state = "open";
      return App.create(this, this.#client, response.result);
    } catch (error) {
      this.#state = "ready";
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

  private constructor(runtime: Runtime, client: ProtocolClient, open: OpenResult) {
    this.#runtime = runtime;
    this.#client = client;
    this.#sessionId = open.sessionId;
    this.requestedUrl = open.requestedUrl;
    this.url = open.url;
    this.boundary = open.boundary;
    this.clockMode = open.clockMode;
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
      this.#requestOptions(options),
      decodeEvaluation,
    );
    return result;
  }

  /** Activate the exact-one element matched by a native CSS selector. */
  async activate(
    selector: string,
    expectedGeneration: bigint,
    options: CommandOptions = {},
  ): Promise<void> {
    this.#assertOpen();
    this.#assertMethod(METHOD.activate);
    const { result } = await this.#client.request(
      METHOD.activate,
      encodeDocumentTargetParams(selector, expectedGeneration),
      this.#requestOptions(options),
      decodeActivation,
    );
    void result;
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
      this.#requestOptions(options),
      decodeText,
    );
    return result;
  }

  async pending(options: CommandOptions = {}): Promise<PendingSnapshot> {
    this.#assertOpen();
    const { result } = await this.#client.request(
      METHOD.pending,
      {},
      this.#requestOptions(options),
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
      this.#requestOptions(options),
      decodeSettle,
    );
    return result;
  }

  async advanceToNext(options: CommandOptions = {}): Promise<AdvanceToNextResult> {
    this.#assertOpen();
    const { result } = await this.#client.request(
      METHOD.advanceToNext,
      {},
      this.#requestOptions(options),
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
          ...this.#requestOptions(options),
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

  #requestOptions(options: CommandOptions): {
    sessionId: string;
    expectedResponseSessionId: string;
    signal?: AbortSignal;
  } {
    return {
      sessionId: this.#sessionId,
      expectedResponseSessionId: this.#sessionId,
      ...(options.signal === undefined ? {} : { signal: options.signal }),
    };
  }
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
