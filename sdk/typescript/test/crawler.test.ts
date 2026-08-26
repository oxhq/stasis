import assert from "node:assert/strict";
import test from "node:test";

import {
  CrawlerOriginPolicyError,
  canonicalHttpUrl,
  crawlWithStasis,
  type ReferenceCrawlerSession,
} from "../src/crawler.js";
import {
  FreshSessionPool,
  type StasisSessionRequest,
} from "../src/session-pool.js";
import {
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  type SelectableSessionProfile,
} from "../src/profile.js";
import type {
  DocumentStateToken,
  SessionExtractPlan,
  SessionNetworkOptions,
  SessionState,
  SettleOutcome,
} from "../src/types.js";

interface PageFixture {
  readonly finalUrl?: string;
  readonly links?: readonly (string | null)[];
  readonly outcome?: SettleOutcome;
  readonly settleGate?: Promise<void>;
  readonly settleError?: unknown;
}

interface CrawlerHarness {
  readonly pool: FreshSessionPool<
    StasisSessionRequest<SelectableSessionProfile>,
    FakeCrawlerSession
  >;
  readonly starts: string[];
  readonly closes: number[];
  readonly terminations: number[];
  readonly tokenUses: { processId: number; operation: string; token: string }[];
  readonly requests: StasisSessionRequest<SelectableSessionProfile>[];
  readonly maximumActive: () => number;
}

class FakeCrawlerSession implements ReferenceCrawlerSession {
  readonly requestedUrl: string;
  readonly url: string;
  readonly stateToken: DocumentStateToken;
  readonly #processId: number;
  readonly #fixture: PageFixture;
  readonly #tokenUses: CrawlerHarness["tokenUses"];

  constructor(
    processId: number,
    requestedUrl: string,
    fixture: PageFixture,
    tokenUses: CrawlerHarness["tokenUses"],
  ) {
    this.#processId = processId;
    this.requestedUrl = requestedUrl;
    this.url = fixture.finalUrl ?? requestedUrl;
    this.stateToken = token(`process-${processId}-open`);
    this.#fixture = fixture;
    this.#tokenUses = tokenUses;
  }

  async settle(expectedStateToken: DocumentStateToken): Promise<{
    outcome: SettleOutcome;
    stateToken: DocumentStateToken;
  }> {
    this.#tokenUses.push({
      processId: this.#processId,
      operation: "settle",
      token: expectedStateToken,
    });
    assert.equal(expectedStateToken, this.stateToken);
    await this.#fixture.settleGate;
    if (this.#fixture.settleError !== undefined) {
      throw this.#fixture.settleError;
    }
    return {
      outcome: this.#fixture.outcome ?? "quiescent",
      stateToken: token(`process-${this.#processId}-settled`),
    };
  }

  async extract(
    plan: SessionExtractPlan,
    expectedStateToken: DocumentStateToken,
  ): Promise<{
    rows: { fields: { name: string; value: string | null }[] }[];
    stateToken: DocumentStateToken;
  }> {
    this.#tokenUses.push({
      processId: this.#processId,
      operation: "extract",
      token: expectedStateToken,
    });
    assert.equal(expectedStateToken, token(`process-${this.#processId}-settled`));
    assert.deepEqual(plan, {
      rootSelector: "a[href]",
      fields: [
        { name: "href", selector: "", read: "resolved_url", attribute: "href" },
      ],
    });
    return {
      rows: (this.#fixture.links ?? []).map((value) => ({
        fields: [{ name: "href", value }],
      })),
      stateToken: expectedStateToken,
    };
  }
}

function crawlerHarness(
  fixtures: ReadonlyMap<string, PageFixture>,
  maxProcesses = 2,
): CrawlerHarness {
  let processId = 0;
  let active = 0;
  let maximumActive = 0;
  const starts: string[] = [];
  const closes: number[] = [];
  const terminations: number[] = [];
  const tokenUses: CrawlerHarness["tokenUses"] = [];
  const requests: StasisSessionRequest<SelectableSessionProfile>[] = [];
  const pool = new FreshSessionPool<
    StasisSessionRequest<SelectableSessionProfile>,
    FakeCrawlerSession
  >({
    maxProcesses,
    maxQueue: 32,
    create: async (request) => {
      const requestedUrl = canonicalHttpUrl(request.url);
      const fixture = fixtures.get(requestedUrl) ?? {};
      const id = ++processId;
      active += 1;
      maximumActive = Math.max(maximumActive, active);
      starts.push(requestedUrl);
      requests.push(request);
      return {
        session: new FakeCrawlerSession(id, requestedUrl, fixture, tokenUses),
        close: async () => {
          active -= 1;
          closes.push(id);
        },
        terminate: async () => {
          active -= 1;
          terminations.push(id);
        },
      };
    },
  });
  return {
    pool,
    starts,
    closes,
    terminations,
    tokenUses,
    requests,
    maximumActive: () => maximumActive,
  };
}

test("crawler preserves deterministic breadth-first order under concurrent completion", async () => {
  const slow = deferred<void>();
  const fast = deferred<void>();
  const fixtures = new Map<string, PageFixture>([
    ["https://example.test/slow", { settleGate: slow.promise }],
    ["https://example.test/fast", { settleGate: fast.promise }],
  ]);
  const harness = crawlerHarness(fixtures, 2);
  const crawlPromise = crawlWithStasis(harness.pool, {
    start: ["https://example.test/slow", "https://example.test/fast"],
    maxPages: 2,
    maxDepth: 0,
    concurrency: 2,
  });

  await waitFor(() => harness.starts.length === 2);
  fast.resolve();
  await waitFor(() => harness.closes.length === 1);
  slow.resolve();
  const result = await crawlPromise;

  assert.deepEqual(
    result.pages.map((page) => page.requestedUrl),
    ["https://example.test/slow", "https://example.test/fast"],
  );
  assert.equal(harness.maximumActive(), 2);
  assert.deepEqual(harness.terminations, []);
  assert.deepEqual([...harness.closes].sort((left, right) => left - right), [1, 2]);
  assert.deepEqual(harness.tokenUses, [
    { processId: 1, operation: "settle", token: "process-1-open" },
    { processId: 2, operation: "settle", token: "process-2-open" },
    { processId: 2, operation: "extract", token: "process-2-settled" },
    { processId: 1, operation: "extract", token: "process-1-settled" },
  ]);
  await harness.pool.close();
});

test("crawler drains in-flight pages and starts no new page after a failure", async () => {
  const slow = deferred<void>();
  const expected = new Error("settlement failed");
  const fixtures = new Map<string, PageFixture>([
    ["https://example.test/failing", { settleError: expected }],
    ["https://example.test/slow", { settleGate: slow.promise }],
    ["https://example.test/not-started", {}],
  ]);
  const harness = crawlerHarness(fixtures, 2);
  const crawlPromise = crawlWithStasis(harness.pool, {
    start: [
      "https://example.test/failing",
      "https://example.test/slow",
      "https://example.test/not-started",
    ],
    maxPages: 3,
    maxDepth: 0,
    concurrency: 2,
  });
  let finished = false;
  let rejection: unknown;
  const observed = crawlPromise.then(
    () => {
      finished = true;
    },
    (error: unknown) => {
      finished = true;
      rejection = error;
    },
  );

  await waitFor(() => harness.starts.length === 2);
  await waitFor(() => harness.terminations.length === 1);
  assert.equal(finished, false);
  assert.deepEqual(harness.starts, [
    "https://example.test/failing",
    "https://example.test/slow",
  ]);

  slow.resolve();
  await observed;
  assert.equal(rejection, expected);
  assert.deepEqual(harness.starts, [
    "https://example.test/failing",
    "https://example.test/slow",
  ]);
  assert.deepEqual(harness.terminations, [1]);
  assert.deepEqual(harness.closes, [2]);
  await harness.pool.close();
});

test("crawler canonicalizes links, bounds the frontier, and rechecks redirect origins", async () => {
  const fixtures = new Map<string, PageFixture>([
    [
      "https://example.test/root",
      {
        links: [
          "https://example.test/a#one",
          "https://example.test/a#two",
          "https://other.test/outside",
          "mailto:hello@example.test",
          "https://example.test/b",
          null,
        ],
      },
    ],
    [
      "https://example.test/a",
      { finalUrl: "https://other.test/redirected", links: ["https://example.test/leak"] },
    ],
    ["https://example.test/b", { links: ["https://example.test/too-deep"] }],
  ]);
  const harness = crawlerHarness(fixtures, 2);
  const result = await crawlWithStasis(harness.pool, {
    start: "https://example.test/root#ignored",
    maxPages: 3,
    maxDepth: 1,
    concurrency: 2,
  });

  assert.deepEqual(result.scheduledUrls, [
    "https://example.test/root",
    "https://example.test/a",
    "https://example.test/b",
  ]);
  assert.deepEqual(
    result.pages.map(({ requestedUrl, url, depth, status }) => ({
      requestedUrl,
      url,
      depth,
      status,
    })),
    [
      {
        requestedUrl: "https://example.test/root",
        url: "https://example.test/root",
        depth: 0,
        status: "crawled",
      },
      {
        requestedUrl: "https://example.test/a",
        url: "https://other.test/redirected",
        depth: 1,
        status: "redirect_disallowed",
      },
      {
        requestedUrl: "https://example.test/b",
        url: "https://example.test/b",
        depth: 1,
        status: "crawled",
      },
    ],
  );
  assert.equal(
    harness.tokenUses.some(
      (use) => use.processId === 2 && use.operation === "settle",
    ),
    false,
  );
  assert.equal(harness.starts.includes("https://example.test/leak"), false);
  assert.equal(harness.starts.includes("https://example.test/too-deep"), false);
  await harness.pool.close();
});

test("cross-origin crawling requires an explicit HTTP(S) origin allowlist", async () => {
  const fixtures = new Map<string, PageFixture>([
    [
      "https://example.test/root",
      { links: ["https://other.test/allowed#fragment"] },
    ],
    ["https://other.test/allowed", {}],
  ]);
  const harness = crawlerHarness(fixtures, 1);
  const result = await crawlWithStasis(harness.pool, {
    start: "https://example.test/root",
    allowedOrigins: ["https://other.test"],
    maxPages: 2,
    maxDepth: 1,
    concurrency: 1,
  });
  assert.deepEqual(result.scheduledUrls, [
    "https://example.test/root",
    "https://other.test/allowed",
  ]);
  await harness.pool.close();

  const secondHarness = crawlerHarness(new Map(), 1);
  await assert.rejects(
    crawlWithStasis(secondHarness.pool, {
      start: ["https://example.test/", "https://other.test/"],
      maxPages: 2,
      maxDepth: 0,
      concurrency: 1,
    }),
    (error) => error instanceof CrawlerOriginPolicyError,
  );
  await secondHarness.pool.close();
});

test("crawler forwards immutable fixtures/state and validates all finite bounds", async () => {
  const harness = crawlerHarness(new Map([["https://example.test/", {}]]), 2);
  const network = {
    mode: "fixtures_only",
    routes: [],
  } as const satisfies SessionNetworkOptions;
  const state = {
    schemaVersion: 1,
    profile: "controlled-web-session-v1",
    sensitive: true,
    sessionStorageScope: "top_level_browsing_context",
    cookies: [],
    origins: [],
  } as const satisfies SessionState;
  await crawlWithStasis(harness.pool, {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    network,
    state,
  });
  assert.equal(harness.requests[0]?.options?.network, network);
  assert.equal(harness.requests[0]?.options?.state, state);
  assert.equal(
    harness.requests[0]?.options?.profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
  );
  await harness.pool.close();

  const defaultHarness = crawlerHarness(new Map([["https://example.test/", {}]]), 1);
  await crawlWithStasis(defaultHarness.pool, {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
  });
  assert.equal(
    Object.hasOwn(defaultHarness.requests[0]?.options ?? {}, "profile"),
    false,
    "crawler omission must preserve the stable openSession default instead of synthesizing v2",
  );
  await defaultHarness.pool.close();

  const invalidHarness = crawlerHarness(new Map(), 2);
  for (const [name, options] of [
    ["maxPages", { maxPages: 0, maxDepth: 0, concurrency: 1 }],
    ["maxDepth", { maxPages: 1, maxDepth: -1, concurrency: 1 }],
    ["concurrency", { maxPages: 1, maxDepth: 0, concurrency: 3 }],
  ] as const) {
    await assert.rejects(
      crawlWithStasis(invalidHarness.pool, {
        start: "https://example.test/",
        ...options,
      }),
      new RegExp(name, "u"),
    );
  }
  assert.throws(() => canonicalHttpUrl("ftp://example.test/file"), /HTTP or HTTPS/u);
  assert.throws(
    () => canonicalHttpUrl("https://user:secret@example.test/"),
    /credentials/u,
  );
  await invalidHarness.pool.close();
});

function token(value: string): DocumentStateToken {
  return value as DocumentStateToken;
}

function deferred<Value>(): {
  readonly promise: Promise<Value>;
  readonly resolve: (value: Value) => void;
} {
  let resolve!: (value: Value) => void;
  const promise = new Promise<Value>((innerResolve) => {
    resolve = innerResolve;
  });
  return { promise, resolve };
}

async function waitFor(predicate: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (predicate()) return;
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
  throw new Error("condition was not reached");
}
