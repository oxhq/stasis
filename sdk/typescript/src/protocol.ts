import { spawn } from "node:child_process";
import type { ChildProcessWithoutNullStreams, SpawnOptionsWithoutStdio } from "node:child_process";

import {
  StasisAbortError,
  StasisCommandTimeoutError,
  StasisProcessError,
  StasisProtocolError,
  StasisStateError,
  StasisTransportError,
  type ProtocolStateEffect,
} from "./errors.js";

const PROTOCOL_VERSION = 1;
const MAX_REQUEST_FRAME_BYTES = 1024 * 1024;
const MAX_U128 = (1n << 128n) - 1n;
const ABRUPT_SIGTERM_GRACE_MS = 250;
const MIN_ABRUPT_TERMINATION_TIMEOUT_MS = 1_000;
const UNEXPECTED_STDOUT_EOF_GRACE_MS = 100;

type SessionExpectation = null | string | "<open>";

interface RequestOptions {
  sessionId: string | null;
  expectedResponseSessionId: SessionExpectation;
  signal?: AbortSignal;
  terminatesProcess?: boolean;
  timeoutMs?: number;
  timeoutStateEffect: ProtocolStateEffect;
}

interface QueuedRequest {
  method: string;
  params: Record<string, unknown>;
  options: RequestOptions;
  state: "queued" | "active" | "settled";
  id: string | null;
  abortListener: (() => void) | null;
  timeoutTimer: ReturnType<typeof setTimeout> | null;
  decode: (result: unknown, sessionId: string | null) => unknown;
  resolve: (result: ProtocolSuccess) => void;
  reject: (error: unknown) => void;
}

export interface ProtocolSuccess<Result = unknown> {
  result: Result;
  sessionId: string | null;
}

interface ProtocolErrorPayload {
  code: string;
  message: string;
  fatal: boolean;
  stateEffect: ProtocolStateEffect;
}

class WireViolation extends Error {
  readonly code: string;

  constructor(code: string, message: string, options?: ErrorOptions) {
    super(message, options);
    this.code = code;
  }
}

class StderrTail {
  readonly #maximumBytes: number;
  #bytes = Buffer.alloc(0);

  constructor(maximumBytes: number) {
    this.#maximumBytes = maximumBytes;
  }

  append(chunk: Buffer): void {
    if (this.#maximumBytes === 0 || chunk.length === 0) return;
    if (chunk.length >= this.#maximumBytes) {
      this.#bytes = Buffer.from(chunk.subarray(chunk.length - this.#maximumBytes));
      return;
    }
    const combined = Buffer.concat([this.#bytes, chunk]);
    this.#bytes =
      combined.length <= this.#maximumBytes
        ? combined
        : Buffer.from(combined.subarray(combined.length - this.#maximumBytes));
  }

  snapshot(): string {
    return this.#bytes.toString("utf8");
  }
}

class NdjsonDecoder {
  readonly #maximumFrameBytes: number;
  readonly #onFrame: (value: unknown) => void;
  #buffer = Buffer.alloc(0);
  #failed = false;

  constructor(maximumFrameBytes: number, onFrame: (value: unknown) => void) {
    this.#maximumFrameBytes = maximumFrameBytes;
    this.#onFrame = onFrame;
  }

  push(chunk: Buffer): void {
    if (this.#failed) return;
    let offset = 0;
    try {
      while (offset < chunk.length) {
        const newline = chunk.indexOf(0x0a, offset);
        const end = newline === -1 ? chunk.length : newline;
        this.#append(chunk.subarray(offset, end));
        if (newline === -1) break;
        this.#emitFrame();
        offset = newline + 1;
      }
    } catch (error) {
      this.#failed = true;
      throw error;
    }
  }

  finish(): void {
    if (!this.#failed && this.#buffer.length !== 0) {
      this.#failed = true;
      throw new WireViolation(
        "incomplete_frame",
        "Stasis stdout ended before the NDJSON frame terminator",
      );
    }
  }

  #append(fragment: Buffer): void {
    const maximumBuffered = this.#maximumFrameBytes + 1;
    if (fragment.length > maximumBuffered - this.#buffer.length) {
      throw new WireViolation(
        "frame_too_large",
        `Stasis stdout frame exceeds ${this.#maximumFrameBytes} bytes`,
      );
    }
    if (fragment.length !== 0) {
      this.#buffer = Buffer.concat([this.#buffer, fragment]);
    }
    if (
      this.#buffer.length === maximumBuffered &&
      this.#buffer[this.#buffer.length - 1] !== 0x0d
    ) {
      throw new WireViolation(
        "frame_too_large",
        `Stasis stdout frame exceeds ${this.#maximumFrameBytes} bytes`,
      );
    }
  }

  #emitFrame(): void {
    let frame = this.#buffer;
    this.#buffer = Buffer.alloc(0);
    if (frame[frame.length - 1] === 0x0d) frame = frame.subarray(0, frame.length - 1);
    if (frame.length > this.#maximumFrameBytes) {
      throw new WireViolation(
        "frame_too_large",
        `Stasis stdout frame exceeds ${this.#maximumFrameBytes} bytes`,
      );
    }
    if (frame.length === 0 || frame.every((byte) => isAsciiWhitespace(byte))) {
      throw new WireViolation("empty_frame", "Stasis stdout emitted an empty NDJSON frame");
    }
    if (frame.length >= 3 && frame[0] === 0xef && frame[1] === 0xbb && frame[2] === 0xbf) {
      throw new WireViolation("unexpected_bom", "UTF-8 BOM is not allowed on Stasis stdout");
    }

    let text: string;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(frame);
    } catch (error) {
      throw new WireViolation("invalid_utf8", "Stasis stdout emitted invalid UTF-8", {
        cause: error,
      });
    }

    let value: unknown;
    try {
      value = JSON.parse(text) as unknown;
      assertNoDuplicateObjectKeys(text);
    } catch (error) {
      if (error instanceof WireViolation) throw error;
      throw new WireViolation("invalid_json", "Stasis stdout emitted invalid JSON", {
        cause: error,
      });
    }
    this.#onFrame(value);
  }
}

function isAsciiWhitespace(byte: number): boolean {
  return byte === 0x20 || byte === 0x09 || byte === 0x0d || byte === 0x0a;
}

/** JSON.parse accepts duplicate names. This second, bounded pass rejects them at every depth. */
function assertNoDuplicateObjectKeys(text: string): void {
  let index = 0;

  const skipWhitespace = (): void => {
    while (index < text.length && /\s/u.test(text[index] ?? "")) index += 1;
  };

  const scanString = (): string => {
    const start = index;
    index += 1;
    while (index < text.length) {
      const character = text[index];
      if (character === "\\") {
        index += 2;
        continue;
      }
      index += 1;
      if (character === '"') return JSON.parse(text.slice(start, index)) as string;
    }
    throw new SyntaxError("unterminated JSON string");
  };

  const scanValue = (): void => {
    skipWhitespace();
    const character = text[index];
    if (character === "{") {
      index += 1;
      skipWhitespace();
      const names = new Set<string>();
      if (text[index] === "}") {
        index += 1;
        return;
      }
      for (;;) {
        skipWhitespace();
        if (text[index] !== '"') throw new SyntaxError("object name must be a string");
        const name = scanString();
        if (names.has(name)) {
          throw new WireViolation(
            "duplicate_member",
            `Stasis stdout JSON contains duplicate object member ${JSON.stringify(name)}`,
          );
        }
        names.add(name);
        skipWhitespace();
        if (text[index] !== ":") throw new SyntaxError("missing object colon");
        index += 1;
        scanValue();
        skipWhitespace();
        if (text[index] === "}") {
          index += 1;
          return;
        }
        if (text[index] !== ",") throw new SyntaxError("missing object comma");
        index += 1;
      }
    }
    if (character === "[") {
      index += 1;
      skipWhitespace();
      if (text[index] === "]") {
        index += 1;
        return;
      }
      for (;;) {
        scanValue();
        skipWhitespace();
        if (text[index] === "]") {
          index += 1;
          return;
        }
        if (text[index] !== ",") throw new SyntaxError("missing array comma");
        index += 1;
      }
    }
    if (character === '"') {
      scanString();
      return;
    }
    while (index < text.length && !/[\s,}\]]/u.test(text[index] ?? "")) index += 1;
  };

  scanValue();
  skipWhitespace();
  if (index !== text.length) throw new SyntaxError("trailing JSON input");
}

export interface SpawnClientOptions {
  executablePath: string;
  args: readonly string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  maxStderrBytes: number;
  maxFrameBytes: number;
  closeTimeoutMs: number;
  commandTimeoutMs: number;
}

interface ExitWaiter {
  resolve: () => void;
  reject: (error: unknown) => void;
  timer: ReturnType<typeof setTimeout>;
  signal?: AbortSignal;
  abortListener?: () => void;
}

export class ProtocolClient {
  readonly #child: ChildProcessWithoutNullStreams;
  readonly #stderr: StderrTail;
  readonly #decoder: NdjsonDecoder;
  readonly #closeTimeoutMs: number;
  readonly #commandTimeoutMs: number;
  readonly #queue: QueuedRequest[] = [];
  readonly #exitWaiters: ExitWaiter[] = [];
  #active: QueuedRequest | null = null;
  #nextRequestId = 0n;
  #lastWireSequence = 0n;
  #accepting = true;
  #expectedExit = false;
  #gracefulExitExpected = false;
  #abruptTerminationRequested = false;
  #processExited = false;
  #processClosed = false;
  #terminationPromise: Promise<void> | null = null;
  #terminalError: Error | null = null;
  #exitCode: number | null = null;
  #exitSignal: NodeJS.Signals | null = null;

  static spawn(options: SpawnClientOptions): ProtocolClient {
    const spawnOptions: SpawnOptionsWithoutStdio = {
      shell: false,
      stdio: ["pipe", "pipe", "pipe"],
    };
    if (options.cwd !== undefined) spawnOptions.cwd = options.cwd;
    if (options.env !== undefined) spawnOptions.env = options.env;
    const child = spawn(options.executablePath, [...options.args], spawnOptions);
    return new ProtocolClient(
      child,
      options.maxStderrBytes,
      options.maxFrameBytes,
      options.closeTimeoutMs,
      options.commandTimeoutMs,
    );
  }

  private constructor(
    child: ChildProcessWithoutNullStreams,
    maxStderrBytes: number,
    maxFrameBytes: number,
    closeTimeoutMs: number,
    commandTimeoutMs: number,
  ) {
    this.#child = child;
    this.#stderr = new StderrTail(maxStderrBytes);
    this.#closeTimeoutMs = closeTimeoutMs;
    this.#commandTimeoutMs = commandTimeoutMs;
    this.#decoder = new NdjsonDecoder(maxFrameBytes, (frame) => this.#handleFrame(frame));

    child.stderr.on("data", (chunk: Buffer) => this.#stderr.append(chunk));
    child.stdout.on("data", (chunk: Buffer) => {
      try {
        this.#decoder.push(chunk);
      } catch (error) {
        this.#failFromWire(error);
      }
    });
    child.stdout.on("end", () => {
      try {
        this.#decoder.finish();
      } catch (error) {
        this.#failFromWire(error);
        return;
      }
      setTimeout(() => {
        if (
          this.#terminalError === null &&
          !this.#gracefulExitExpected &&
          !this.#abruptTerminationRequested &&
          !this.#processClosed
        ) {
          this.#failStop(
            new StasisTransportError(
              "unexpected_stdout_eof",
              "Stasis stdout ended before a graceful session close",
              this.stderrTail,
            ),
          );
        }
      }, UNEXPECTED_STDOUT_EOF_GRACE_MS);
    });
    child.stdin.on("error", (error) => {
      if (!this.#expectedExit) {
        this.#failStop(
          new StasisTransportError(
            "stdin_error",
            `Could not write to Stasis stdin: ${error.message}`,
            this.stderrTail,
            { cause: error },
          ),
        );
      }
    });
    child.once("error", (error) => {
      this.#failStop(
        new StasisProcessError(
          `Could not start or communicate with Stasis: ${error.message}`,
          this.stderrTail,
          null,
          null,
          { cause: error },
        ),
        false,
      );
    });
    child.once("exit", (code, signal) => {
      this.#processExited = true;
      this.#exitCode = code;
      this.#exitSignal = signal;
      if (this.#terminalError !== null) return;
      if (!this.#expectedExit || (this.#gracefulExitExpected && (code !== 0 || signal !== null))) {
        this.#failStop(
          new StasisProcessError(formatProcessExit(code, signal), this.stderrTail, code, signal),
          false,
        );
      }
    });
    child.once("close", (code, signal) => {
      this.#processClosed = true;
      this.#exitCode = code;
      this.#exitSignal = signal;
      if (this.#terminalError !== null) return;
      if (this.#gracefulExitExpected) {
        if (code === 0 && signal === null) {
          this.#resolveExitWaiters();
        } else {
          this.#failStop(
            new StasisProcessError(formatProcessExit(code, signal), this.stderrTail, code, signal),
            false,
          );
        }
        return;
      }
      if (this.#expectedExit) return;
      this.#failStop(
        new StasisProcessError(
          formatProcessExit(code, signal),
          this.stderrTail,
          code,
          signal,
        ),
        false,
      );
    });
  }

  get pid(): number | undefined {
    return this.#child.pid;
  }

  get stderrTail(): string {
    return this.#stderr.snapshot();
  }

  get isUsable(): boolean {
    return this.#accepting && this.#terminalError === null;
  }

  waitForCleanExit(signal?: AbortSignal): Promise<void> {
    if (this.#terminalError !== null) return Promise.reject(this.#terminalError);
    if (signal?.aborted === true) {
      const error = new StasisAbortError(signal.reason, this.stderrTail, {
        fatal: true,
        stateEffect: "indeterminate",
        method: "session.close",
      });
      this.#failStop(error);
      return Promise.reject(error);
    }
    if (this.#processClosed) {
      if (this.#exitCode === 0 && this.#exitSignal === null) return Promise.resolve();
      return Promise.reject(
        new StasisProcessError(
          formatProcessExit(this.#exitCode, this.#exitSignal),
          this.stderrTail,
          this.#exitCode,
          this.#exitSignal,
        ),
      );
    }

    return new Promise((resolve, reject) => {
      const waiter: ExitWaiter = {
        resolve,
        reject,
        timer: setTimeout(() => {
          const error = new StasisTransportError(
            "close_timeout",
            `Stasis did not exit within ${this.#closeTimeoutMs} ms after session.close`,
            this.stderrTail,
          );
          this.#failStop(error);
        }, this.#closeTimeoutMs),
        ...(signal === undefined ? {} : { signal }),
      };
      if (signal !== undefined) {
        waiter.abortListener = () => {
          this.#failStop(
            new StasisAbortError(signal.reason, this.stderrTail, {
              fatal: true,
              stateEffect: "indeterminate",
              method: "session.close",
            }),
          );
        };
        signal.addEventListener("abort", waiter.abortListener, { once: true });
      }
      this.#exitWaiters.push(waiter);
    });
  }

  request<Result>(
    method: string,
    params: Record<string, unknown>,
    options: RequestOptions,
    decode: (result: unknown, sessionId: string | null) => Result,
  ): Promise<ProtocolSuccess<Result>> {
    if (!this.#accepting) {
      return Promise.reject(
        this.#terminalError ?? new StasisStateError("The Stasis runtime is closed", this.stderrTail),
      );
    }
    if (options.signal?.aborted === true) {
      return Promise.reject(new StasisAbortError(options.signal.reason, this.stderrTail));
    }

    return new Promise((resolve, reject) => {
      const request: QueuedRequest = {
        method,
        params,
        options,
        state: "queued",
        id: null,
        abortListener: null,
        timeoutTimer: null,
        decode,
        resolve: (success) => resolve(success as ProtocolSuccess<Result>),
        reject,
      };
      if (options.signal !== undefined) {
        request.abortListener = () => this.#abort(request, options.signal?.reason);
        options.signal.addEventListener("abort", request.abortListener, { once: true });
      }
      this.#queue.push(request);
      this.#pump();
    });
  }

  terminate(): Promise<void> {
    if (this.#processClosed) return Promise.resolve();
    if (this.#abruptTerminationRequested) return this.#startProcessTermination();
    this.#abruptTerminationRequested = true;
    this.#accepting = false;
    this.#expectedExit = true;
    this.#gracefulExitExpected = false;
    if (this.#terminalError === null) {
      const error = new StasisStateError("The Stasis runtime was closed", this.stderrTail);
      this.#terminalError = error;
      this.#rejectAll(error);
      this.#rejectExitWaiters(error);
    }
    this.#child.stdin.destroy();
    return this.#startProcessTermination();
  }

  #pump(): void {
    if (!this.#accepting || this.#active !== null) return;
    const request = this.#queue.shift();
    if (request === undefined) return;
    if (request.options.signal?.aborted === true) {
      this.#settleRequest(request);
      request.reject(new StasisAbortError(request.options.signal.reason, this.stderrTail));
      this.#pump();
      return;
    }

    const nextId = this.#nextRequestId + 1n;
    if (nextId > MAX_U128) {
      this.#settleRequest(request);
      request.reject(
        new StasisTransportError(
          "request_sequence_exhausted",
          "Stasis SDK request sequence is exhausted",
          this.stderrTail,
        ),
      );
      this.#failStop(
        new StasisTransportError(
          "request_sequence_exhausted",
          "Stasis SDK request sequence is exhausted",
          this.stderrTail,
        ),
      );
      return;
    }
    this.#nextRequestId = nextId;
    request.id = nextId.toString();

    const envelope: Record<string, unknown> = {
      v: PROTOCOL_VERSION,
      type: "request",
      id: request.id,
      method: request.method,
      params: request.params,
    };
    if (request.options.sessionId !== null) envelope.sessionId = request.options.sessionId;

    let frame: Buffer;
    try {
      frame = Buffer.from(`${JSON.stringify(envelope)}\n`, "utf8");
    } catch (error) {
      this.#settleRequest(request);
      request.reject(
        new StasisTransportError(
          "request_encoding_failed",
          `Could not encode Stasis request ${request.method}`,
          this.stderrTail,
          { cause: error },
        ),
      );
      this.#pump();
      return;
    }
    if (frame.length - 1 > MAX_REQUEST_FRAME_BYTES) {
      this.#settleRequest(request);
      request.reject(
        new StasisTransportError(
          "request_frame_too_large",
          `Stasis request exceeds ${MAX_REQUEST_FRAME_BYTES} bytes`,
          this.stderrTail,
        ),
      );
      this.#pump();
      return;
    }

    request.state = "active";
    this.#active = request;
    const timeoutMs = request.options.timeoutMs ?? this.#commandTimeoutMs;
    request.timeoutTimer = setTimeout(() => this.#commandTimedOut(request, timeoutMs), timeoutMs);
    request.timeoutTimer.unref();
    this.#child.stdin.write(frame, (error) => {
      if (error !== null && error !== undefined) {
        this.#failStop(
          new StasisTransportError(
            "stdin_write_failed",
            `Could not write Stasis request ${request.method}: ${error.message}`,
            this.stderrTail,
            { cause: error },
          ),
        );
      }
    });
  }

  #abort(request: QueuedRequest, reason: unknown): void {
    if (request.state === "settled") return;
    if (request.state === "queued") {
      const error = new StasisAbortError(reason, this.stderrTail);
      const index = this.#queue.indexOf(request);
      if (index !== -1) this.#queue.splice(index, 1);
      this.#settleRequest(request);
      request.reject(error);
      return;
    }
    const error = new StasisAbortError(reason, this.stderrTail, {
      fatal: true,
      stateEffect: request.options.timeoutStateEffect,
      method: request.method,
      requestId: request.id,
    });
    this.#failStop(error);
  }

  #commandTimedOut(request: QueuedRequest, timeoutMs: number): void {
    if (request.state !== "active" || this.#active !== request || request.id === null) return;
    this.#failStop(
      new StasisCommandTimeoutError({
        method: request.method,
        requestId: request.id,
        timeoutMs,
        stateEffect: request.options.timeoutStateEffect,
        stderrTail: this.stderrTail,
      }),
    );
  }

  #handleFrame(frame: unknown): void {
    const envelope = asRecord(frame, "response envelope");
    if (envelope.v !== PROTOCOL_VERSION) {
      throw new WireViolation("unsupported_protocol", "Unexpected Stasis protocol version");
    }
    const wireSequence = parseCanonicalU128(envelope.wireSeq, "wireSeq");
    if (wireSequence !== this.#lastWireSequence + 1n) {
      throw new WireViolation(
        "wire_sequence_mismatch",
        `Expected Stasis wireSeq ${this.#lastWireSequence + 1n}, received ${wireSequence}`,
      );
    }
    this.#lastWireSequence = wireSequence;

    if (envelope.type === "event") {
      assertExactKeys(envelope, ["error", "event", "id", "sessionId", "type", "v", "wireSeq"]);
      if (envelope.event !== "protocol.fatal" || envelope.id !== null) {
        throw new WireViolation("unexpected_event", "Unexpected Stasis protocol event");
      }
      const error = this.#protocolError(envelope.error, null, envelope.sessionId);
      this.#failStop(error);
      return;
    }
    if (envelope.type !== "response") {
      throw new WireViolation("invalid_envelope", "Stasis stdout frame is not a response or event");
    }

    const request = this.#active;
    if (request === null || request.id === null) {
      throw new WireViolation("unmatched_response", "Stasis emitted a response with no active request");
    }
    if (envelope.id !== request.id) {
      throw new WireViolation(
        "unmatched_response",
        `Expected Stasis response id ${request.id}, received ${String(envelope.id)}`,
      );
    }
    const sessionId = parseNullableString(envelope.sessionId, "sessionId");

    if (Object.hasOwn(envelope, "result")) {
      assertExactKeys(envelope, ["id", "result", "sessionId", "type", "v", "wireSeq"]);
      this.#validateSession(request.options.expectedResponseSessionId, sessionId, true);
      const decoded = request.decode(envelope.result, sessionId);
      this.#active = null;
      this.#settleRequest(request);
      if (request.options.terminatesProcess === true) {
        this.#accepting = false;
        this.#expectedExit = true;
        this.#gracefulExitExpected = true;
      }
      request.resolve({ result: decoded, sessionId });
      this.#pump();
      return;
    }

    assertExactKeys(envelope, ["error", "event", "id", "sessionId", "type", "v", "wireSeq"]);
    if (envelope.event !== null) {
      throw new WireViolation("invalid_envelope", "A response error must carry event: null");
    }
    this.#validateSession(request.options.expectedResponseSessionId, sessionId, false);
    const error = this.#protocolError(envelope.error, request.id, sessionId);
    this.#active = null;
    this.#settleRequest(request);
    if (error.fatal || error.stateEffect === "indeterminate") {
      request.reject(error);
      this.#failStop(error);
      return;
    }
    request.reject(error);
    this.#pump();
  }

  #protocolError(payload: unknown, requestId: string | null, sessionIdValue: unknown): StasisProtocolError {
    const error = asRecord(payload, "protocol error");
    assertExactKeys(error, ["code", "fatal", "message", "stateEffect"]);
    if (typeof error.code !== "string" || error.code.length === 0) {
      throw new WireViolation("invalid_envelope", "Protocol error code must be a non-empty string");
    }
    if (typeof error.message !== "string" || typeof error.fatal !== "boolean") {
      throw new WireViolation("invalid_envelope", "Protocol error has invalid field types");
    }
    if (!isStateEffect(error.stateEffect)) {
      throw new WireViolation("invalid_envelope", "Protocol error has an invalid stateEffect");
    }
    return new StasisProtocolError({
      code: error.code,
      message: error.message,
      fatal: error.fatal,
      stateEffect: error.stateEffect,
      requestId,
      sessionId: parseNullableString(sessionIdValue, "sessionId"),
      stderrTail: this.stderrTail,
    });
  }

  #validateSession(expectation: SessionExpectation, actual: string | null, success: boolean): void {
    if (expectation === "<open>") {
      if (success && (actual === null || actual.length === 0)) {
        throw new WireViolation("session_mismatch", "session.open did not return a sessionId");
      }
      if (!success && actual !== null) {
        throw new WireViolation("session_mismatch", "Failed session.open returned a sessionId");
      }
      return;
    }
    if (actual !== expectation) {
      throw new WireViolation(
        "session_mismatch",
        `Expected response sessionId ${String(expectation)}, received ${String(actual)}`,
      );
    }
  }

  #failFromWire(error: unknown): void {
    if (this.#terminalError !== null) return;
    if (error instanceof StasisTransportError) {
      this.#failStop(
        new StasisTransportError(error.code, error.message, this.stderrTail, { cause: error }),
      );
      return;
    }
    const violation =
      error instanceof WireViolation
        ? error
        : new WireViolation("invalid_frame", "Could not decode Stasis stdout", { cause: error });
    this.#failStop(
      new StasisTransportError(violation.code, violation.message, this.stderrTail, {
        cause: violation,
      }),
    );
  }

  #failStop(error: Error, kill = true): void {
    if (this.#terminalError !== null) return;
    this.#terminalError = error;
    this.#accepting = false;
    this.#expectedExit = true;
    this.#rejectAll(error);
    this.#rejectExitWaiters(error);
    this.#child.stdin.destroy();
    if (kill) void this.#startProcessTermination().catch(() => undefined);
  }

  #startProcessTermination(): Promise<void> {
    if (this.#processExited || this.#processClosed) return Promise.resolve();
    if (this.#terminationPromise !== null) return this.#terminationPromise;

    this.#terminationPromise = new Promise((resolve, reject) => {
      let escalationTimer: ReturnType<typeof setTimeout> | undefined;
      let timeoutTimer: ReturnType<typeof setTimeout> | undefined;
      const cleanup = (): void => {
        if (escalationTimer !== undefined) clearTimeout(escalationTimer);
        if (timeoutTimer !== undefined) clearTimeout(timeoutTimer);
        this.#child.removeListener("exit", onTermination);
        this.#child.removeListener("close", onTermination);
      };
      const onTermination = (): void => {
        cleanup();
        resolve();
      };

      this.#child.once("exit", onTermination);
      this.#child.once("close", onTermination);
      const escalationDelay = Math.min(ABRUPT_SIGTERM_GRACE_MS, this.#closeTimeoutMs);
      escalationTimer = setTimeout(() => {
        if (!this.#processExited) {
          try {
            this.#child.kill("SIGKILL");
          } catch {
            // The bounded timeout below reports a child that still cannot be reaped.
          }
        }
      }, escalationDelay);
      timeoutTimer = setTimeout(
        () => {
          cleanup();
          reject(
            new StasisTransportError(
              "termination_timeout",
              "Stasis did not exit after SIGTERM and SIGKILL",
              this.stderrTail,
            ),
          );
        },
        Math.max(this.#closeTimeoutMs, MIN_ABRUPT_TERMINATION_TIMEOUT_MS),
      );

      if (this.#child.exitCode === null && this.#child.signalCode === null) {
        try {
          this.#child.kill("SIGTERM");
        } catch {
          // A concurrent termination is observed by the event listeners.
        }
      }
    });
    return this.#terminationPromise;
  }

  #rejectAll(error: Error): void {
    if (this.#active !== null) {
      const active = this.#active;
      this.#active = null;
      this.#settleRequest(active);
      active.reject(error);
    }
    for (const request of this.#queue.splice(0)) {
      this.#settleRequest(request);
      request.reject(error);
    }
  }

  #settleRequest(request: QueuedRequest): void {
    request.state = "settled";
    if (request.abortListener !== null && request.options.signal !== undefined) {
      request.options.signal.removeEventListener("abort", request.abortListener);
    }
    request.abortListener = null;
    if (request.timeoutTimer !== null) clearTimeout(request.timeoutTimer);
    request.timeoutTimer = null;
  }

  #resolveExitWaiters(): void {
    for (const waiter of this.#exitWaiters.splice(0)) {
      cleanupExitWaiter(waiter);
      waiter.resolve();
    }
  }

  #rejectExitWaiters(error: Error): void {
    for (const waiter of this.#exitWaiters.splice(0)) {
      cleanupExitWaiter(waiter);
      waiter.reject(error);
    }
  }
}

function cleanupExitWaiter(waiter: ExitWaiter): void {
  clearTimeout(waiter.timer);
  if (waiter.signal !== undefined && waiter.abortListener !== undefined) {
    waiter.signal.removeEventListener("abort", waiter.abortListener);
  }
}

function asRecord(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new WireViolation("invalid_envelope", `${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function assertExactKeys(value: Record<string, unknown>, expected: readonly string[]): void {
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (actual.length !== wanted.length || actual.some((key, index) => key !== wanted[index])) {
    throw new WireViolation(
      "invalid_envelope",
      `Unexpected response members: ${actual.join(", ")}`,
    );
  }
}

function parseNullableString(value: unknown, label: string): string | null {
  if (value === null || typeof value === "string") return value;
  throw new WireViolation("invalid_envelope", `${label} must be a string or null`);
}

function parseCanonicalU128(value: unknown, label: string): bigint {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/u.test(value)) {
    throw new WireViolation("invalid_decimal", `${label} must be a canonical decimal string`);
  }
  const parsed = BigInt(value);
  if (parsed > MAX_U128) {
    throw new WireViolation("invalid_decimal", `${label} exceeds u128`);
  }
  return parsed;
}

function isStateEffect(value: unknown): value is ProtocolStateEffect {
  return value === "none" || value === "partial" || value === "indeterminate";
}

function formatProcessExit(code: number | null, signal: NodeJS.Signals | null): string {
  if (signal !== null) return `Stasis exited from signal ${signal}`;
  return `Stasis exited with code ${String(code)}`;
}
