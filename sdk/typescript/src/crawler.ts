import type {
  CommandOptions,
  DocumentStateToken,
  SessionExtractPlan,
  SessionNetworkOptions,
  SessionOpenOptions,
  SessionStateFor,
  SessionStateV1,
  SettleOutcome,
  SettlePolicy,
} from "./types.js";
import { StasisAbortError } from "./errors.js";
import {
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  type SelectableSessionProfile,
  type SessionSupportProfile,
} from "./profile.js";
import type {
  SessionAcquireOptions,
  StasisSessionRequest,
} from "./session-pool.js";

const LINK_EXTRACTION_PLAN = {
  rootSelector: "a[href]",
  fields: [
    {
      name: "href",
      selector: "",
      read: "resolved_url",
      attribute: "href",
    },
  ],
} as const satisfies SessionExtractPlan;

type CrawlableSettleOutcome = "quiescent" | "quiescent_with_persistent_work";

export interface ReferenceCrawlerSession {
  readonly requestedUrl: string;
  readonly url: string;
  readonly stateToken: DocumentStateToken;
  settle(
    expectedStateToken: DocumentStateToken,
    policy?: SettlePolicy,
    options?: CommandOptions,
  ): Promise<{
    readonly outcome: SettleOutcome;
    readonly stateToken: DocumentStateToken;
  }>;
  extract(
    plan: SessionExtractPlan,
    expectedStateToken: DocumentStateToken,
    options?: CommandOptions,
  ): Promise<{
    readonly rows: readonly {
      readonly fields: readonly {
        readonly name: string;
        readonly value: string | null;
      }[];
    }[];
    readonly stateToken: DocumentStateToken;
  }>;
}

/** Minimal structural pool surface, allowing native-free crawler tests and custom instrumentation. */
export interface ReferenceCrawlerPool<SessionType extends ReferenceCrawlerSession> {
  readonly maxProcesses: number;
  run<Result>(
    request: StasisSessionRequest<SelectableSessionProfile>,
    callback: (session: SessionType) => Result | Promise<Result>,
    options?: SessionAcquireOptions,
  ): Promise<Result>;
}

/**
 * Reference-crawler options keep the selected profile and imported state artifact discriminated.
 * Omitting the generic retains the frozen v1 default. The optional `profile` member is preserved
 * for v1 source compatibility; candidate-aware values must carry an explicit candidate profile at
 * the `crawlWithStasis()` call boundary, where no implicit artifact migration can type-check.
 */
export interface ReferenceCrawlerOptions<
  Profile extends SelectableSessionProfile = SessionSupportProfile,
> {
  readonly start: string | URL | readonly (string | URL)[];
  readonly maxPages: number;
  readonly maxDepth: number;
  readonly concurrency: number;
  /**
   * Exact HTTP(S) origins allowed in addition to the first start URL's origin.
   * Without this list, the crawl is strictly same-origin.
   */
  readonly allowedOrigins?: readonly (string | URL)[];
  /** Defaults to controlled-web-session-v1; candidate profiles require explicit selection. */
  readonly profile?: Profile;
  /** Imported into every fresh session before its first request. */
  readonly state?: SessionStateFor<Profile>;
  /** Use fixtures_only for a cross-run reproducible crawl. */
  readonly network?: SessionNetworkOptions;
  readonly settle?: SettlePolicy;
  readonly signal?: AbortSignal;
}

export type CrawlPageStatus =
  | "crawled"
  | "settlement_not_crawlable"
  | "redirect_disallowed";

export interface CrawlPageResult {
  /** Canonical scheduled URL: HTTP(S), normalized by URL, with no fragment. */
  readonly requestedUrl: string;
  /** Canonical final URL after the initial navigation and redirects. */
  readonly url: string;
  readonly depth: number;
  readonly status: CrawlPageStatus;
  readonly settleOutcome: SettleOutcome | null;
  /** Canonical, policy-admitted links in DOM order, before global deduplication. */
  readonly links: readonly string[];
}

export interface ReferenceCrawlResult {
  /** Deterministic breadth-first order, independent of concurrent completion order. */
  readonly pages: readonly CrawlPageResult[];
  /** Canonical URLs admitted to the bounded frontier, in admission order. */
  readonly scheduledUrls: readonly string[];
}

export class CrawlerOriginPolicyError extends Error {
  readonly code = "crawler_origin_policy";

  constructor(message: string) {
    super(message);
    this.name = "CrawlerOriginPolicyError";
  }
}

interface FrontierEntry {
  readonly url: string;
  readonly depth: number;
}

/**
 * Small, deterministic reference workload for the controlled-session runtime.
 * Each page gets one fresh process/session from the pool. It performs no sleep,
 * retry, proxy, stealth, robots, or distributed-frontier behavior.
 */
export function crawlWithStasis<SessionType extends ReferenceCrawlerSession>(
  pool: ReferenceCrawlerPool<SessionType>,
  options: ReferenceCrawlerOptions<SessionSupportProfile>,
): Promise<ReferenceCrawlResult>;
export function crawlWithStasis<
  Profile extends SelectableSessionProfile,
  SessionType extends ReferenceCrawlerSession,
>(
  pool: ReferenceCrawlerPool<SessionType>,
  options: ReferenceCrawlerOptions<Profile> & { readonly profile: Profile },
): Promise<ReferenceCrawlResult>;
export async function crawlWithStasis<SessionType extends ReferenceCrawlerSession>(
  pool: ReferenceCrawlerPool<SessionType>,
  options: ReferenceCrawlerOptions<SelectableSessionProfile>,
): Promise<ReferenceCrawlResult> {
  const maxPages = positiveFiniteInteger(options.maxPages, "maxPages");
  const maxDepth = nonNegativeFiniteInteger(options.maxDepth, "maxDepth");
  const concurrency = positiveFiniteInteger(options.concurrency, "concurrency");
  if (!Number.isSafeInteger(pool.maxProcesses) || pool.maxProcesses < 1) {
    throw new RangeError("pool.maxProcesses must be a finite positive safe integer");
  }
  if (concurrency > pool.maxProcesses) {
    throw new RangeError("concurrency cannot exceed pool.maxProcesses");
  }
  throwIfAborted(options.signal);

  const inputs = Array.isArray(options.start) ? options.start : [options.start];
  if (inputs.length === 0) throw new RangeError("start must contain at least one URL");
  const canonicalStarts = inputs.map((value) => canonicalHttpUrl(value));
  const primaryOrigin = new URL(canonicalStarts[0] as string).origin;
  const allowedOrigins = new Set<string>([primaryOrigin]);
  for (const value of options.allowedOrigins ?? []) {
    allowedOrigins.add(canonicalOrigin(value));
  }
  for (const start of canonicalStarts) {
    if (!allowedOrigins.has(new URL(start).origin)) {
      throw new CrawlerOriginPolicyError(
        `Start URL origin ${new URL(start).origin} requires an explicit allowedOrigins entry`,
      );
    }
  }

  const scheduled = new Set<string>();
  let frontier: FrontierEntry[] = [];
  for (const url of canonicalStarts) {
    if (scheduled.size >= maxPages) break;
    if (scheduled.has(url)) continue;
    scheduled.add(url);
    frontier.push({ url, depth: 0 });
  }

  const pages: CrawlPageResult[] = [];
  while (frontier.length > 0) {
    throwIfAborted(options.signal);
    const round = frontier;
    frontier = [];
    const roundResults = await mapConcurrentOrdered(round, concurrency, (entry) =>
      crawlOne(pool, entry, allowedOrigins, options),
    );
    pages.push(...roundResults);

    for (const page of roundResults) {
      if (page.depth >= maxDepth || page.status !== "crawled") continue;
      for (const url of page.links) {
        if (scheduled.size >= maxPages) break;
        if (scheduled.has(url)) continue;
        scheduled.add(url);
        frontier.push({ url, depth: page.depth + 1 });
      }
    }
  }

  return { pages, scheduledUrls: [...scheduled] };
}

async function crawlOne<SessionType extends ReferenceCrawlerSession>(
  pool: ReferenceCrawlerPool<SessionType>,
  entry: FrontierEntry,
  allowedOrigins: ReadonlySet<string>,
  options: ReferenceCrawlerOptions<SelectableSessionProfile>,
): Promise<CrawlPageResult> {
  const sharedOpenOptions: Omit<SessionOpenOptions<SessionSupportProfile>, "state"> = {
    ...(options.network === undefined ? {} : { network: options.network }),
    ...(options.signal === undefined ? {} : { signal: options.signal }),
  };
  const request: StasisSessionRequest<SelectableSessionProfile> =
    options.profile === CONTROLLED_WEB_SESSION_V2_PROFILE
      ? {
          url: entry.url,
          options: {
            ...sharedOpenOptions,
            profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
            ...(options.state === undefined
              ? {}
              : {
                  state: options.state as SessionStateFor<
                    typeof CONTROLLED_WEB_SESSION_V2_PROFILE
                  >,
                }),
          },
        }
      : {
          url: entry.url,
          options: {
            ...sharedOpenOptions,
            ...(options.state === undefined
              ? {}
              : { state: options.state as SessionStateV1 }),
            ...(options.profile === undefined ? {} : { profile: options.profile }),
          },
        };
  return pool.run(
    request,
    async (session) => {
      throwIfAborted(options.signal);
      const finalUrl = canonicalHttpUrl(session.url);
      if (!allowedOrigins.has(new URL(finalUrl).origin)) {
        return {
          requestedUrl: entry.url,
          url: finalUrl,
          depth: entry.depth,
          status: "redirect_disallowed",
          settleOutcome: null,
          links: [],
        };
      }

      // The token is deliberately local to this callback/process. The crawler
      // never stores it in the frontier or a result and cannot pass it to a
      // later process.
      const settled = await session.settle(
        session.stateToken,
        options.settle ?? {},
        options.signal === undefined ? {} : { signal: options.signal },
      );
      if (!isCrawlableOutcome(settled.outcome)) {
        return {
          requestedUrl: entry.url,
          url: finalUrl,
          depth: entry.depth,
          status: "settlement_not_crawlable",
          settleOutcome: settled.outcome,
          links: [],
        };
      }

      const extraction = await session.extract(
        LINK_EXTRACTION_PLAN,
        settled.stateToken,
        options.signal === undefined ? {} : { signal: options.signal },
      );
      const links: string[] = [];
      const localSeen = new Set<string>();
      for (const row of extraction.rows) {
        const value = row.fields.find((field) => field.name === "href")?.value;
        if (value === null || value === undefined) continue;
        let canonical: string;
        try {
          canonical = canonicalHttpUrl(value, finalUrl);
        } catch {
          continue;
        }
        if (!allowedOrigins.has(new URL(canonical).origin) || localSeen.has(canonical)) {
          continue;
        }
        localSeen.add(canonical);
        links.push(canonical);
      }
      return {
        requestedUrl: entry.url,
        url: finalUrl,
        depth: entry.depth,
        status: "crawled",
        settleOutcome: settled.outcome,
        links,
      };
    },
    options.signal === undefined ? {} : { signal: options.signal },
  );
}

/** Normalize an HTTP(S) URL for frontier deduplication and remove its fragment. */
export function canonicalHttpUrl(value: string | URL, base?: string | URL): string {
  let url: URL;
  try {
    url = base === undefined ? new URL(value) : new URL(value, base);
  } catch (error) {
    throw new TypeError("Crawler URLs must be valid absolute HTTP(S) URLs", { cause: error });
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new TypeError("Crawler URLs must use HTTP or HTTPS");
  }
  if (url.username.length > 0 || url.password.length > 0) {
    throw new TypeError("Crawler URLs must not contain credentials");
  }
  url.hash = "";
  return url.href;
}

function canonicalOrigin(value: string | URL): string {
  const url = new URL(value);
  canonicalHttpUrl(url);
  if (url.pathname !== "/" || url.search.length > 0 || url.hash.length > 0) {
    throw new TypeError("allowedOrigins entries must contain only an HTTP(S) origin");
  }
  return url.origin;
}

async function mapConcurrentOrdered<Input, Output>(
  inputs: readonly Input[],
  concurrency: number,
  callback: (input: Input) => Promise<Output>,
): Promise<Output[]> {
  const results = new Array<Output>(inputs.length);
  const failures: Array<{ readonly index: number; readonly error: unknown }> = [];
  let nextIndex = 0;
  let stopped = false;
  async function worker(): Promise<void> {
    for (;;) {
      if (stopped) return;
      const index = nextIndex;
      nextIndex += 1;
      if (index >= inputs.length) return;
      const input = inputs[index];
      if (input === undefined) return;
      try {
        results[index] = await callback(input);
      } catch (error) {
        failures.push({ index, error });
        stopped = true;
        return;
      }
    }
  }
  const workers = Array.from(
    { length: Math.min(concurrency, inputs.length) },
    () => worker(),
  );
  await Promise.all(workers);
  if (failures.length > 0) {
    failures.sort((left, right) => left.index - right.index);
    throw failures[0]!.error;
  }
  return results;
}

function isCrawlableOutcome(outcome: SettleOutcome): outcome is CrawlableSettleOutcome {
  return outcome === "quiescent" || outcome === "quiescent_with_persistent_work";
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted === true) throw new StasisAbortError(signal.reason);
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
