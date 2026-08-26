/** The exact controlled browser subset supported by Stasis 0.1. */
export const CONTROLLED_WEBAPP_V1_PROFILE = "controlled-webapp-v1" as const;

/** The bounded controlled session subset introduced by the additive Stasis 0.2 API. */
export const CONTROLLED_WEB_SESSION_V1_PROFILE = "controlled-web-session-v1" as const;

/** Candidate Stasis 0.3 session profile with controlled local MessageChannel delivery. */
export const CONTROLLED_WEB_SESSION_V2_PROFILE = "controlled-web-session-v2" as const;

/** Session profiles understood by this SDK, in compatibility order. */
export const SESSION_SUPPORT_PROFILES = Object.freeze([
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  CONTROLLED_WEB_SESSION_V2_PROFILE,
] as const);

/**
 * The original Stasis 0.1 profile alias. Keep this exact legacy literal so
 * existing generic constraints and exhaustive checks remain source-compatible.
 */
export type SupportProfile = typeof CONTROLLED_WEBAPP_V1_PROFILE;
export type LegacySupportProfile = SupportProfile;
/** The frozen stable session profile retained for source compatibility. */
export type SessionSupportProfile = typeof CONTROLLED_WEB_SESSION_V1_PROFILE;
/** Every session profile that may be selected explicitly by this SDK. */
export type SelectableSessionProfile = (typeof SESSION_SUPPORT_PROFILES)[number];
/** Frozen stable profiles accepted across the legacy and session APIs. */
export type AnySupportProfile = LegacySupportProfile | SessionSupportProfile;

/** @internal */
export function isSelectableSessionProfile(value: unknown): value is SelectableSessionProfile {
  return (
    value === CONTROLLED_WEB_SESSION_V1_PROFILE ||
    value === CONTROLLED_WEB_SESSION_V2_PROFILE
  );
}
