import { launch, type Runtime, type Session } from "./api.js";
import { StasisAbortError } from "./errors.js";
import type { LaunchOptions, SessionOpenOptions } from "./types.js";

/** A freshly-created native process and its single terminal session. */
export interface OwnedSessionProcess<SessionType> {
  readonly session: SessionType;
  /** Close the session, observe the native process exit, and discard it. */
  close(): Promise<void>;
  /** Abruptly terminate and discard a poisoned process. */
  terminate(): Promise<void>;
}

export interface SessionProcessFactoryContext {
  readonly signal?: AbortSignal;
}

export type SessionProcessFactory<Request, SessionType> = (
  request: Request,
  context: SessionProcessFactoryContext,
) => Promise<OwnedSessionProcess<SessionType>>;

export interface FreshSessionPoolOptions<Request, SessionType> {
  /** Maximum native processes that may be spawning, leased, or closing at once. */
  readonly maxProcesses: number;
  /** Maximum waiters. Set to zero to reject whenever all process slots are occupied. */
  readonly maxQueue: number;
  /** Must create a fresh process and its only session for every invocation. */
  readonly create: SessionProcessFactory<Request, SessionType>;
}

export interface SessionAcquireOptions {
  /** Cancellation is guaranteed while queued and is also forwarded to process creation. */
  readonly signal?: AbortSignal;
}

export class SessionPoolQueueFullError extends Error {
  readonly code = "session_pool_queue_full";

  constructor(maxQueue: number) {
    super(`The Stasis session pool queue is full (maximum ${maxQueue})`);
    this.name = "SessionPoolQueueFullError";
  }
}

export class SessionPoolClosedError extends Error {
  readonly code = "session_pool_closed";

  constructor() {
    super("The Stasis session pool is closed");
    this.name = "SessionPoolClosedError";
  }
}

/**
 * Exclusive ownership of one fresh native process and its one session.
 * A lease is terminal: release closes the process; poison terminates it.
 */
export interface SessionLease<SessionType> {
  readonly session: SessionType;
  release(): Promise<void>;
  poison(): Promise<void>;
}

interface QueueEntry<Request, SessionType> {
  readonly request: Request;
  readonly signal?: AbortSignal;
  readonly resolve: (lease: SessionLease<SessionType>) => void;
  readonly reject: (error: unknown) => void;
  removeAbortListener?: () => void;
}

class ExclusiveSessionLease<SessionType> implements SessionLease<SessionType> {
  readonly session: SessionType;
  readonly #finish: (healthy: boolean) => Promise<void>;
  #finishPromise: Promise<void> | null = null;

  constructor(
    session: SessionType,
    finish: (healthy: boolean) => Promise<void>,
  ) {
    this.session = session;
    this.#finish = finish;
  }

  release(): Promise<void> {
    return this.#finishOnce(true);
  }

  poison(): Promise<void> {
    return this.#finishOnce(false);
  }

  #finishOnce(healthy: boolean): Promise<void> {
    this.#finishPromise ??= this.#finish(healthy);
    return this.#finishPromise;
  }
}

/**
 * Bounded FIFO coordination for process-per-session Stasis work.
 *
 * There is intentionally no idle process cache. A released lease performs the
 * terminal close handshake, observes process exit, and frees its slot only
 * after the process has been discarded. The next waiter then creates a fresh
 * process, so document/session tokens can never be carried to another lease by
 * the pool.
 */
export class FreshSessionPool<Request, SessionType> {
  readonly maxProcesses: number;
  readonly maxQueue: number;
  readonly #create: SessionProcessFactory<Request, SessionType>;
  readonly #queue: QueueEntry<Request, SessionType>[] = [];
  #activeProcesses = 0;
  #closed = false;
  #drainPromise: Promise<void> | null = null;
  #resolveDrain: (() => void) | null = null;

  constructor(options: FreshSessionPoolOptions<Request, SessionType>) {
    this.maxProcesses = positiveFiniteInteger(options.maxProcesses, "maxProcesses");
    this.maxQueue = nonNegativeFiniteInteger(options.maxQueue, "maxQueue");
    if (typeof options.create !== "function") {
      throw new TypeError("create must be a function");
    }
    this.#create = options.create;
  }

  get activeProcesses(): number {
    return this.#activeProcesses;
  }

  get queuedAcquisitions(): number {
    return this.#queue.length;
  }

  get closed(): boolean {
    return this.#closed;
  }

  acquire(
    request: Request,
    options: SessionAcquireOptions = {},
  ): Promise<SessionLease<SessionType>> {
    if (this.#closed) return Promise.reject(new SessionPoolClosedError());
    if (options.signal?.aborted === true) {
      return Promise.reject(new StasisAbortError(options.signal.reason));
    }

    return new Promise<SessionLease<SessionType>>((resolve, reject) => {
      const entry: QueueEntry<Request, SessionType> = {
        request,
        resolve,
        reject,
        ...(options.signal === undefined ? {} : { signal: options.signal }),
      };

      if (this.#activeProcesses < this.maxProcesses && this.#queue.length === 0) {
        this.#start(entry);
        return;
      }
      if (this.#queue.length >= this.maxQueue) {
        reject(new SessionPoolQueueFullError(this.maxQueue));
        return;
      }

      if (entry.signal !== undefined) {
        const onAbort = (): void => {
          const index = this.#queue.indexOf(entry);
          if (index === -1) return;
          this.#queue.splice(index, 1);
          entry.removeAbortListener?.();
          reject(new StasisAbortError(entry.signal?.reason));
        };
        entry.signal.addEventListener("abort", onAbort, { once: true });
        entry.removeAbortListener = () => {
          entry.signal?.removeEventListener("abort", onAbort);
          delete entry.removeAbortListener;
        };
      }
      this.#queue.push(entry);
    });
  }

  /**
   * Run one callback on one fresh session. Successful callbacks close the
   * session; thrown callbacks conservatively poison the process. Work is never
   * retried or replayed.
   */
  async run<Result>(
    request: Request,
    callback: (session: SessionType) => Result | Promise<Result>,
    options: SessionAcquireOptions = {},
  ): Promise<Result> {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    const lease = await this.acquire(request, options);
    let result: Result;
    try {
      result = await callback(lease.session);
    } catch (error) {
      try {
        await lease.poison();
      } catch (cleanupError) {
        throw new AggregateError(
          [error, cleanupError],
          "Session work failed and the poisoned process could not be terminated",
        );
      }
      throw error;
    }
    await lease.release();
    return result;
  }

  /** Stop admission, reject queued work, and resolve after all leases are discarded. */
  close(): Promise<void> {
    if (!this.#closed) {
      this.#closed = true;
      const error = new SessionPoolClosedError();
      for (const entry of this.#queue.splice(0)) {
        entry.removeAbortListener?.();
        entry.reject(error);
      }
    }
    if (this.#activeProcesses === 0) return Promise.resolve();
    this.#drainPromise ??= new Promise<void>((resolve) => {
      this.#resolveDrain = resolve;
    });
    return this.#drainPromise;
  }

  #pump(): void {
    while (
      !this.#closed &&
      this.#activeProcesses < this.maxProcesses &&
      this.#queue.length > 0
    ) {
      const entry = this.#queue.shift();
      if (entry === undefined) return;
      entry.removeAbortListener?.();
      if (entry.signal?.aborted === true) {
        entry.reject(new StasisAbortError(entry.signal.reason));
        continue;
      }
      this.#start(entry);
    }
  }

  #start(entry: QueueEntry<Request, SessionType>): void {
    entry.removeAbortListener?.();
    if (entry.signal?.aborted === true) {
      entry.reject(new StasisAbortError(entry.signal.reason));
      this.#pump();
      return;
    }
    this.#activeProcesses += 1;
    let creation: Promise<OwnedSessionProcess<SessionType>>;
    try {
      creation = Promise.resolve(
        this.#create(
          entry.request,
          entry.signal === undefined ? {} : { signal: entry.signal },
        ),
      );
    } catch (error) {
      this.#freeProcessSlot();
      entry.reject(error);
      return;
    }
    void creation.then(
      async (owned) => {
        if (entry.signal?.aborted === true) {
          try {
            await owned.terminate();
          } catch {
            // Cancellation is the public cause; the process has still been discarded.
          } finally {
            this.#freeProcessSlot();
          }
          entry.reject(new StasisAbortError(entry.signal.reason));
          return;
        }
        entry.resolve(
          new ExclusiveSessionLease(owned.session, (healthy) =>
            this.#discardOwnedProcess(owned, healthy),
          ),
        );
      },
      (error: unknown) => {
        this.#freeProcessSlot();
        entry.reject(error);
      },
    );
  }

  async #discardOwnedProcess(
    owned: OwnedSessionProcess<SessionType>,
    healthy: boolean,
  ): Promise<void> {
    try {
      if (!healthy) {
        await owned.terminate();
        return;
      }
      try {
        await owned.close();
      } catch (closeError) {
        try {
          await owned.terminate();
        } catch (terminateError) {
          throw new AggregateError(
            [closeError, terminateError],
            "The session close failed and the process could not be terminated",
          );
        }
        throw closeError;
      }
    } finally {
      this.#freeProcessSlot();
    }
  }

  #freeProcessSlot(): void {
    this.#activeProcesses -= 1;
    if (this.#activeProcesses < 0) {
      throw new Error("Stasis session pool process accounting underflow");
    }
    this.#pump();
    if (this.#closed && this.#activeProcesses === 0) {
      this.#resolveDrain?.();
      this.#resolveDrain = null;
    }
  }
}

export interface StasisSessionRequest {
  readonly url: string | URL;
  readonly options?: SessionOpenOptions;
}

export interface StasisSessionPoolOptions {
  readonly maxProcesses: number;
  readonly maxQueue: number;
  /** Defaults shared by every newly-spawned native process. */
  readonly launch?: LaunchOptions;
}

/** Create the production process-per-session pool used by the reference crawler. */
export function createStasisSessionPool(
  options: StasisSessionPoolOptions,
): FreshSessionPool<StasisSessionRequest, Session> {
  const launchOptions = options.launch ?? {};
  return new FreshSessionPool({
    maxProcesses: options.maxProcesses,
    maxQueue: options.maxQueue,
    create: async (request, context) => {
      const combined = combineAbortSignals(
        launchOptions.signal,
        request.options?.signal,
        context.signal,
      );
      const { signal } = combined;
      let runtime: Runtime | null = null;
      try {
        runtime = await launch({
          ...launchOptions,
          ...(signal === undefined ? {} : { signal }),
        });
        const session = await runtime.openSession(request.url, {
          ...request.options,
          ...(signal === undefined ? {} : { signal }),
        });
        return {
          session,
          close: () => session.close(),
          terminate: () => runtime?.close() ?? Promise.resolve(),
        };
      } catch (error) {
        if (runtime !== null) {
          try {
            await runtime.close();
          } catch (cleanupError) {
            throw new AggregateError(
              [error, cleanupError],
              "Could not create a Stasis session and its process could not be terminated",
            );
          }
        }
        throw error;
      } finally {
        combined.dispose();
      }
    },
  });
}

/** @internal Compose cancellation without relying on AbortSignal.any (Node 20.3+). */
export interface CombinedAbortSignals {
  readonly signal: AbortSignal | undefined;
  /** Remove every source listener once the operation using the combined signal has ended. */
  dispose(): void;
}

/** @internal */
export function combineAbortSignals(
  ...signals: readonly (AbortSignal | undefined)[]
): CombinedAbortSignals {
  const present = [
    ...new Set(signals.filter((signal): signal is AbortSignal => signal !== undefined)),
  ];
  if (present.length === 0) return { signal: undefined, dispose: () => undefined };
  if (present.length === 1) return { signal: present[0], dispose: () => undefined };

  const controller = new AbortController();
  const listeners: Array<readonly [AbortSignal, () => void]> = [];
  const dispose = (): void => {
    for (const [signal, listener] of listeners.splice(0)) {
      signal.removeEventListener("abort", listener);
    }
  };
  const abortFrom = (source: AbortSignal): void => {
    dispose();
    if (!controller.signal.aborted) controller.abort(source.reason);
  };

  for (const signal of present) {
    if (signal.aborted) {
      abortFrom(signal);
      break;
    }
    const listener = (): void => abortFrom(signal);
    signal.addEventListener("abort", listener, { once: true });
    listeners.push([signal, listener]);
  }
  return { signal: controller.signal, dispose };
}

function positiveFiniteInteger(value: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new RangeError(`${label} must be a finite positive safe integer`);
  }
  return value;
}

function nonNegativeFiniteInteger(value: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new RangeError(`${label} must be a finite non-negative safe integer`);
  }
  return value;
}
