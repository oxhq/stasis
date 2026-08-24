export interface StasisErrorOptions {
  cause?: unknown;
}

export class StasisError extends Error {
  readonly stderrTail: string;

  constructor(message: string, stderrTail = "", options?: StasisErrorOptions) {
    super(message, options);
    this.name = new.target.name;
    this.stderrTail = stderrTail;
  }
}

export class StasisTransportError extends StasisError {
  readonly code: string;

  constructor(code: string, message: string, stderrTail = "", options?: StasisErrorOptions) {
    super(message, stderrTail, options);
    this.code = code;
  }
}

export class StasisProcessError extends StasisTransportError {
  readonly exitCode: number | null;
  readonly signal: string | null;

  constructor(
    message: string,
    stderrTail: string,
    exitCode: number | null = null,
    signal: string | null = null,
    options?: StasisErrorOptions,
  ) {
    super("process_exit", message, stderrTail, options);
    this.exitCode = exitCode;
    this.signal = signal;
  }
}

export type ProtocolStateEffect = "none" | "partial" | "indeterminate";

export type ProtocolErrorDetailValue =
  | null
  | boolean
  | number
  | string
  | readonly ProtocolErrorDetailValue[]
  | ProtocolErrorDetails;

export interface ProtocolErrorDetails {
  readonly [key: string]: ProtocolErrorDetailValue;
}

export class StasisProtocolError extends StasisError {
  readonly code: string;
  readonly fatal: boolean;
  readonly stateEffect: ProtocolStateEffect;
  readonly requestId: string | null;
  readonly sessionId: string | null;
  readonly details: ProtocolErrorDetails | undefined;

  constructor(options: {
    code: string;
    message: string;
    fatal: boolean;
    stateEffect: ProtocolStateEffect;
    requestId: string | null;
    sessionId: string | null;
    stderrTail: string;
    details: ProtocolErrorDetails | undefined;
  }) {
    super(options.message, options.stderrTail);
    this.code = options.code;
    this.fatal = options.fatal;
    this.stateEffect = options.stateEffect;
    this.requestId = options.requestId;
    this.sessionId = options.sessionId;
    this.details = options.details;
  }
}

export class StasisAbortError extends StasisError {
  readonly code = "aborted";
  readonly reason: unknown;
  readonly fatal: boolean;
  readonly stateEffect: ProtocolStateEffect;
  readonly method: string | null;
  readonly requestId: string | null;

  constructor(
    reason: unknown,
    stderrTail = "",
    options: {
      fatal?: boolean;
      stateEffect?: ProtocolStateEffect;
      method?: string | null;
      requestId?: string | null;
    } = {},
  ) {
    super("The Stasis operation was aborted", stderrTail, { cause: reason });
    this.name = "AbortError";
    this.reason = reason;
    this.fatal = options.fatal ?? false;
    this.stateEffect = options.stateEffect ?? "none";
    this.method = options.method ?? null;
    this.requestId = options.requestId ?? null;
  }
}

/** A written native command exceeded its mandatory wall-clock supervision bound. */
export class StasisCommandTimeoutError extends StasisTransportError {
  readonly fatal = true;
  readonly stateEffect: ProtocolStateEffect;
  readonly method: string;
  readonly requestId: string;
  readonly timeoutMs: number;

  constructor(options: {
    method: string;
    requestId: string;
    timeoutMs: number;
    stateEffect: ProtocolStateEffect;
    stderrTail: string;
  }) {
    super(
      "command_timeout",
      `Stasis command ${options.method} did not complete within ${options.timeoutMs} ms`,
      options.stderrTail,
    );
    this.stateEffect = options.stateEffect;
    this.method = options.method;
    this.requestId = options.requestId;
    this.timeoutMs = options.timeoutMs;
  }
}

export class StasisStateError extends StasisError {
  readonly code = "invalid_sdk_state";
}
