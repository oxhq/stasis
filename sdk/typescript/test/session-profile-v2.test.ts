import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  SESSION_SUPPORT_PROFILES,
  StasisStateError,
  StasisTransportError,
  createStasisSessionPool,
  launch,
  settlementEvidence,
  type AnySupportProfile,
  type Runtime,
  type SelectableSessionProfile,
  type Session,
  type SessionOpenOptions,
  type SessionSettleResult,
  type SessionState,
  type SessionSupportProfile,
  type SettlementEvidenceV2,
} from "../src/index.js";

const fixture = fileURLToPath(new URL("./fixtures/fake-shell.mjs", import.meta.url));

const stateArtifact = {
  schemaVersion: 1,
  profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
  sensitive: true,
  sessionStorageScope: "top_level_browsing_context",
  cookies: [],
  origins: [],
} as const satisfies SessionState;

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
    state: stateArtifact,
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
    state: stateArtifact,
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
    CONTROLLED_WEB_SESSION_V1_PROFILE,
    "v2 expands execution sources but deliberately reuses the v1 state artifact",
  );
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

test("v2 rejects a state artifact that claims a new profile identity", async (context) => {
  const runtime = await fakeRuntime(context, "session-profile-v2-state-boundary");
  const invalidState = {
    ...stateArtifact,
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
  } as unknown as SessionState;

  await assert.rejects(
    runtime.openSession("https://example.test/", {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
      state: invalidState,
    }),
    /state\.profile must be controlled-web-session-v1/u,
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
