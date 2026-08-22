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

export class StasisProtocolError extends StasisError {
  readonly code: string;
  readonly fatal: boolean;
  readonly stateEffect: ProtocolStateEffect;
  readonly requestId: string | null;
  readonly sessionId: string | null;

  constructor(options: {
    code: string;
    message: string;
    fatal: boolean;
    stateEffect: ProtocolStateEffect;
    requestId: string | null;
    sessionId: string | null;
    stderrTail: string;
  }) {
    super(options.message, options.stderrTail);
    this.code = options.code;
    this.fatal = options.fatal;
    this.stateEffect = options.stateEffect;
    this.requestId = options.requestId;
    this.sessionId = options.sessionId;
  }
}

export class StasisAbortError extends StasisError {
  readonly code = "aborted";
  readonly reason: unknown;

  constructor(reason: unknown, stderrTail = "") {
    super("The Stasis operation was aborted", stderrTail, { cause: reason });
    this.name = "AbortError";
    this.reason = reason;
  }
}

export class StasisStateError extends StasisError {
  readonly code = "invalid_sdk_state";
}
