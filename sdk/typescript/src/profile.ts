/** The exact controlled browser subset supported by Stasis 0.1. */
export const CONTROLLED_WEBAPP_V1_PROFILE = "controlled-webapp-v1" as const;

/** The bounded controlled session subset introduced by the additive Stasis 0.2 API. */
export const CONTROLLED_WEB_SESSION_V1_PROFILE = "controlled-web-session-v1" as const;

/**
 * The original Stasis 0.1 profile alias. Keep this exact legacy literal so
 * existing generic constraints and exhaustive checks remain source-compatible.
 */
export type SupportProfile = typeof CONTROLLED_WEBAPP_V1_PROFILE;
export type LegacySupportProfile = SupportProfile;
export type SessionSupportProfile = typeof CONTROLLED_WEB_SESSION_V1_PROFILE;
/** Every support profile accepted across the legacy and session APIs. */
export type AnySupportProfile = LegacySupportProfile | SessionSupportProfile;
