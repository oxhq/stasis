import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  StasisAbortError,
  StasisProtocolError,
  StasisStateError,
  StasisTransportError,
  launch,
  type AnySupportProfile,
  type DocumentStateToken,
  type NetworkRoute,
  type OpenOptions,
  type ProtocolErrorDetails,
  type ProtocolErrorDetailValue,
  type Runtime,
  type Session,
  type SessionCookie,
  type SessionEvidenceFailureReason,
  type SessionOriginState,
  type SessionState,
  type SessionStateToken,
  type SupportProfile,
} from "../src/index.js";
import {
  encodeSessionCookiesSetParams,
  encodeSessionDocumentTargetParams,
  encodeSessionState,
  encodeSessionStateTokenParams,
  encodeSessionStorageSetParams,
} from "../src/wire.js";

if (false) {
  const legacyOptions = <Profile extends SupportProfile>(profile: Profile): OpenOptions => ({
    profile,
  });
  void legacyOptions(CONTROLLED_WEBAPP_V1_PROFILE);

  const sessionProfile: AnySupportProfile = CONTROLLED_WEB_SESSION_V1_PROFILE;
  void sessionProfile;

  // @ts-expect-error SupportProfile is the exact legacy alias preserved from Stasis 0.1.
  const incompatibleLegacyProfile: SupportProfile = CONTROLLED_WEB_SESSION_V1_PROFILE;
  void incompatibleLegacyProfile;
}

const fixture = fileURLToPath(new URL("./fixtures/fake-shell.mjs", import.meta.url));
const stringifyBigInts = (value: unknown): string =>
  JSON.stringify(value, (_key, item) =>
    typeof item === "bigint" ? item.toString() : item,
  );

const initialState = {
  schemaVersion: 1,
  profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
  sensitive: true,
  sessionStorageScope: "top_level_browsing_context",
  cookies: [
    {
      name: "session",
      value: "sensitive-cookie-value",
      domain: "example.test",
      path: "/",
      hostOnly: true,
      secure: true,
      httpOnly: true,
      sameSite: "lax",
      expiresUnixTimeNs: null,
      partitioned: false,
      creationSequence: 1n,
      lastAccessSequence: 2n,
    },
  ],
  origins: [
    {
      origin: "https://example.test",
      localStorage: [{ key: "theme", value: "dark" }],
      sessionStorage: [{ key: "csrf", value: "sensitive-storage-value" }],
    },
  ],
} as const satisfies SessionState;

const canonicalSessionToken =
  "session:22222222222222222222222222222222:1" as SessionStateToken;

function cookieWith(overrides: Partial<SessionCookie> = {}): SessionCookie {
  return { ...initialState.cookies[0], ...overrides };
}

function originWith(overrides: Partial<SessionOriginState> = {}): SessionOriginState {
  return { ...initialState.origins[0], ...overrides };
}

function assertSecretSafeInputError(
  operation: () => unknown,
  expected: RegExp,
  secret: string,
): void {
  assert.throws(operation, (error) => {
    assert.ok(error instanceof TypeError || error instanceof RangeError);
    assert.match(error.message, expected);
    assert.equal(error.message.includes(secret), false);
    return true;
  });
}

test("session-state preflight rejects oversized and adversarial collections before iteration", () => {
  const oversizedCookies = new Array(513) as SessionCookie[];
  let cookieElementRead = false;
  Object.defineProperty(oversizedCookies, 0, {
    get() {
      cookieElementRead = true;
      throw new Error("cookie element must not be read");
    },
  });
  assert.throws(
    () => encodeSessionCookiesSetParams(oversizedCookies, canonicalSessionToken),
    /cookies must contain at most 512 items/u,
  );
  assert.equal(cookieElementRead, false);

  const oversizedOrigins = new Array(65) as SessionOriginState[];
  let originElementRead = false;
  Object.defineProperty(oversizedOrigins, 0, {
    get() {
      originElementRead = true;
      throw new Error("origin element must not be read");
    },
  });
  assert.throws(
    () => encodeSessionStorageSetParams(oversizedOrigins, canonicalSessionToken),
    /origins must contain at most 64 items/u,
  );
  assert.equal(originElementRead, false);

  const oversizedEntries = new Array(1025);
  let entryRead = false;
  Object.defineProperty(oversizedEntries, 0, {
    get() {
      entryRead = true;
      throw new Error("storage entry must not be read");
    },
  });
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [originWith({ localStorage: oversizedEntries })],
        canonicalSessionToken,
      ),
    /localStorage must contain at most 1024 items/u,
  );
  assert.equal(entryRead, false);

  let iterableRead = false;
  const adversarialIterable = {
    *[Symbol.iterator]() {
      iterableRead = true;
      throw new Error("iterable must not be consumed");
    },
  } as unknown as readonly SessionCookie[];
  assert.throws(
    () => encodeSessionCookiesSetParams(adversarialIterable, canonicalSessionToken),
    /cookies must be an array/u,
  );
  assert.equal(iterableRead, false);

  const unreadCookies = new Array(1) as SessionCookie[];
  Object.defineProperty(unreadCookies, 0, {
    get() {
      throw new Error("invalid token must short-circuit cookie traversal");
    },
  });
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        unreadCookies,
        "x".repeat(257) as SessionStateToken,
      ),
    /non-empty opaque token of at most 256 UTF-8 bytes/u,
  );
});

test("session cookie preflight mirrors syntax, identity, sequence, and byte limits", () => {
  const secret = "cookie-secret-canary";
  assertSecretSafeInputError(
    () =>
      encodeSessionCookiesSetParams(
        [{ ...cookieWith(), [secret]: secret } as SessionCookie],
        canonicalSessionToken,
      ),
    /contains an unexpected field/u,
    secret,
  );
  assertSecretSafeInputError(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith({ name: `${secret};invalid` })],
        canonicalSessionToken,
      ),
    /RFC 6265 cookie wire shape/u,
    secret,
  );
  assertSecretSafeInputError(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith({ value: `${secret};invalid` })],
        canonicalSessionToken,
      ),
    /RFC 6265 cookie wire shape/u,
    secret,
  );
  for (const domain of [
    "Example.test",
    ".example.test",
    "example.test.",
    "user@example.test",
    "example.test:443",
    "[2001:db8::1]",
  ]) {
    assert.throws(
      () =>
        encodeSessionCookiesSetParams(
          [cookieWith({ domain })],
          canonicalSessionToken,
        ),
      /non-empty canonical host/u,
      `accepted non-canonical cookie domain ${domain}`,
    );
  }
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith({ path: "not-absolute" })],
        canonicalSessionToken,
      ),
    /path must start with/u,
  );
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith({ sameSite: "none", secure: false })],
        canonicalSessionToken,
      ),
    /SameSite=None cookies must be secure/u,
  );
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith(), cookieWith({ creationSequence: 3n, lastAccessSequence: 4n })],
        canonicalSessionToken,
      ),
    /duplicate cookie identities/u,
  );
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith(), cookieWith({ name: "other", lastAccessSequence: 4n })],
        canonicalSessionToken,
      ),
    /duplicate creation sequences/u,
  );
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith(), cookieWith({ name: "other", creationSequence: 3n })],
        canonicalSessionToken,
      ),
    /duplicate last-access sequences/u,
  );
  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith({ path: `/${"é".repeat(2048)}` })],
        canonicalSessionToken,
      ),
    /4096 UTF-8 bytes/u,
  );

  const fragmentOverflow = Array.from({ length: 70 }, (_unused, index) =>
    cookieWith({
      name: `cookie-${index}`,
      value: "x",
      path: `/${"p".repeat(3800)}`,
      creationSequence: BigInt(index + 1),
      lastAccessSequence: BigInt(index + 1000),
    }),
  );
  assert.throws(
    () => encodeSessionCookiesSetParams(fragmentOverflow, canonicalSessionToken),
    /cookies must encode to at most 256000 UTF-8 bytes/u,
  );
});

test("the public cookie fragment accepts exactly 256000 compact bytes and rejects one more", () => {
  let cookies = Array.from({ length: 64 }, (_unused, index) =>
    cookieWith({
      name: `boundary-${index}`,
      value: "x",
      path: "/",
      creationSequence: BigInt(index + 1),
      lastAccessSequence: BigInt(index + 1000),
    }),
  );
  const compactBytes = (value: readonly SessionCookie[]): number => {
    const encoded = encodeSessionCookiesSetParams(value, canonicalSessionToken);
    return Buffer.byteLength(JSON.stringify(encoded.cookies), "utf8");
  };
  let remaining = 256_000 - compactBytes(cookies);
  assert.ok(remaining > 0);
  cookies = cookies.map((cookie) => {
    const fixedBytes = Buffer.byteLength(
      `${cookie.name}${cookie.value}${cookie.domain}`,
      "utf8",
    );
    const capacity = 4096 - fixedBytes - 1;
    const added = Math.min(capacity, remaining);
    remaining -= added;
    return { ...cookie, path: `/${"p".repeat(added)}` };
  });
  assert.equal(remaining, 0);
  assert.equal(compactBytes(cookies), 256_000);
  assert.doesNotThrow(() =>
    encodeSessionState({
      ...initialState,
      cookies,
    }),
  );

  const last = cookies.at(-1)!;
  const oneByteOver = [
    ...cookies.slice(0, -1),
    { ...last, path: `${last.path}p` },
  ];
  assert.throws(
    () => encodeSessionCookiesSetParams(oneByteOver, canonicalSessionToken),
    /cookies must encode to at most 256000 UTF-8 bytes/u,
  );
});

test("session storage preflight mirrors canonical, uniqueness, and byte limits", () => {
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [originWith({ origin: "HTTPS://example.test" })],
        canonicalSessionToken,
      ),
    /canonical HTTP\(S\) origin/u,
  );
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [originWith(), originWith()],
        canonicalSessionToken,
      ),
    /duplicate origins/u,
  );
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [
          originWith({
            localStorage: [
              { key: "duplicate", value: "one" },
              { key: "duplicate", value: "two" },
            ],
          }),
        ],
        canonicalSessionToken,
      ),
    /duplicate keys/u,
  );
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [originWith({ localStorage: [{ key: "key", value: "é".repeat(65_537) }] })],
        canonicalSessionToken,
      ),
    /131072 UTF-8 bytes/u,
  );
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [
          originWith({
            localStorage: [
              { key: "a", value: "x".repeat(131_072) },
              { key: "b", value: "x".repeat(131_072) },
            ],
            sessionStorage: [
              { key: "c", value: "x".repeat(131_072) },
              { key: "d", value: "x".repeat(131_072) },
            ],
          }),
        ],
        canonicalSessionToken,
      ),
    /524288 UTF-8 bytes/u,
  );
  assert.throws(
    () =>
      encodeSessionStorageSetParams(
        [
          originWith({
            localStorage: [
              { key: "a", value: "x".repeat(128_000) },
              { key: "b", value: "x".repeat(128_000) },
            ],
            sessionStorage: [],
          }),
        ],
        canonicalSessionToken,
      ),
    /origins must encode to at most 256000 UTF-8 bytes/u,
  );

  const encoded = encodeSessionState(initialState);
  assert.ok(Buffer.byteLength(JSON.stringify(encoded), "utf8") <= 524_288);
});

test("the public origins fragment accepts exactly 256000 compact bytes and rejects one more", () => {
  const compactBytes = (value: readonly SessionOriginState[]): number => {
    const encoded = encodeSessionStorageSetParams(value, canonicalSessionToken);
    return Buffer.byteLength(JSON.stringify(encoded.origins), "utf8");
  };
  let origins = [
    originWith({
      localStorage: [
        { key: "first", value: "" },
        { key: "second", value: "" },
      ],
      sessionStorage: [],
    }),
  ];
  let remaining = 256_000 - compactBytes(origins);
  assert.ok(remaining > 131_072 && remaining <= 262_144);
  const firstBytes = Math.min(131_072, remaining);
  remaining -= firstBytes;
  const secondBytes = remaining;
  remaining -= secondBytes;
  origins = [
    originWith({
      localStorage: [
        { key: "first", value: "x".repeat(firstBytes) },
        { key: "second", value: "x".repeat(secondBytes) },
      ],
      sessionStorage: [],
    }),
  ];
  assert.equal(remaining, 0);
  assert.equal(compactBytes(origins), 256_000);
  assert.doesNotThrow(() => encodeSessionState({ ...initialState, origins }));

  const oneByteOver = [
    originWith({
      ...origins[0],
      localStorage: [
        origins[0]!.localStorage[0]!,
        {
          ...origins[0]!.localStorage[1]!,
          value: `${origins[0]!.localStorage[1]!.value}x`,
        },
      ],
    }),
  ];
  assert.throws(
    () => encodeSessionStorageSetParams(oneByteOver, canonicalSessionToken),
    /origins must encode to at most 256000 UTF-8 bytes/u,
  );
});

async function openSessionFake(
  context: { after(callback: () => void | Promise<void>): void },
  scenario = "session-v02",
): Promise<{ runtime: Runtime; session: Session }> {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, scenario],
  });
  context.after(() => runtime.close());
  const session = await runtime.openSession("https://example.test/start", {
    clock: { mode: "controlled", initialVirtualTimeNs: 7n, unixTimeOriginNs: 0n },
    state: initialState,
    network: {
      mode: "fixtures_only",
      routes: [
        {
          match: {
            method: "GET",
            url: { exact: "https://example.test/start?token=must-not-leak" },
          },
          fulfill: {
            status: 200,
            headers: [
              ["content-type", "text/html"],
              ["set-cookie", "secret=must-not-leak"],
            ],
            body: { utf8: "<p>fixture</p>" },
          },
        },
        {
          match: { method: "POST", url: { prefix: "https://example.test/private" } },
          abort: { reason: "blocked_by_fixture" },
        },
      ],
    },
  });
  return { runtime, session };
}

test("openSession exposes token authority and the complete bounded session surface", async (context) => {
  const { runtime, session } = await openSessionFake(context);
  assert.ok(runtime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V1_PROFILE));
  assert.equal(session.profile, CONTROLLED_WEB_SESSION_V1_PROFILE);
  assert.equal(session.clockMode, "controlled");
  assert.equal(session.boundary, "controlled_ready");

  const pending = await session.pending();
  assert.equal(pending.stateToken, session.stateToken);
  assert.equal(pending.stateGeneration, 9007199254740993n);

  const query = await session.query(".result", pending.stateToken);
  assert.equal(query.count, 2n);
  assert.equal(query.stateToken, pending.stateToken);
  const text = await session.text("#status", query.stateToken);
  assert.equal(text.value, "ready");
  assert.equal(text.stateToken, query.stateToken);

  const fill = await session.fill("#email", "sensitive-fill", text.stateToken);
  assert.notEqual(fill.stateToken, text.stateToken);
  const focused = await session.focus("#email", fill.stateToken);
  assert.equal(focused.focused, true);
  const checked = await session.check("#terms", focused.stateToken);
  assert.deepEqual(
    { changed: checked.changed, checked: checked.checked },
    { changed: true, checked: true },
  );
  const unchecked = await session.uncheck("#terms", checked.stateToken);
  assert.equal(unchecked.checked, false);
  await assert.rejects(
    session.select("#region", ["north", "north"], unchecked.stateToken),
    /must not contain duplicates/u,
  );
  const selected = await session.select("#region", ["north", "west"], unchecked.stateToken);
  assert.deepEqual(selected.values, ["north", "west"]);
  const submitted = await session.submit("#login", selected.stateToken);
  assert.equal(submitted.submitted, true);
  const extraction = await session.extract(
    {
      rootSelector: "a.next",
      fields: [
        { name: "link", selector: "", read: "resolved_url", attribute: "href" },
        { name: "missing", selector: "", read: "attribute", attribute: "data-missing" },
      ],
    },
    submitted.stateToken,
  );
  assert.deepEqual(extraction.rows, [
    {
      fields: [
        { name: "link", value: "https://example.test/next" },
        { name: "missing", value: null },
      ],
    },
  ]);
  assert.equal(extraction.stateToken, submitted.stateToken);

  const settled = await session.settle(extraction.stateToken, {
    persistentWork: "report",
    maxControlTurns: 100_000n,
  });
  assert.equal(settled.outcome, "quiescent");
  assert.equal(settled.snapshot.stateToken, settled.stateToken);
  assert.notEqual(settled.stateToken, extraction.stateToken);

  const navigated = await session.navigate(
    "https://example.test/next",
    settled.stateToken,
  );
  assert.equal(navigated.url, "https://example.test/next");
  assert.equal(navigated.documentEpoch, 3n);
  assert.equal(navigated.navigationId, 2n);
  assert.equal(navigated.historyRevision, 4n);
  assert.notEqual(navigated.stateToken, settled.stateToken);
  const advanced = await session.advanceToNext(navigated.stateToken);
  assert.equal(advanced.snapshot.stateToken, advanced.stateToken);

  const cookies = await session.getCookies();
  assert.equal(cookies.cookies[0]?.creationSequence, 1n);
  assert.equal(cookies.sessionStateToken, session.sessionStateToken);
  const cookieMutation = await session.setCookies(cookies.cookies, cookies.sessionStateToken);
  assert.notEqual(cookieMutation.sessionStateToken, cookies.sessionStateToken);

  const storage = await session.getStorage();
  assert.equal(storage.sessionStateToken, cookieMutation.sessionStateToken);
  assert.equal(storage.origins[0]?.localStorage[0]?.value, "dark");
  const storageMutation = await session.setStorage(
    storage.origins,
    storage.sessionStateToken,
  );
  const exported = await session.exportState();
  assert.equal(exported.sessionStateToken, storageMutation.sessionStateToken);
  assert.equal(exported.state.profile, CONTROLLED_WEB_SESSION_V1_PROFILE);
  assert.equal(exported.state.cookies[0]?.lastAccessSequence, 2n);
  const postPublicationImport: Promise<never> = session.importState(
    exported.state,
    exported.sessionStateToken,
  );
  await assert.rejects(postPublicationImport, (error) => {
    assert.ok(error instanceof StasisProtocolError);
    assert.equal(error.code, "session_state_import_phase_closed");
    assert.equal(
      error.message,
      "Session state import is closed after session publication; pass state to session.open instead",
    );
    assert.equal(error.fatal, false);
    assert.equal(error.stateEffect, "none");
    assert.equal(error.sessionId, "fake-session");
    assert.match(error.requestId ?? "", /^\d+$/u);
    const details: ProtocolErrorDetails | undefined = error.details;
    const detailValue: ProtocolErrorDetailValue = details ?? null;
    assert.equal(detailValue, null);
    return true;
  });
  const stateAfterRejectedImport = await session.exportState();
  assert.equal(stateAfterRejectedImport.sessionStateToken, exported.sessionStateToken);
  assert.deepEqual(stateAfterRejectedImport.state, exported.state);

  const requests = await session.requests({ afterSeq: 0n, limit: 16 });
  assert.equal(requests.records[0]?.seq, 1n);
  assert.deepEqual(requests.records[0]?.url, {
    origin: "https://example.test",
    path: "/start",
    queryKeys: ["token"],
  });
  assert.equal(requests.records[0]?.bodyBytes, 0n);
  assert.equal(requests.stateToken, advanced.stateToken);
  const requestJson = stringifyBigInts(requests);
  assert.equal(requestJson.includes("must-not-leak"), false);
  assert.equal(requestJson.includes("sensitive-cookie-value"), false);

  const evidence = await session.evidence({ afterSeq: 0n, limit: 16 });
  assert.equal(evidence.schemaVersion, 2);
  assert.equal(evidence.records[0]?.kind, "request_started");
  assert.equal(evidence.records[1]?.kind, "route_decided");
  const navigationStarted = evidence.records[2];
  assert.equal(navigationStarted?.kind, "navigation_started");
  if (navigationStarted?.kind === "navigation_started") {
    assert.equal(navigationStarted.navigationId, 2n);
  }
  assert.equal(evidence.stateToken, advanced.stateToken);
  assert.equal(stringifyBigInts(evidence).includes("must-not-leak"), false);

  if (false) {
    // @ts-expect-error Document authority cannot authorize session-state mutation.
    await session.setCookies([], advanced.stateToken);
    // @ts-expect-error Session-state authority cannot authorize a document operation.
    await session.query("body", exported.sessionStateToken);
    const validFailureReason: SessionEvidenceFailureReason = "fixture_miss";
    void validFailureReason;
    // @ts-expect-error Schema-2 evidence failure reasons are a frozen vocabulary.
    const invalidFailureReason: SessionEvidenceFailureReason = "invented_failure_reason";
    void invalidFailureReason;
  }

  await session.close();
  await assert.rejects(
    runtime.open("https://example.test/", {
      profile: CONTROLLED_WEBAPP_V1_PROFILE,
    }),
    StasisStateError,
  );
});

test("canonical unbracketed IPv6 cookie domains pass outbound and inbound state validation", async (context) => {
  const { session } = await openSessionFake(context);
  const before = await session.getCookies();
  const ipv6Cookie = cookieWith({ domain: "2001:db8::1" });
  await session.setCookies([ipv6Cookie], before.sessionStateToken);
  const after = await session.getCookies();
  assert.equal(after.cookies[0]?.domain, "2001:db8::1");

  assert.throws(
    () =>
      encodeSessionCookiesSetParams(
        [cookieWith({ domain: "[2001:db8::1]" })],
        canonicalSessionToken,
      ),
    /non-empty canonical host/u,
  );
});

test("openSession is explicit and leaves the frozen legacy default available", async (context) => {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, "normal"],
  });
  context.after(() => runtime.close());
  await assert.rejects(
    runtime.openSession("https://example.test/"),
    (error) => {
      assert.ok(error instanceof StasisStateError);
      assert.match(error.message, /controlled-web-session-v1/u);
      return true;
    },
  );
  const app = await runtime.open("https://example.test/");
  assert.equal(app.profile, CONTROLLED_WEBAPP_V1_PROFILE);
  await app.close();
});

test("session result decoding rejects malformed-Unicode opaque document tokens", async (context) => {
  const { session } = await openSessionFake(context, "session-invalid-token");
  await assert.rejects(session.pending(), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.match(error.message, /stateToken/u);
    return true;
  });
});

test("session open decoding rejects overlong opaque session-state tokens", async (context) => {
  await assert.rejects(
    openSessionFake(context, "session-invalid-session-state-token"),
    (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.match(error.message, /sessionStateToken/u);
      return true;
    },
  );
});

for (const [scenario, operation, expectedMessage] of [
  [
    "session-invalid-cookie-state",
    (session: Session) => session.getCookies(),
    /duplicate cookie identities/u,
  ],
  [
    "session-secret-cookie-field",
    (session: Session) => session.getCookies(),
    /unexpected field/u,
  ],
  [
    "session-invalid-storage-state",
    (session: Session) => session.getStorage(),
    /duplicate keys/u,
  ],
  [
    "session-oversized-cookie-state",
    (session: Session) => session.getCookies(),
    /at most 256000 UTF-8 bytes/u,
  ],
  [
    "session-oversized-storage-state",
    (session: Session) => session.getStorage(),
    /at most 256000 UTF-8 bytes/u,
  ],
  [
    "session-invalid-export-state",
    (session: Session) => session.exportState(),
    /canonical HTTP\(S\) origin/u,
  ],
] as const) {
  test(`malformed inbound state from ${scenario} fail-stops the transport`, async (context) => {
    const { session } = await openSessionFake(context, scenario);
    let terminal: StasisTransportError | undefined;
    await assert.rejects(operation(session), (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "invalid_result");
      assert.match(error.message, expectedMessage);
      assert.equal(error.message.includes("sensitive"), false);
      terminal = error;
      return true;
    });
    await assert.rejects(session.pending(), (error) => error === terminal);
  });
}

test("an impossible successful post-publication import response fail-stops", async (context) => {
  const { session } = await openSessionFake(context, "session-import-unexpected-success");
  let terminal: StasisTransportError | undefined;
  await assert.rejects(
    session.importState(initialState, session.sessionStateToken),
    (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "invalid_result");
      assert.match(error.message, /unexpectedly succeeded after session publication/u);
      terminal = error;
      return true;
    },
  );
  await assert.rejects(session.pending(), (error) => error === terminal);
});

test("future opaque token forms are accepted inbound and outbound without parsing", async (context) => {
  const { session } = await openSessionFake(context, "session-future-opaque-tokens");
  assert.equal(session.stateToken, "future-document-authority/v9");
  assert.equal(session.sessionStateToken, "future-session-authority/v9");
  const pending = await session.pending();
  assert.equal(pending.stateToken, session.stateToken);
  await session.query("body", session.stateToken);
  await session.setCookies([], session.sessionStateToken);
});

test("opaque token encoders enforce only nonempty well-formed 256-byte bounds", () => {
  const futureDocument = "future:document/authority-v9" as DocumentStateToken;
  const futureSession = "future:session/authority-v9" as SessionStateToken;
  assert.equal(
    encodeSessionDocumentTargetParams("body", futureDocument).expectedStateToken,
    futureDocument,
  );
  assert.equal(
    encodeSessionStateTokenParams(futureSession).expectedSessionStateToken,
    futureSession,
  );

  const maximumAscii = "x".repeat(256) as DocumentStateToken;
  const maximumMultibyte = "é".repeat(128) as SessionStateToken;
  assert.equal(
    encodeSessionDocumentTargetParams("body", maximumAscii).expectedStateToken,
    maximumAscii,
  );
  assert.equal(
    encodeSessionStateTokenParams(maximumMultibyte).expectedSessionStateToken,
    maximumMultibyte,
  );

  for (const invalid of ["", "x".repeat(257), "é".repeat(129), "\ud800"]) {
    assert.throws(
      () => encodeSessionDocumentTargetParams("body", invalid as DocumentStateToken),
      /non-empty opaque token of at most 256 UTF-8 bytes/u,
    );
    assert.throws(
      () => encodeSessionStateTokenParams(invalid as SessionStateToken),
      /non-empty opaque token of at most 256 UTF-8 bytes/u,
    );
  }
});

test("session core runtime methods require advertised capabilities", async (context) => {
  const { session } = await openSessionFake(context, "session-no-runtime-methods");
  const unavailable = [
    ["runtime.pending", () => session.pending()],
    ["runtime.settle", () => session.settle(session.stateToken)],
    ["runtime.advance_to_next", () => session.advanceToNext(session.stateToken)],
  ] as const;
  for (const [method, operation] of unavailable) {
    await assert.rejects(
      operation(),
      (error) => error instanceof StasisStateError && error.message.includes(method),
    );
  }
  await session.close();
});

for (const [scenario, operation, expectedEffect, expectedMethod] of [
  [
    "session-abort-active-pending",
    (session: Session, signal: AbortSignal) => session.pending({ signal }),
    "none",
    "runtime.pending",
  ],
  [
    "session-abort-active-settle",
    (session: Session, signal: AbortSignal) =>
      session.settle(session.stateToken, {}, { signal }),
    "indeterminate",
    "runtime.settle",
  ],
] as const) {
  test(`active v2 ${expectedMethod} abort fail-stops with ${expectedEffect} effect`, async (context) => {
    const { session } = await openSessionFake(context, scenario);
    const controller = new AbortController();
    let terminal: StasisAbortError | undefined;
    const command = operation(session, controller.signal);
    const assertion = assert.rejects(command, (error) => {
      assert.ok(error instanceof StasisAbortError);
      terminal = error;
      assert.equal(error.fatal, true);
      assert.equal(error.stateEffect, expectedEffect);
      assert.equal(error.method, expectedMethod);
      assert.match(error.requestId ?? "", /^[1-9][0-9]*$/u);
      return true;
    });
    setImmediate(() => controller.abort(`stop-${expectedMethod}`));
    await assertion;
    await assert.rejects(session.pending(), (error) => error === terminal);
  });
}

test("post-publication import phase-closes invalid inputs without serializing secrets", async (context) => {
  const { session } = await openSessionFake(context);
  const canary = "must-not-cross-the-closed-import-boundary";
  const invalidState = {
    ...initialState,
    profile: `invalid-${canary}`,
    cookies: [
      { ...initialState.cookies[0], name: canary, value: canary },
      { ...initialState.cookies[0], name: canary, value: canary },
    ],
  } as unknown as SessionState;
  const wrongDomainToken = `document:${canary}` as SessionStateToken;

  await assert.rejects(
    session.importState(invalidState, wrongDomainToken),
    (error) => {
      assert.ok(error instanceof StasisProtocolError);
      assert.equal(error.code, "session_state_import_phase_closed");
      assert.equal(error.fatal, false);
      assert.equal(error.stateEffect, "none");
      assert.equal(error.message.includes(canary), false);
      return true;
    },
  );
  assert.equal(session.stderrTail.includes(canary), false);
  const unchanged = await session.exportState();
  assert.deepEqual(unchanged.state, initialState);
});

const invalidSessionAuditResults = [
  ["session-unsorted-query-keys", "sorted and unique", (session: Session) => session.requests()],
  ["session-duplicate-query-keys", "sorted and unique", (session: Session) => session.requests()],
  [
    "session-invalid-evidence-reason",
    "invented_failure_reason",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-incomplete-without-drop",
    "complete may be false only when droppedThroughSeq is present",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-has-more-without-records",
    "hasMore cannot be true without a returned record",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-next-cursor-mismatch",
    "nextAfterSeq must equal the last returned record seq",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-nonincreasing-records",
    "strictly increasing by seq",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-invalid-retention-order",
    "droppedThroughSeq must precede firstRetainedSeq",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-missing-first-retained",
    "firstRetainedSeq is required when records are returned",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-record-before-retention",
    "records cannot precede firstRetainedSeq",
    (session: Session) => session.evidence(),
  ],
  [
    "session-audit-latest-before-record",
    "latestSeq must include every returned record",
    (session: Session) => session.evidence(),
  ],
] as const;

for (const [scenario, expectedMessage, operation] of invalidSessionAuditResults) {
  test(`session audit decoding rejects ${scenario}`, async (context) => {
    const { session } = await openSessionFake(context, scenario);
    await assert.rejects(operation(session), (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "invalid_result");
      assert.match(error.message, new RegExp(expectedMessage, "u"));
      return true;
    });
  });
}

test("session audit pagination accepts an echoed future cursor on an empty page", async (context) => {
  const { session } = await openSessionFake(context, "session-audit-future-cursor");

  const requests = await session.requests({ afterSeq: 100n, limit: 16 });
  assert.deepEqual(requests.records, []);
  assert.equal(requests.nextAfterSeq, 100n);
  assert.equal(requests.latestSeq, 1n);
  assert.equal(requests.complete, true);
  assert.equal(requests.hasMore, false);

  const evidence = await session.evidence({ afterSeq: 100n, limit: 16 });
  assert.deepEqual(evidence.records, []);
  assert.equal(evidence.nextAfterSeq, 100n);
  assert.equal(evidence.latestSeq, 3n);
  assert.equal(evidence.complete, true);
  assert.equal(evidence.hasMore, false);
});

test("session open validation is strict and recovers before a request is written", async (context) => {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, "session-v02"],
  });
  context.after(() => runtime.close());
  const invalidRoute = {
    match: { method: "GET", url: { exact: "https://example.test/" } },
    fulfill: { status: 199 },
    abort: { reason: "not-allow-listed" },
  } as unknown as NetworkRoute;
  await assert.rejects(
    runtime.openSession("https://example.test/", {
      state: {
        ...initialState,
        cookies: new Array(513) as SessionCookie[],
      },
    }),
    /state.cookies must contain at most 512 items/u,
  );
  await assert.rejects(
    runtime.openSession("https://example.test/", {
      network: { mode: "fixtures_only", routes: [invalidRoute] },
    }),
    /exactly one of fulfill or abort/u,
  );

  const session = await runtime.openSession("https://example.test/", {
    network: { mode: "live", routes: [] },
  });
  await session.close();
});

test("session fixture encoding preserves the frozen request-frame budget", async (context) => {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, "session-v02"],
  });
  context.after(() => runtime.close());

  await assert.rejects(
    runtime.openSession("https://example.test/", {
      network: {
        mode: "fixtures_only",
        routes: [
          {
            match: { method: "GET", url: { exact: "https://example.test/oversized" } },
            fulfill: { status: 200, body: { utf8: "x".repeat(384 * 1024) } },
          },
        ],
      },
    }),
    /network must encode to at most 393216 UTF-8 bytes/u,
  );

  const maximumBinaryBody = Buffer.alloc(256 * 1024).toString("base64");
  const session = await runtime.openSession("https://example.test/", {
    network: {
      mode: "fixtures_only",
      routes: [
        {
          match: { method: "GET", url: { exact: "https://example.test/maximum-binary" } },
          fulfill: { status: 200, body: { base64: maximumBinaryBody } },
        },
      ],
    },
  });
  await session.close();
});

// Keep the brands referenced as public compile-time API even under isolatedDeclarations-style use.
type _OpaqueTokenProof = [DocumentStateToken, SessionStateToken];
