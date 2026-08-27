import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  SESSION_SUPPORT_PROFILES,
  StasisStateError,
  StasisTransportError,
  crawlWithStasis,
  createStasisSessionPool,
  launch,
  settlementEvidence,
  type AnySupportProfile,
  type ReferenceCrawlerOptions,
  type ReferenceCrawlerPool,
  type ReferenceCrawlerSession,
  type Runtime,
  type SelectableSessionProfile,
  type Session,
  type SessionCookieV2,
  type SessionOpenOptions,
  type SessionSettleResult,
  type SessionState,
  type SessionStateV2,
  type SessionSupportProfile,
  type SettlementEvidenceV2,
} from "../src/index.js";

const fixture = fileURLToPath(new URL("./fixtures/fake-shell.mjs", import.meta.url));

const v1StateArtifact = {
  schemaVersion: 1,
  profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
  sensitive: true,
  sessionStorageScope: "top_level_browsing_context",
  cookies: [],
  origins: [],
} as const satisfies SessionState;

const persistentCookie = {
  name: "remember_me",
  value: "sensitive-cookie-value",
  domain: "example.test",
  path: "/",
  hostOnly: true,
  secure: true,
  httpOnly: true,
  sameSite: "lax",
  expiresUnixTimeNs: 2_592_000_000_000_000n,
  partitioned: false,
  creationSequence: 1n,
  lastAccessSequence: 2n,
} as const satisfies SessionCookieV2;

const v2StateArtifact = {
  schemaVersion: 1,
  profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
  sensitive: true,
  sessionStorageScope: "top_level_browsing_context",
  cookies: [persistentCookie],
  origins: [],
} as const satisfies SessionStateV2;

if (false) {
  const runtime = null as unknown as Runtime;
  const stableAlias: SessionSupportProfile = CONTROLLED_WEB_SESSION_V1_PROFILE;
  const stableAnyAlias: AnySupportProfile = CONTROLLED_WEB_SESSION_V1_PROFILE;
  const selectableCandidate: SelectableSessionProfile = CONTROLLED_WEB_SESSION_V2_PROFILE;
  // @ts-expect-error The compatibility alias must remain the exact stable v1 literal.
  const widenedStableAlias: SessionSupportProfile = CONTROLLED_WEB_SESSION_V2_PROFILE;
  // @ts-expect-error The compatibility union must not silently acquire the candidate literal.
  const widenedAnyAlias: AnySupportProfile = CONTROLLED_WEB_SESSION_V2_PROFILE;
  const bareOpenOptions: SessionOpenOptions = {};
  const widenedBareOpenOptions: SessionOpenOptions = {
    // @ts-expect-error Candidate selection requires an explicit candidate-aware type argument.
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
  };
  const bareSession = null as unknown as Session;
  const bareSessionProfile: typeof CONTROLLED_WEB_SESSION_V1_PROFILE = bareSession.profile;
  const bareEvidence = null as unknown as SettlementEvidenceV2;
  const bareEvidenceProfile: typeof CONTROLLED_WEB_SESSION_V1_PROFILE = bareEvidence.profile;
  const defaultSession: Promise<Session<typeof CONTROLLED_WEB_SESSION_V1_PROFILE>> =
    runtime.openSession("https://example.test/");
  // @ts-expect-error Omitted profile selection cannot be inferred from a v2 contextual return type.
  const impossibleCandidate: Promise<Session<typeof CONTROLLED_WEB_SESSION_V2_PROFILE>> =
    runtime.openSession("https://example.test/");
  const candidateOptions: SessionOpenOptions<typeof CONTROLLED_WEB_SESSION_V2_PROFILE> & {
    readonly profile: typeof CONTROLLED_WEB_SESSION_V2_PROFILE;
  } = {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    state: v2StateArtifact,
  };
  const candidateSession: Promise<Session<typeof CONTROLLED_WEB_SESSION_V2_PROFILE>> =
    runtime.openSession("https://example.test/", candidateOptions);
  const candidateSettled: Promise<SessionSettleResult<typeof CONTROLLED_WEB_SESSION_V2_PROFILE>> =
    candidateSession.then((session) => session.settle(session.stateToken));
  const inferredCandidateEvidence: Promise<
    SettlementEvidenceV2<typeof CONTROLLED_WEB_SESSION_V2_PROFILE>
  > = candidateSettled.then((result) => settlementEvidence(result));
  const candidateResult = null as unknown as SessionSettleResult<
    typeof CONTROLLED_WEB_SESSION_V2_PROFILE
  >;
  const stableResult = null as unknown as SessionSettleResult;
  const copiedCandidateResult = { ...candidateResult };
  const copiedCandidateEvidence: SettlementEvidenceV2<
    typeof CONTROLLED_WEB_SESSION_V1_PROFILE
  > = settlementEvidence(copiedCandidateResult);
  settlementEvidence(candidateResult, CONTROLLED_WEB_SESSION_V2_PROFILE);
  settlementEvidence(stableResult, CONTROLLED_WEB_SESSION_V1_PROFILE);
  // @ts-expect-error An SDK-bound v2 result cannot be explicitly relabeled as v1.
  settlementEvidence(candidateResult, CONTROLLED_WEB_SESSION_V1_PROFILE);
  // @ts-expect-error An unbound/manual result is the legacy-v1 result shape only.
  settlementEvidence(stableResult, CONTROLLED_WEB_SESSION_V2_PROFILE);
  // @ts-expect-error Spreading drops the private SDK binding and restores the legacy-v1 shape.
  const falselyBoundCopy: SettlementEvidenceV2<typeof CONTROLLED_WEB_SESSION_V2_PROFILE> =
    settlementEvidence(copiedCandidateResult);
  const stablePool = createStasisSessionPool({ maxProcesses: 1, maxQueue: 0 });
  const stablePooledSession: Promise<Session<typeof CONTROLLED_WEB_SESSION_V1_PROFILE>> =
    stablePool.acquire({ url: "https://example.test/" }).then((lease) => lease.session);
  const stableCookieExpiry: null = (
    null as unknown as Awaited<ReturnType<Session["getCookies"]>>
  ).cookies[0]!.expiresUnixTimeNs;
  const candidateSessionValue = null as unknown as Session<
    typeof CONTROLLED_WEB_SESSION_V2_PROFILE
  >;
  const candidateCookieExpiry: bigint | null = (
    null as unknown as Awaited<ReturnType<typeof candidateSessionValue.getCookies>>
  ).cookies[0]!.expiresUnixTimeNs;
  const mismatchedCandidateState: SessionOpenOptions<typeof CONTROLLED_WEB_SESSION_V2_PROFILE> = {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    // @ts-expect-error A v1 state artifact cannot be imported into a v2 session.
    state: v1StateArtifact,
  };
  // @ts-expect-error Direct open inference must not widen v2 into a v1-or-v2 state union.
  const mismatchedCandidateOpen = runtime.openSession("https://example.test/", {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    state: v1StateArtifact,
  });
  const stableCrawlerOptions: ReferenceCrawlerOptions = {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    state: v1StateArtifact,
  };
  const candidateCrawlerOptions: ReferenceCrawlerOptions<
    typeof CONTROLLED_WEB_SESSION_V2_PROFILE
  > = {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    state: v2StateArtifact,
  };
  const incompleteCandidateCrawlerOptions: ReferenceCrawlerOptions<
    typeof CONTROLLED_WEB_SESSION_V2_PROFILE
  > = {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    state: v2StateArtifact,
  };
  const mismatchedCandidateCrawlerOptions: ReferenceCrawlerOptions<
    typeof CONTROLLED_WEB_SESSION_V2_PROFILE
  > = {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    // @ts-expect-error Reference-crawler v2 selection requires an exact v2 state artifact.
    state: v1StateArtifact,
  };
  const implicitCandidateCrawlerState: ReferenceCrawlerOptions = {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    // @ts-expect-error A v2 crawler state cannot select the omitted/default v1 profile.
    state: v2StateArtifact,
  };
  const referenceCrawlerPool = null as unknown as ReferenceCrawlerPool<ReferenceCrawlerSession>;
  const incompleteCandidateCrawl = crawlWithStasis(
    referenceCrawlerPool,
    // @ts-expect-error A candidate-aware annotation is not callable until profile is explicit.
    incompleteCandidateCrawlerOptions,
  );
  // @ts-expect-error Direct crawler inference must not widen v2 into a v1-or-v2 state union.
  const mismatchedCandidateCrawl = crawlWithStasis(referenceCrawlerPool, {
    start: "https://example.test/",
    maxPages: 1,
    maxDepth: 0,
    concurrency: 1,
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    state: v1StateArtifact,
  });
  void stableAlias;
  void stableAnyAlias;
  void selectableCandidate;
  void widenedStableAlias;
  void widenedAnyAlias;
  void bareOpenOptions;
  void widenedBareOpenOptions;
  void bareSessionProfile;
  void bareEvidenceProfile;
  void defaultSession;
  void impossibleCandidate;
  void candidateSession;
  void candidateSettled;
  void inferredCandidateEvidence;
  void candidateResult;
  void stableResult;
  void copiedCandidateEvidence;
  void falselyBoundCopy;
  void stablePool;
  void stablePooledSession;
  void stableCookieExpiry;
  void candidateCookieExpiry;
  void mismatchedCandidateState;
  void mismatchedCandidateOpen;
  void stableCrawlerOptions;
  void candidateCrawlerOptions;
  void incompleteCandidateCrawlerOptions;
  void mismatchedCandidateCrawlerOptions;
  void implicitCandidateCrawlerState;
  void referenceCrawlerPool;
  void incompleteCandidateCrawl;
  void mismatchedCandidateCrawl;
}

async function fakeRuntime(
  context: { after(callback: () => void | Promise<void>): void },
  scenario: string,
): Promise<Runtime> {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, scenario],
  });
  context.after(() => runtime.close().catch(() => undefined));
  return runtime;
}

test("openSession keeps v1 as the default profile", async (context) => {
  const runtime = await fakeRuntime(context, "session-profile-default");
  const session = await runtime.openSession("https://example.test/");

  assert.deepEqual(SESSION_SUPPORT_PROFILES, [
    CONTROLLED_WEB_SESSION_V1_PROFILE,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
  ]);
  assert.equal(Object.isFrozen(SESSION_SUPPORT_PROFILES), true);
  assert.equal(
    Reflect.set(SESSION_SUPPORT_PROFILES, 0, CONTROLLED_WEB_SESSION_V2_PROFILE),
    false,
  );
  assert.equal(session.profile, CONTROLLED_WEB_SESSION_V1_PROFILE);
  await session.close();
});

test("explicit v2 selection binds capability, response, state, and evidence identity", async (context) => {
  const runtime = await fakeRuntime(context, "session-profile-v2");
  const session = await runtime.openSession("https://example.test/", {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    state: v2StateArtifact,
  });

  assert.ok(runtime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V2_PROFILE));
  assert.equal(session.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const settled = await session.settle(session.stateToken);
  assert.equal(
    session.settlementEvidence(settled).profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
  );
  assert.equal(
    settlementEvidence(settled, CONTROLLED_WEB_SESSION_V2_PROFILE).profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
  );
  assert.equal(
    settlementEvidence(settled).profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
    "an SDK result must retain its runtime-bound selected profile",
  );
  assert.throws(
    () =>
      settlementEvidence(
        settled as SessionSettleResult<SelectableSessionProfile>,
        CONTROLLED_WEB_SESSION_V1_PROFILE,
      ),
    /does not match runtime-bound profile controlled-web-session-v2/u,
  );
  const unboundResult = { ...settled };
  assert.equal(
    settlementEvidence(unboundResult).profile,
    CONTROLLED_WEB_SESSION_V1_PROFILE,
    "an unbound/manual result must retain the legacy-v1 evidence identity",
  );
  assert.throws(
    () =>
      settlementEvidence(
        unboundResult as unknown as SessionSettleResult<
          typeof CONTROLLED_WEB_SESSION_V2_PROFILE
        >,
        CONTROLLED_WEB_SESSION_V2_PROFILE,
      ),
    /Unbound session settle results can only produce controlled-web-session-v1 evidence/u,
  );

  const exported = await session.exportState();
  assert.equal(
    exported.state.profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
    "v2 exports a profile-matched state artifact",
  );
  assert.equal(exported.state.schemaVersion, 1);
  assert.equal(exported.state.cookies[0]?.expiresUnixTimeNs, persistentCookie.expiresUnixTimeNs);

  const cookies = await session.getCookies();
  assert.equal(cookies.cookies[0]?.expiresUnixTimeNs, persistentCookie.expiresUnixTimeNs);
  const replacementExpiry = persistentCookie.expiresUnixTimeNs + 1n;
  const mutation = await session.setCookies(
    [{ ...persistentCookie, expiresUnixTimeNs: replacementExpiry }],
    cookies.sessionStateToken,
  );
  const replaced = await session.getCookies();
  assert.notEqual(mutation.sessionStateToken, cookies.sessionStateToken);
  assert.equal(replaced.cookies[0]?.expiresUnixTimeNs, replacementExpiry);
  await session.close();
});

test("explicit v2 selection fails before open when the runtime does not advertise it", async (context) => {
  const runtime = await fakeRuntime(context, "session-v2-unadvertised");

  await assert.rejects(
    runtime.openSession("https://example.test/", {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    }),
    (error) => {
      assert.ok(error instanceof StasisStateError);
      assert.match(error.message, /controlled-web-session-v2/u);
      return true;
    },
  );
});

test("explicit v2 selection rejects a mismatched open response profile", async (context) => {
  const runtime = await fakeRuntime(context, "session-v2-response-mismatch");

  await assert.rejects(
    runtime.openSession("https://example.test/", {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    }),
    (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.match(error.message, /requested profile controlled-web-session-v2/u);
      return true;
    },
  );
});

test("v2 rejects a v1 state artifact instead of migrating it implicitly", async (context) => {
  const runtime = await fakeRuntime(context, "session-profile-v2-state-boundary");
  const invalidState = {
    ...v1StateArtifact,
  } as unknown as SessionStateV2;

  await assert.rejects(
    runtime.openSession("https://example.test/", {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
      state: invalidState,
    }),
    /state\.profile must be controlled-web-session-v2/u,
  );
});

test("the production pool forwards explicit v2 selection to each fresh process", async (context) => {
  const pool = createStasisSessionPool<typeof CONTROLLED_WEB_SESSION_V2_PROFILE>({
    maxProcesses: 1,
    maxQueue: 0,
    launch: {
      executablePath: process.execPath,
      args: [fixture, "session-profile-v2-pool"],
    },
  });
  context.after(() => pool.close());

  const lease = await pool.acquire({
    url: "https://example.test/",
    options: { profile: CONTROLLED_WEB_SESSION_V2_PROFILE },
  });
  assert.equal(lease.session.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
  await lease.release();
  await pool.close();
});
