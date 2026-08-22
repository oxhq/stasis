/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Product-owned NDJSON shapes for the controlled-runtime MVP.
//!
//! Servo observations are same-build evidence, not a public protocol. This module is the only
//! projection boundary: it intentionally omits process-local clock, scheduler, timer, pipeline,
//! event-loop, producer-fence, and guarded-advance identities.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use embedder_traits::document_automation::{
    DocumentAutomationLimits, DocumentAutomationOperation, DocumentAutomationRequest,
    DocumentAutomationRequestError, DocumentAutomationResult as EngineDocumentAutomationResult,
    DocumentExtractionField, DocumentExtractionPlan, DocumentExtractionRead,
};
use embedder_traits::document_pending::{
    PendingClockMode, PendingExternalIoLoadBlocking, PendingExternalIoObservation,
    PendingExternalIoOwner, PendingExternalIoPhase, PendingLogicalTimerSnapshot,
    PendingNetworkKind, PendingOpenEndedSourceReason, PendingParserPhase,
    PendingPipelineRenderingObservation, PendingProducerStability,
    PendingRenderingPipelineActivity, PendingSourceDisposition, PendingSourceKind,
    PendingSourceObservation, PendingTargetObservation, PendingUnsupportedSourceReason,
    RawPendingSnapshot, RuntimeStateGeneration,
};
use serde::de::value::MapAccessDeserializer;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use servo_base::id::ScriptEventLoopId;
use timers::DocumentTimeSurface;

use crate::settle::{
    PersistentWork as SettlePersistentWork, SettleCompletion, SettlePolicy as EngineSettlePolicy,
    SettleRuntimeFailure,
};

/// An exact non-negative integer encoded as a canonical decimal JSON string.
///
/// JavaScript cannot represent every engine timestamp or generation as a number. Keeping the
/// string wrapper at the DTO boundary prevents an accidental lossy projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DecimalU128(String);

impl DecimalU128 {
    pub fn new(value: u128) -> Self {
        Self(value.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn get(&self) -> u128 {
        self.0
            .parse()
            .expect("DecimalU128 always contains a validated u128")
    }
}

impl From<u64> for DecimalU128 {
    fn from(value: u64) -> Self {
        Self::new(u128::from(value))
    }
}

impl From<u128> for DecimalU128 {
    fn from(value: u128) -> Self {
        Self::new(value)
    }
}

impl Serialize for DecimalU128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

struct DecimalU128Visitor;

impl Visitor<'_> for DecimalU128Visitor {
    type Value = DecimalU128;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a canonical non-negative decimal u128 string")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.is_empty() ||
            (value.len() > 1 && value.starts_with('0')) ||
            !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(E::custom(
                "expected a canonical non-negative decimal string",
            ));
        }
        value
            .parse::<u128>()
            .map_err(|_| E::custom("decimal value exceeds u128"))?;
        Ok(DecimalU128(value.to_owned()))
    }
}

impl<'de> Deserialize<'de> for DecimalU128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(DecimalU128Visitor)
    }
}

/// Public automation operations supported by the native product wire.
///
/// `Fill` and standalone `InnerHtml` deliberately have no public variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicAutomationKind {
    Activate,
    Query,
    Text,
    Extract,
}

/// A strict, bounded public automation request after wire validation.
///
/// The target remains private Servo authority and is bound only after the shell obtains a fresh
/// observation. None of this type is serialized back to the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAutomationParams {
    kind: PublicAutomationKind,
    expected_generation: RuntimeStateGeneration,
    operation: DocumentAutomationOperation,
}

impl ResolvedAutomationParams {
    fn new(
        kind: PublicAutomationKind,
        expected_generation: DecimalU128,
        operation: DocumentAutomationOperation,
    ) -> Result<Self, AutomationParamsError> {
        let expected_generation = u64::try_from(expected_generation.get())
            .map(RuntimeStateGeneration::new)
            .map_err(|_| AutomationParamsError::ExpectedGenerationOutOfRange)?;
        operation
            .validate(DocumentAutomationLimits::MVP)
            .map_err(AutomationParamsError::InvalidOperation)?;
        Ok(Self {
            kind,
            expected_generation,
            operation,
        })
    }

    pub const fn kind(&self) -> PublicAutomationKind {
        self.kind
    }

    pub const fn expected_generation(&self) -> RuntimeStateGeneration {
        self.expected_generation
    }

    /// Bind client data to fresh private target authority without weakening engine limits.
    pub fn bind_to_target(
        self,
        target: PendingTargetObservation,
    ) -> Result<DocumentAutomationRequest, DocumentAutomationRequestError> {
        DocumentAutomationRequest::new_internal(
            target,
            self.expected_generation,
            self.operation,
            DocumentAutomationLimits::MVP,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutomationParamsError {
    ExpectedGenerationOutOfRange,
    InvalidOperation(DocumentAutomationRequestError),
}

/// Strict parameters for `action.activate`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ActionActivateParams {
    selector: String,
    expected_generation: DecimalU128,
}

impl ActionActivateParams {
    pub fn resolve(self) -> Result<ResolvedAutomationParams, AutomationParamsError> {
        ResolvedAutomationParams::new(
            PublicAutomationKind::Activate,
            self.expected_generation,
            DocumentAutomationOperation::Activate {
                selector: self.selector,
            },
        )
    }
}

/// Strict parameters for `dom.query`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DomQueryParams {
    selector: String,
    expected_generation: DecimalU128,
}

impl DomQueryParams {
    pub fn resolve(self) -> Result<ResolvedAutomationParams, AutomationParamsError> {
        ResolvedAutomationParams::new(
            PublicAutomationKind::Query,
            self.expected_generation,
            DocumentAutomationOperation::QueryCount {
                selector: self.selector,
            },
        )
    }
}

/// Strict parameters for `dom.text`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DomTextParams {
    selector: String,
    expected_generation: DecimalU128,
}

impl DomTextParams {
    pub fn resolve(self) -> Result<ResolvedAutomationParams, AutomationParamsError> {
        ResolvedAutomationParams::new(
            PublicAutomationKind::Text,
            self.expected_generation,
            DocumentAutomationOperation::TextContent {
                selector: self.selector,
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DomExtractRead {
    Text,
    Html,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DomExtractFieldParams {
    name: String,
    selector: String,
    read: DomExtractRead,
}

/// Strict parameters for `dom.extract`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DomExtractParams {
    root_selector: String,
    fields: Vec<DomExtractFieldParams>,
    expected_generation: DecimalU128,
}

impl DomExtractParams {
    pub fn resolve(self) -> Result<ResolvedAutomationParams, AutomationParamsError> {
        let fields = self
            .fields
            .into_iter()
            .map(|field| {
                DocumentExtractionField::new_internal(
                    field.name,
                    field.selector,
                    match field.read {
                        DomExtractRead::Text => DocumentExtractionRead::TextContent,
                        DomExtractRead::Html => DocumentExtractionRead::InnerHtml,
                    },
                )
            })
            .collect();
        ResolvedAutomationParams::new(
            PublicAutomationKind::Extract,
            self.expected_generation,
            DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
                self.root_selector,
                fields,
            )),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionActivateResult {
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomQueryResult {
    count: DecimalU128,
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomTextResult {
    value: String,
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DomExtractValue {
    name: String,
    value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DomExtractRow {
    fields: Vec<DomExtractValue>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomExtractResult {
    rows: Vec<DomExtractRow>,
    state_generation: DecimalU128,
}

/// The exact public result shape selected by the request method.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PublicAutomationResult {
    Activate(ActionActivateResult),
    Query(DomQueryResult),
    Text(DomTextResult),
    Extract(DomExtractResult),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationResultProjectionError {
    UnexpectedResult,
    InternalOnlyResult,
}

impl PublicAutomationResult {
    /// Project an engine result together with its authoritative post-operation generation.
    pub fn project(
        expected: PublicAutomationKind,
        result: EngineDocumentAutomationResult,
        post_operation: &RawPendingSnapshot,
    ) -> Result<Self, AutomationResultProjectionError> {
        let state_generation = post_operation.state_generation.get().into();
        match (expected, result) {
            (PublicAutomationKind::Activate, EngineDocumentAutomationResult::Activated) => {
                Ok(Self::Activate(ActionActivateResult { state_generation }))
            },
            (PublicAutomationKind::Query, EngineDocumentAutomationResult::QueryCount { count }) => {
                Ok(Self::Query(DomQueryResult {
                    count: u128::from(count).into(),
                    state_generation,
                }))
            },
            (PublicAutomationKind::Text, EngineDocumentAutomationResult::TextContent { value }) => {
                Ok(Self::Text(DomTextResult {
                    value,
                    state_generation,
                }))
            },
            (PublicAutomationKind::Extract, EngineDocumentAutomationResult::Extract { rows }) => {
                Ok(Self::Extract(DomExtractResult {
                    rows: rows
                        .into_iter()
                        .map(|row| DomExtractRow {
                            fields: row
                                .fields
                                .into_iter()
                                .map(|field| DomExtractValue {
                                    name: field.name,
                                    value: field.value,
                                })
                                .collect(),
                        })
                        .collect(),
                    state_generation,
                }))
            },
            (_, EngineDocumentAutomationResult::InnerHtml { .. }) |
            (_, EngineDocumentAutomationResult::Filled) => {
                Err(AutomationResultProjectionError::InternalOnlyResult)
            },
            _ => Err(AutomationResultProjectionError::UnexpectedResult),
        }
    }
}

/// Opaque, session-local source identity. Its numeric engine origin is not part of the API.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OpaqueSourceId(String);

impl OpaqueSourceId {
    fn from_alias(value: u128) -> Self {
        Self(value.to_string())
    }
}

/// Session-owned product projection of engine source identities.
///
/// The raw allocator value is same-build control evidence. Compact canonical-decimal aliases are
/// assigned when a source is first observed, preserving cross-response correlation without
/// exposing allocator values or gaps as public wire ABI. One context must live for exactly one
/// shell session.
pub struct WireProjectionContext {
    event_loop_id: Option<ScriptEventLoopId>,
    source_ids: BTreeMap<u64, OpaqueSourceId>,
    next_source_alias: u128,
}

impl Default for WireProjectionContext {
    fn default() -> Self {
        Self {
            event_loop_id: None,
            source_ids: BTreeMap::new(),
            next_source_alias: 1,
        }
    }
}

impl WireProjectionContext {
    pub fn new() -> Self {
        Self::default()
    }

    fn observe_pending(&mut self, raw: &RawPendingSnapshot) {
        self.observe_event_loop(raw.target.event_loop_id);
        for source in raw.sources.sources() {
            self.source_id(source.id.get());
        }
    }

    fn observe_event_loop(&mut self, event_loop_id: ScriptEventLoopId) {
        if self.event_loop_id != Some(event_loop_id) {
            self.event_loop_id = Some(event_loop_id);
            self.source_ids.clear();
        }
    }

    fn source_id(&mut self, engine_id: u64) -> OpaqueSourceId {
        if let Some(projected) = self.source_ids.get(&engine_id) {
            return projected.clone();
        }
        let alias = OpaqueSourceId::from_alias(self.next_source_alias);
        self.next_source_alias = self
            .next_source_alias
            .checked_add(1)
            .expect("a shell session cannot allocate more than u128::MAX source aliases");
        self.source_ids.insert(engine_id, alias.clone());
        alias
    }
}

/// Strict parameters for `runtime.pending`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimePendingParams {}

/// Strict parameters for `runtime.advance_to_next`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeAdvanceToNextParams {}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentWorkPolicy {
    #[default]
    Report,
    Strict,
}

/// Strict parameters for `runtime.settle`. Omitted fields select product defaults.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeSettleParams {
    pub persistent_work: PersistentWorkPolicy,
    pub max_virtual_time_ns: Option<DecimalU128>,
    pub max_control_turns: Option<DecimalU128>,
    pub wall_io_timeout_ns: Option<DecimalU128>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParamsInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RuntimeSettleParamsInput {
    #[serde(default)]
    persistent_work: PersistentWorkPolicy,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    max_virtual_time_ns: Option<DecimalU128>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    max_control_turns: Option<DecimalU128>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    wall_io_timeout_ns: Option<DecimalU128>,
}

impl<'de> Deserialize<'de> for RuntimePendingParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_object::<D, EmptyParamsInput>(deserializer)?;
        Ok(Self {})
    }
}

impl<'de> Deserialize<'de> for RuntimeAdvanceToNextParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_object::<D, EmptyParamsInput>(deserializer)?;
        Ok(Self {})
    }
}

impl<'de> Deserialize<'de> for RuntimeSettleParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = deserialize_object::<D, RuntimeSettleParamsInput>(deserializer)?;
        Ok(Self {
            persistent_work: input.persistent_work,
            max_virtual_time_ns: input.max_virtual_time_ns,
            max_control_turns: input.max_control_turns,
            wall_io_timeout_ns: input.wall_io_timeout_ns,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettleParamsError {
    DurationOutOfRange(&'static str),
    CountOutOfRange(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedSettlePolicy {
    pub persistent_work: PersistentWorkPolicy,
    pub engine: EngineSettlePolicy,
}

impl RuntimeSettleParams {
    pub fn resolve(
        self,
        defaults: EngineSettlePolicy,
    ) -> Result<ResolvedSettlePolicy, SettleParamsError> {
        let engine = EngineSettlePolicy {
            max_virtual_time: match self.max_virtual_time_ns {
                Some(value) => checked_duration(value.get())
                    .ok_or(SettleParamsError::DurationOutOfRange("maxVirtualTimeNs"))?,
                None => defaults.max_virtual_time,
            },
            max_control_turns: match self.max_control_turns {
                Some(value) => u64::try_from(value.get())
                    .map_err(|_| SettleParamsError::CountOutOfRange("maxControlTurns"))?,
                None => defaults.max_control_turns,
            },
            wall_io_timeout: match self.wall_io_timeout_ns {
                Some(value) => checked_duration(value.get())
                    .ok_or(SettleParamsError::DurationOutOfRange("wallIoTimeoutNs"))?,
                None => defaults.wall_io_timeout,
            },
        };
        Ok(ResolvedSettlePolicy {
            persistent_work: self.persistent_work,
            engine,
        })
    }
}

fn checked_duration(nanos: u128) -> Option<Duration> {
    let seconds = u64::try_from(nanos / 1_000_000_000).ok()?;
    let subsec_nanos = u32::try_from(nanos % 1_000_000_000).ok()?;
    Some(Duration::new(seconds, subsec_nanos))
}

fn deserialize_optional_decimal<'de, D>(deserializer: D) -> Result<Option<DecimalU128>, D::Error>
where
    D: Deserializer<'de>,
{
    DecimalU128::deserialize(deserializer).map(Some)
}

fn deserialize_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for ObjectVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(ObjectVisitor(PhantomData))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockMode {
    Real,
    Controlled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeSurface {
    WindowTimers,
    SameEventLoopIframe,
    JavaScriptDate,
    Performance,
    HostTimestamp,
    UpdateRendering,
    AnimationFrame,
    DocumentTimeline,
    Worker,
    Worklet,
    CrossEventLoopIframe,
    CrossEventLoopNavigation,
    AuxiliaryWebView,
    ResourceThreadIo,
    ExternalSubscription,
    NativeMedia,
    EmbedderControl,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockSnapshot {
    pub mode: ClockMode,
    pub unsupported_surfaces: Vec<TimeSurface>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub ready: DecimalU128,
    pub throttled: DecimalU128,
    pub inactive: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputSnapshot {
    pub ready_events: DecimalU128,
    pub intake_saturated: bool,
    pub tasks: TaskSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrotaskSnapshot {
    pub queued: DecimalU128,
    pub checkpoint_in_progress: bool,
    pub terminal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerStability {
    NotCheckpointed,
    Busy,
    FirstEmpty,
    StableEmpty,
    Unqualified,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProducerSnapshot {
    pub pending: DecimalU128,
    pub stability: ProducerStability,
    pub terminal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerSnapshot {
    pub ready: DecimalU128,
    pub future_finite: DecimalU128,
    pub persistent: DecimalU128,
    pub unsupported: DecimalU128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_deadline_ns: Option<DecimalU128>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParserSnapshot {
    pub total: DecimalU128,
    pub ready: DecimalU128,
    pub awaiting_external_io: DecimalU128,
    pub awaiting_commit: DecimalU128,
    pub awaiting_script_input: DecimalU128,
    pub suspended: DecimalU128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkKind {
    Navigation,
    Fetch,
    XmlHttpRequest,
    Image,
    Font,
    Stylesheet,
    Script,
    /// Conservative external-I/O evidence with no physical request classification.
    UnclassifiedProducerIo,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalIoPhase {
    Queued,
    AwaitingResponse,
    StreamingBody,
    TerminalTaskQueued,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalIoOwner {
    TopLevelNavigation,
    DocumentParser,
    Script,
    DocumentSubresource,
    RenderingResource,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBlocking {
    Blocking,
    NonBlocking,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalIoSnapshot {
    pub source_id: OpaqueSourceId,
    pub kind: NetworkKind,
    pub phase: ExternalIoPhase,
    pub owner: ExternalIoOwner,
    pub load_blocking: LoadBlocking,
    pub started_at_ns: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkCounts {
    pub navigation: DecimalU128,
    pub fetch: DecimalU128,
    pub xml_http_request: DecimalU128,
    pub image: DecimalU128,
    pub font: DecimalU128,
    pub stylesheet: DecimalU128,
    pub script: DecimalU128,
    pub unclassified_producer_io: DecimalU128,
    pub other: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSnapshot {
    pub counts: NetworkCounts,
    pub active: Vec<ExternalIoSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderingSnapshot {
    pub opportunity_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_opportunity_ns: Option<DecimalU128>,
    pub retained_animation_frames: DecimalU128,
    pub runnable_animation_frames: DecimalU128,
    pub update_required: bool,
    pub pending_animation_events: DecimalU128,
    pub finite_animations: DecimalU128,
    pub persistent_animations: DecimalU128,
    pub unsupported_animations: DecimalU128,
    pub finite_animated_images: DecimalU128,
    pub persistent_animated_images: DecimalU128,
    pub unsupported_animated_images: DecimalU128,
    pub image_update_ready: bool,
    pub dirty_canvases: DecimalU128,
    pub canvas_upload_pending: bool,
    pub unsupported_canvases: DecimalU128,
    pub pending_fonts: DecimalU128,
    pub pending_images: DecimalU128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Task,
    Microtask,
    Timer,
    AnimationFrame,
    Animation,
    Network,
    Parser,
    RenderingUpdate,
    TrackedPresence,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentReason {
    Interval,
    InfiniteAnimation,
    InfiniteAnimatedImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenEndedReason {
    Interval,
    InfiniteAnimation,
    WebSocket,
    EventSource,
    BroadcastChannel,
    MessagePort,
    EmbedderControl,
    MediaSessionActionHandler,
    StorageEventListener,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistentDescription {
    pub reason: PersistentReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_period_ns: Option<DecimalU128>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenEndedDescription {
    pub reason: OpenEndedReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_period_ns: Option<DecimalU128>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedReason {
    TimeSurface,
    UnclassifiedTimer,
    UnclassifiedAnimation,
    AnimatedImage,
    WebSocket,
    EventSource,
    BroadcastChannel,
    MessagePort,
    EmbedderControl,
    MediaSessionActionHandler,
    StorageEventListener,
    ClockNotControlled,
    CanvasUpload,
    FontLoad,
    ImageLoad,
    InactiveRendering,
    ThrottledRendering,
    IneligibleLogicalTimer,
    ThrottledTask,
    InactiveTask,
    CrossEventLoopDocument,
    Worker,
    Worklet,
    MediaElement,
    GraphicsSource,
    StorageBackend,
    ServiceWorker,
    ExternalSubscription,
    UntrackedCallback,
    ScriptCreatedParserInput,
    SuspendedParser,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedDescription {
    pub reason: UnsupportedReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_surface: Option<TimeSurface>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceState {
    Inert,
    Ready,
    FiniteDeadline {
        #[serde(rename = "deadlineNs")]
        deadline_ns: DecimalU128,
    },
    FiniteRenderingOpportunity,
    AwaitingExternalIo {
        owner: ExternalIoOwner,
        #[serde(rename = "loadBlocking")]
        load_blocking: LoadBlocking,
    },
    OpenEnded {
        #[serde(rename = "openEnded")]
        open_ended: OpenEndedDescription,
    },
    Unsupported {
        unsupported: UnsupportedDescription,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSnapshot {
    pub source_id: OpaqueSourceId,
    pub kind: SourceKind,
    #[serde(flatten)]
    pub state: SourceState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailureComponent {
    Clock,
    TargetTime,
    Scheduler,
    Producer,
    Microtasks,
    InputRevision,
    SourceIdentity,
    LogicalTimer,
    AnimatedImageTimer,
    DomGeneration,
    StateGeneration,
    NavigationRevision,
    PipelineMembershipRevision,
    SourceEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeFailureSummary {
    pub component: RuntimeFailureComponent,
    pub occurrences: DecimalU128,
}

/// Engine-neutral public pending snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingWorkSnapshot {
    pub state_generation: DecimalU128,
    pub dom_epoch: DecimalU128,
    pub virtual_time_ns: DecimalU128,
    pub clock: ClockSnapshot,
    pub input: InputSnapshot,
    pub microtasks: MicrotaskSnapshot,
    pub producers: ProducerSnapshot,
    pub timers: TimerSnapshot,
    pub parser: ParserSnapshot,
    pub network: NetworkSnapshot,
    pub rendering: RenderingSnapshot,
    pub source_epoch: DecimalU128,
    pub sources: Vec<SourceSnapshot>,
    pub runtime_failures: Vec<RuntimeFailureSummary>,
}

impl PendingWorkSnapshot {
    pub fn project(raw: &RawPendingSnapshot, context: &mut WireProjectionContext) -> Self {
        context.observe_pending(raw);
        Self::project_with_source_ids(raw, context)
    }

    fn project_with_source_ids(
        raw: &RawPendingSnapshot,
        context: &mut WireProjectionContext,
    ) -> Self {
        let sources: Vec<_> = raw
            .sources
            .sources()
            .iter()
            .copied()
            .map(|source| project_source(source, context))
            .collect();
        let timer_count = |predicate: fn(&SourceState) -> bool| {
            sources
                .iter()
                .filter(|source| source.kind == SourceKind::Timer && predicate(&source.state))
                .count() as u128
        };

        Self {
            state_generation: raw.state_generation.get().into(),
            dom_epoch: raw.dom_epoch.get().into(),
            virtual_time_ns: raw.clock.now.as_nanos().into(),
            clock: ClockSnapshot {
                mode: match raw.clock.mode {
                    PendingClockMode::Realtime => ClockMode::Real,
                    PendingClockMode::Controlled => ClockMode::Controlled,
                },
                unsupported_surfaces: project_unsupported_time_surfaces(raw),
            },
            input: InputSnapshot {
                ready_events: raw.input.ready_events.into(),
                intake_saturated: raw.input.intake_saturated,
                tasks: TaskSnapshot {
                    ready: raw.input.tasks.ready.into(),
                    throttled: raw.input.tasks.throttled.into(),
                    inactive: raw.input.tasks.inactive.into(),
                },
            },
            microtasks: MicrotaskSnapshot {
                queued: raw.microtasks.queued.into(),
                checkpoint_in_progress: raw.microtasks.checkpoint_in_progress,
                terminal: raw.microtasks.terminal.is_some(),
            },
            producers: ProducerSnapshot {
                pending: raw.producers.snapshot.pending().into(),
                stability: project_producer_stability(raw.producers.stability),
                terminal: raw.producers.snapshot.terminal_error().is_some(),
            },
            timers: TimerSnapshot {
                ready: timer_count(|state| matches!(state, SourceState::Ready)).into(),
                future_finite: timer_count(|state| {
                    matches!(state, SourceState::FiniteDeadline { .. })
                })
                .into(),
                persistent: timer_count(|state| matches!(state, SourceState::OpenEnded { .. }))
                    .into(),
                unsupported: timer_count(|state| matches!(state, SourceState::Unsupported { .. }))
                    .into(),
                next_deadline_ns: project_next_logical_timer_deadline(&raw.logical_timers),
            },
            parser: project_parser(raw),
            network: project_network(raw, context),
            rendering: project_rendering(raw),
            source_epoch: raw.sources.epoch().get().into(),
            sources,
            runtime_failures: project_runtime_failures(raw),
        }
    }

    pub fn virtual_time_ns(&self) -> &DecimalU128 {
        &self.virtual_time_ns
    }
}

fn project_next_logical_timer_deadline(
    timers: &PendingLogicalTimerSnapshot,
) -> Option<DecimalU128> {
    timers
        .timers()
        .iter()
        .filter_map(|timer| timer.outer_wake)
        .min_by_key(|wake| (wake.deadline, wake.id.sequence()))
        .map(|wake| wake.deadline.as_nanos().into())
}

/// `runtime.pending` returns the projected snapshot directly, without an internal wrapper.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RuntimePendingResult(pub PendingWorkSnapshot);

impl RuntimePendingResult {
    pub fn project(raw: &RawPendingSnapshot, context: &mut WireProjectionContext) -> Self {
        Self(PendingWorkSnapshot::project(raw, context))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleOutcome {
    Quiescent,
    QuiescentWithPersistentWork,
    BlockedOnExternalIo,
    BlockedOnOpenEndedWork,
    UnsupportedWork,
    VirtualTimeLimitExceeded,
    ControlTurnLimitExceeded,
    RuntimeError,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedWorkSnapshot {
    pub control_turns: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistentWork {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<OpaqueSourceId>,
    pub kind: SourceKind,
    pub count: DecimalU128,
    #[serde(flatten)]
    pub description: PersistentDescription,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedWork {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<OpaqueSourceId>,
    pub kind: SourceKind,
    pub count: DecimalU128,
    #[serde(flatten)]
    pub description: UnsupportedDescription,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveSettlePolicy {
    pub persistent_work: PersistentWorkPolicy,
    pub max_virtual_time_ns: DecimalU128,
    pub max_control_turns: DecimalU128,
    pub wall_io_timeout_ns: DecimalU128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleLimitKind {
    VirtualTime,
    ControlTurns,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleLimitSnapshot {
    pub kind: SettleLimitKind,
    pub limit: DecimalU128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_virtual_time_ns: Option<DecimalU128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_virtual_time_ns: Option<DecimalU128>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleFailureCode {
    RuntimeTerminals,
    WebViewIdentityChanged,
    ClockNotControlled,
    UnsupportedClockSurface,
    ClockIdentityChanged,
    VirtualTimeRegressed,
    UnsupportedSource,
    UnsupportedOpenEndedSource,
    UnsupportedRendering,
    UnsupportedRetainedTasks,
    IneligibleLogicalTimerHead,
    InconsistentPendingEvidence,
    MissingFiniteSchedulerHead,
    UnclassifiedSchedulerHead,
    MissingAdvanceAuthority,
    MismatchedAdvanceAuthority,
    QuietCheckpointDidNotAdvance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleFailureSnapshot {
    pub code: SettleFailureCode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettleResult {
    pub outcome: SettleOutcome,
    pub virtual_time_ns: DecimalU128,
    pub wall_time_ns: DecimalU128,
    pub state_generation: DecimalU128,
    pub dom_epoch: DecimalU128,
    pub effective_policy: EffectiveSettlePolicy,
    pub processed: ProcessedWorkSnapshot,
    pub snapshot: PendingWorkSnapshot,
    pub persistent_work: Vec<PersistentWork>,
    pub external_io: Vec<ExternalIoSnapshot>,
    pub unsupported_work: Vec<UnsupportedWork>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<SettleLimitSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<SettleFailureSnapshot>,
}

impl RuntimeSettleResult {
    pub fn project(
        completion: SettleCompletion,
        wall_time: Duration,
        policy: ResolvedSettlePolicy,
        context: &mut WireProjectionContext,
    ) -> Self {
        let mut persistent_work = Vec::new();
        let mut external_io = Vec::new();
        let mut unsupported_work = Vec::new();
        let mut limit = None;
        let mut failure = None;
        let (mut outcome, pending, control_turns) = match completion {
            SettleCompletion::Quiescent {
                pending,
                control_turns,
            } => (SettleOutcome::Quiescent, pending, control_turns),
            SettleCompletion::QuiescentWithPersistentWork {
                pending,
                persistent,
                control_turns,
            } => {
                context.observe_pending(&pending);
                persistent_work = project_settle_persistent_work(persistent, context);
                (
                    SettleOutcome::QuiescentWithPersistentWork,
                    pending,
                    control_turns,
                )
            },
            SettleCompletion::BlockedOnExternalIo {
                pending,
                network,
                control_turns,
            } => {
                context.observe_pending(&pending);
                external_io = network
                    .iter()
                    .map(|operation| project_external_io(operation, context))
                    .collect();
                (SettleOutcome::BlockedOnExternalIo, pending, control_turns)
            },
            SettleCompletion::BlockedOnOpenEndedWork {
                pending,
                persistent,
                control_turns,
            } => {
                context.observe_pending(&pending);
                persistent_work = project_settle_persistent_work(persistent, context);
                (
                    SettleOutcome::BlockedOnOpenEndedWork,
                    pending,
                    control_turns,
                )
            },
            SettleCompletion::VirtualTimeLimitExceeded {
                pending,
                start_virtual_time_ns,
                requested_virtual_time_ns,
                limit: virtual_limit,
                control_turns,
            } => {
                limit = Some(SettleLimitSnapshot {
                    kind: SettleLimitKind::VirtualTime,
                    limit: virtual_limit.as_nanos().into(),
                    start_virtual_time_ns: Some(start_virtual_time_ns.into()),
                    requested_virtual_time_ns: Some(requested_virtual_time_ns.into()),
                });
                (
                    SettleOutcome::VirtualTimeLimitExceeded,
                    pending,
                    control_turns,
                )
            },
            SettleCompletion::ControlTurnLimitExceeded {
                pending,
                limit: control_limit,
                control_turns,
            } => {
                limit = Some(SettleLimitSnapshot {
                    kind: SettleLimitKind::ControlTurns,
                    limit: control_limit.into(),
                    start_virtual_time_ns: None,
                    requested_virtual_time_ns: None,
                });
                (
                    SettleOutcome::ControlTurnLimitExceeded,
                    pending,
                    control_turns,
                )
            },
            SettleCompletion::RuntimeError {
                pending,
                failure: runtime_failure,
                control_turns,
            } => {
                let projected = project_settle_failure(&runtime_failure, &pending, context);
                unsupported_work = projected.unsupported_work;
                failure = Some(SettleFailureSnapshot {
                    code: projected.code,
                });
                (projected.outcome, pending, control_turns)
            },
        };
        if policy.persistent_work == PersistentWorkPolicy::Strict &&
            outcome == SettleOutcome::QuiescentWithPersistentWork
        {
            outcome = SettleOutcome::BlockedOnOpenEndedWork;
        }
        let snapshot = PendingWorkSnapshot::project(&pending, context);
        Self {
            outcome,
            virtual_time_ns: snapshot.virtual_time_ns().clone(),
            wall_time_ns: wall_time.as_nanos().into(),
            state_generation: snapshot.state_generation.clone(),
            dom_epoch: snapshot.dom_epoch.clone(),
            effective_policy: EffectiveSettlePolicy {
                persistent_work: policy.persistent_work,
                max_virtual_time_ns: policy.engine.max_virtual_time.as_nanos().into(),
                max_control_turns: policy.engine.max_control_turns.into(),
                wall_io_timeout_ns: policy.engine.wall_io_timeout.as_nanos().into(),
            },
            processed: ProcessedWorkSnapshot {
                control_turns: control_turns.into(),
            },
            snapshot,
            persistent_work,
            external_io,
            unsupported_work,
            limit,
            failure,
        }
    }
}

/// Engine result selected by the public `runtime.advance_to_next` operation.
///
/// Deliberately absent: the single-use advance token consumed inside Servo.
pub enum RuntimeAdvanceToNextFacts<'a> {
    Advanced {
        from_virtual_time_ns: u128,
        final_snapshot: &'a RawPendingSnapshot,
    },
    NoFiniteDeadline {
        final_snapshot: &'a RawPendingSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvanceToNextOutcome {
    Advanced,
    NoFiniteDeadline,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAdvanceToNextResult {
    pub outcome: AdvanceToNextOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_virtual_time_ns: Option<DecimalU128>,
    pub virtual_time_ns: DecimalU128,
    pub state_generation: DecimalU128,
    pub snapshot: PendingWorkSnapshot,
}

impl RuntimeAdvanceToNextResult {
    pub fn project(
        facts: RuntimeAdvanceToNextFacts<'_>,
        context: &mut WireProjectionContext,
    ) -> Self {
        let (outcome, from_virtual_time_ns, raw) = match facts {
            RuntimeAdvanceToNextFacts::Advanced {
                from_virtual_time_ns,
                final_snapshot,
            } => (
                AdvanceToNextOutcome::Advanced,
                Some(from_virtual_time_ns.into()),
                final_snapshot,
            ),
            RuntimeAdvanceToNextFacts::NoFiniteDeadline { final_snapshot } => {
                (AdvanceToNextOutcome::NoFiniteDeadline, None, final_snapshot)
            },
        };
        let snapshot = PendingWorkSnapshot::project(raw, context);
        Self {
            outcome,
            from_virtual_time_ns,
            virtual_time_ns: snapshot.virtual_time_ns().clone(),
            state_generation: snapshot.state_generation.clone(),
            snapshot,
        }
    }
}

fn project_source(
    source: embedder_traits::document_pending::PendingSourceObservation,
    context: &mut WireProjectionContext,
) -> SourceSnapshot {
    SourceSnapshot {
        source_id: context.source_id(source.id.get()),
        kind: project_source_kind(source.kind),
        state: match source.disposition {
            PendingSourceDisposition::Inert => SourceState::Inert,
            PendingSourceDisposition::Ready => SourceState::Ready,
            PendingSourceDisposition::FiniteDeadline(deadline) => SourceState::FiniteDeadline {
                deadline_ns: deadline.as_nanos().into(),
            },
            PendingSourceDisposition::FiniteRenderingOpportunity => {
                SourceState::FiniteRenderingOpportunity
            },
            PendingSourceDisposition::AwaitingExternalIo(evidence) => {
                SourceState::AwaitingExternalIo {
                    owner: project_external_owner(evidence.owner),
                    load_blocking: project_load_blocking(evidence.load_blocking),
                }
            },
            PendingSourceDisposition::OpenEnded(reason) => SourceState::OpenEnded {
                open_ended: project_open_ended_reason(reason),
            },
            PendingSourceDisposition::Unsupported(reason) => SourceState::Unsupported {
                unsupported: project_unsupported_reason(reason),
            },
        },
    }
}

fn project_source_kind(kind: PendingSourceKind) -> SourceKind {
    match kind {
        PendingSourceKind::Task => SourceKind::Task,
        PendingSourceKind::Microtask => SourceKind::Microtask,
        PendingSourceKind::Timer => SourceKind::Timer,
        PendingSourceKind::AnimationFrame => SourceKind::AnimationFrame,
        PendingSourceKind::Animation => SourceKind::Animation,
        PendingSourceKind::Network => SourceKind::Network,
        PendingSourceKind::Parser => SourceKind::Parser,
        PendingSourceKind::RenderingUpdate => SourceKind::RenderingUpdate,
        PendingSourceKind::TrackedPresence => SourceKind::TrackedPresence,
        PendingSourceKind::Other => SourceKind::Other,
    }
}

fn project_open_ended_reason(reason: PendingOpenEndedSourceReason) -> OpenEndedDescription {
    let (reason, requested_period_ns) = match reason {
        PendingOpenEndedSourceReason::Interval { requested_period } => (
            OpenEndedReason::Interval,
            Some(requested_period.as_nanos().into()),
        ),
        PendingOpenEndedSourceReason::InfiniteAnimation => {
            (OpenEndedReason::InfiniteAnimation, None)
        },
        PendingOpenEndedSourceReason::WebSocket => (OpenEndedReason::WebSocket, None),
        PendingOpenEndedSourceReason::EventSource => (OpenEndedReason::EventSource, None),
        PendingOpenEndedSourceReason::BroadcastChannel => (OpenEndedReason::BroadcastChannel, None),
        PendingOpenEndedSourceReason::MessagePort => (OpenEndedReason::MessagePort, None),
        PendingOpenEndedSourceReason::EmbedderControl => (OpenEndedReason::EmbedderControl, None),
        PendingOpenEndedSourceReason::MediaSessionActionHandler => {
            (OpenEndedReason::MediaSessionActionHandler, None)
        },
        PendingOpenEndedSourceReason::StorageEventListener => {
            (OpenEndedReason::StorageEventListener, None)
        },
    };
    OpenEndedDescription {
        reason,
        requested_period_ns,
    }
}

fn project_unsupported_reason(reason: PendingUnsupportedSourceReason) -> UnsupportedDescription {
    let (reason, time_surface) = match reason {
        PendingUnsupportedSourceReason::TimeSurface(surface) => (
            UnsupportedReason::TimeSurface,
            Some(project_time_surface(surface)),
        ),
        PendingUnsupportedSourceReason::UnclassifiedTimer => {
            (UnsupportedReason::UnclassifiedTimer, None)
        },
        PendingUnsupportedSourceReason::CrossEventLoopDocument => {
            (UnsupportedReason::CrossEventLoopDocument, None)
        },
        PendingUnsupportedSourceReason::Worker => (UnsupportedReason::Worker, None),
        PendingUnsupportedSourceReason::Worklet => (UnsupportedReason::Worklet, None),
        PendingUnsupportedSourceReason::MediaElement => (UnsupportedReason::MediaElement, None),
        PendingUnsupportedSourceReason::GraphicsSource => (UnsupportedReason::GraphicsSource, None),
        PendingUnsupportedSourceReason::StorageBackend => (UnsupportedReason::StorageBackend, None),
        PendingUnsupportedSourceReason::ServiceWorker => (UnsupportedReason::ServiceWorker, None),
        PendingUnsupportedSourceReason::ExternalSubscription => {
            (UnsupportedReason::ExternalSubscription, None)
        },
        PendingUnsupportedSourceReason::UntrackedCallback => {
            (UnsupportedReason::UntrackedCallback, None)
        },
        PendingUnsupportedSourceReason::ScriptCreatedParserInput => {
            (UnsupportedReason::ScriptCreatedParserInput, None)
        },
        PendingUnsupportedSourceReason::SuspendedParser => {
            (UnsupportedReason::SuspendedParser, None)
        },
    };
    UnsupportedDescription {
        reason,
        time_surface,
    }
}

fn project_time_surface(surface: DocumentTimeSurface) -> TimeSurface {
    match surface {
        DocumentTimeSurface::WindowTimers => TimeSurface::WindowTimers,
        DocumentTimeSurface::SameEventLoopIframe => TimeSurface::SameEventLoopIframe,
        DocumentTimeSurface::JavaScriptDate => TimeSurface::JavaScriptDate,
        DocumentTimeSurface::Performance => TimeSurface::Performance,
        DocumentTimeSurface::HostTimestamp => TimeSurface::HostTimestamp,
        DocumentTimeSurface::UpdateRendering => TimeSurface::UpdateRendering,
        DocumentTimeSurface::AnimationFrame => TimeSurface::AnimationFrame,
        DocumentTimeSurface::DocumentTimeline => TimeSurface::DocumentTimeline,
        DocumentTimeSurface::Worker => TimeSurface::Worker,
        DocumentTimeSurface::Worklet => TimeSurface::Worklet,
        DocumentTimeSurface::CrossEventLoopIframe => TimeSurface::CrossEventLoopIframe,
        DocumentTimeSurface::CrossEventLoopNavigation => TimeSurface::CrossEventLoopNavigation,
        DocumentTimeSurface::AuxiliaryWebView => TimeSurface::AuxiliaryWebView,
        DocumentTimeSurface::ResourceThreadIo => TimeSurface::ResourceThreadIo,
        DocumentTimeSurface::ExternalSubscription => TimeSurface::ExternalSubscription,
        DocumentTimeSurface::NativeMedia => TimeSurface::NativeMedia,
        DocumentTimeSurface::EmbedderControl => TimeSurface::EmbedderControl,
    }
}

fn project_unsupported_time_surfaces(raw: &RawPendingSnapshot) -> Vec<TimeSurface> {
    let mut surfaces = Vec::new();
    for surface in [
        raw.clock.unsupported_surface,
        raw.target.unsupported_time_surface,
    ]
    .into_iter()
    .flatten()
    .map(project_time_surface)
    {
        if !surfaces.contains(&surface) {
            surfaces.push(surface);
        }
    }
    surfaces
}

fn project_producer_stability(stability: PendingProducerStability) -> ProducerStability {
    match stability {
        PendingProducerStability::NotCheckpointed => ProducerStability::NotCheckpointed,
        PendingProducerStability::Busy => ProducerStability::Busy,
        PendingProducerStability::FirstEmpty => ProducerStability::FirstEmpty,
        PendingProducerStability::StableEmpty => ProducerStability::StableEmpty,
        PendingProducerStability::Unqualified => ProducerStability::Unqualified,
    }
}

fn project_parser(raw: &RawPendingSnapshot) -> ParserSnapshot {
    let mut ready = 0_u128;
    let mut awaiting_external_io = 0_u128;
    let mut awaiting_commit = 0_u128;
    let mut awaiting_script_input = 0_u128;
    let mut suspended = 0_u128;
    for source in raw.parser.sources() {
        match source.phase {
            PendingParserPhase::Ready => ready += 1,
            PendingParserPhase::AwaitingExternalInput => awaiting_external_io += 1,
            PendingParserPhase::AwaitingCommit => awaiting_commit += 1,
            PendingParserPhase::AwaitingScriptInput => awaiting_script_input += 1,
            PendingParserPhase::Suspended => suspended += 1,
        }
    }
    ParserSnapshot {
        total: (raw.parser.sources().len() as u128).into(),
        ready: ready.into(),
        awaiting_external_io: awaiting_external_io.into(),
        awaiting_commit: awaiting_commit.into(),
        awaiting_script_input: awaiting_script_input.into(),
        suspended: suspended.into(),
    }
}

fn project_network(
    raw: &RawPendingSnapshot,
    context: &mut WireProjectionContext,
) -> NetworkSnapshot {
    let mut counts = [0_u128; 9];
    let active = raw
        .network
        .active()
        .iter()
        .map(|operation| {
            let (_, index) = project_network_kind(operation.kind);
            counts[index] += 1;
            project_external_io(operation, context)
        })
        .collect();
    NetworkSnapshot {
        counts: NetworkCounts {
            navigation: counts[0].into(),
            fetch: counts[1].into(),
            xml_http_request: counts[2].into(),
            image: counts[3].into(),
            font: counts[4].into(),
            stylesheet: counts[5].into(),
            script: counts[6].into(),
            unclassified_producer_io: counts[7].into(),
            other: counts[8].into(),
        },
        active,
    }
}

fn project_external_io(
    operation: &PendingExternalIoObservation,
    context: &mut WireProjectionContext,
) -> ExternalIoSnapshot {
    ExternalIoSnapshot {
        source_id: context.source_id(operation.source_id.get()),
        kind: project_network_kind(operation.kind).0,
        phase: project_external_phase(operation.phase),
        owner: project_external_owner(operation.evidence.owner),
        load_blocking: project_load_blocking(operation.evidence.load_blocking),
        started_at_ns: operation.started_at.as_nanos().into(),
    }
}

fn project_network_kind(kind: PendingNetworkKind) -> (NetworkKind, usize) {
    match kind {
        PendingNetworkKind::Navigation => (NetworkKind::Navigation, 0),
        PendingNetworkKind::Fetch => (NetworkKind::Fetch, 1),
        PendingNetworkKind::XmlHttpRequest => (NetworkKind::XmlHttpRequest, 2),
        PendingNetworkKind::Image => (NetworkKind::Image, 3),
        PendingNetworkKind::Font => (NetworkKind::Font, 4),
        PendingNetworkKind::Stylesheet => (NetworkKind::Stylesheet, 5),
        PendingNetworkKind::Script => (NetworkKind::Script, 6),
        PendingNetworkKind::ProducerFallback => (NetworkKind::UnclassifiedProducerIo, 7),
        PendingNetworkKind::Other => (NetworkKind::Other, 8),
    }
}

fn project_external_phase(phase: PendingExternalIoPhase) -> ExternalIoPhase {
    match phase {
        PendingExternalIoPhase::Queued => ExternalIoPhase::Queued,
        PendingExternalIoPhase::AwaitingResponse => ExternalIoPhase::AwaitingResponse,
        PendingExternalIoPhase::StreamingBody => ExternalIoPhase::StreamingBody,
        PendingExternalIoPhase::TerminalTaskQueued => ExternalIoPhase::TerminalTaskQueued,
    }
}

fn project_external_owner(owner: PendingExternalIoOwner) -> ExternalIoOwner {
    match owner {
        PendingExternalIoOwner::TopLevelNavigation => ExternalIoOwner::TopLevelNavigation,
        PendingExternalIoOwner::DocumentParser => ExternalIoOwner::DocumentParser,
        PendingExternalIoOwner::Script => ExternalIoOwner::Script,
        PendingExternalIoOwner::DocumentSubresource => ExternalIoOwner::DocumentSubresource,
        PendingExternalIoOwner::RenderingResource => ExternalIoOwner::RenderingResource,
        PendingExternalIoOwner::Other => ExternalIoOwner::Other,
    }
}

fn project_load_blocking(value: PendingExternalIoLoadBlocking) -> LoadBlocking {
    match value {
        PendingExternalIoLoadBlocking::Blocking => LoadBlocking::Blocking,
        PendingExternalIoLoadBlocking::NonBlocking => LoadBlocking::NonBlocking,
        PendingExternalIoLoadBlocking::Unknown => LoadBlocking::Unknown,
    }
}

fn project_rendering(raw: &RawPendingSnapshot) -> RenderingSnapshot {
    let mut retained_animation_frames = 0_u128;
    let mut runnable_animation_frames = 0_u128;
    let mut update_required = false;
    let mut pending_animation_events = 0_u128;
    let mut finite_animations = 0_u128;
    let mut persistent_animations = 0_u128;
    let mut unsupported_animations = 0_u128;
    let mut finite_animated_images = 0_u128;
    let mut persistent_animated_images = 0_u128;
    let mut unsupported_animated_images = 0_u128;
    let mut image_update_ready = false;
    let mut dirty_canvases = 0_u128;
    let mut canvas_upload_pending = false;
    let mut unsupported_canvases = 0_u128;
    let mut pending_fonts = 0_u128;
    let mut pending_images = 0_u128;
    for pipeline in raw.rendering.pipelines() {
        retained_animation_frames += u128::from(pipeline.retained_animation_frame_callbacks);
        runnable_animation_frames += u128::from(pipeline.runnable_animation_frame_callbacks);
        update_required |= pipeline.document_update_required;
        pending_animation_events += u128::from(pipeline.pending_animation_events);
        finite_animations += u128::from(pipeline.finite_animations);
        persistent_animations += u128::from(pipeline.infinite_animations);
        unsupported_animations += u128::from(pipeline.unsupported_animations);
        finite_animated_images += u128::from(pipeline.animated_images.finite_images);
        persistent_animated_images += u128::from(pipeline.animated_images.infinite_images);
        unsupported_animated_images +=
            u128::from(pipeline.animated_images.unsupported.loop_count_unavailable) +
                u128::from(pipeline.animated_images.unsupported.timeline_uncontrolled) +
                u128::from(
                    pipeline
                        .animated_images
                        .unsupported
                        .timer_binding_unavailable,
                );
        image_update_ready |= pipeline.animated_images.update_ready;
        dirty_canvases += u128::from(pipeline.canvas.dirty_contexts);
        canvas_upload_pending |= pipeline.canvas.awaiting_async_upload;
        unsupported_canvases += u128::from(
            pipeline
                .canvas
                .unsupported
                .live_source_inventory_unavailable,
        ) + u128::from(pipeline.canvas.unsupported.offscreen_execution) +
            u128::from(pipeline.canvas.unsupported.mutation_generation_unbound);
        pending_fonts += u128::from(pipeline.pending_fonts);
        pending_images += u128::from(pipeline.pending_images);
    }
    RenderingSnapshot {
        opportunity_ready: raw.rendering.opportunity_ready,
        next_opportunity_ns: raw
            .rendering
            .scheduled_opportunity
            .map(|deadline| deadline.deadline.as_nanos().into()),
        retained_animation_frames: retained_animation_frames.into(),
        runnable_animation_frames: runnable_animation_frames.into(),
        update_required,
        pending_animation_events: pending_animation_events.into(),
        finite_animations: finite_animations.into(),
        persistent_animations: persistent_animations.into(),
        unsupported_animations: unsupported_animations.into(),
        finite_animated_images: finite_animated_images.into(),
        persistent_animated_images: persistent_animated_images.into(),
        unsupported_animated_images: unsupported_animated_images.into(),
        image_update_ready,
        dirty_canvases: dirty_canvases.into(),
        canvas_upload_pending,
        unsupported_canvases: unsupported_canvases.into(),
        pending_fonts: pending_fonts.into(),
        pending_images: pending_images.into(),
    }
}

fn project_runtime_failures(raw: &RawPendingSnapshot) -> Vec<RuntimeFailureSummary> {
    let terminals = &raw.terminals;
    let mut failures = Vec::new();
    let mut push = |present: bool, component, occurrences: u128| {
        if present {
            failures.push(RuntimeFailureSummary {
                component,
                occurrences: occurrences.into(),
            });
        }
    };
    push(terminals.clock.is_some(), RuntimeFailureComponent::Clock, 1);
    push(
        terminals.target_time.is_some(),
        RuntimeFailureComponent::TargetTime,
        1,
    );
    push(
        terminals.outer_scheduler.is_some(),
        RuntimeFailureComponent::Scheduler,
        1,
    );
    push(
        terminals.producer.is_some(),
        RuntimeFailureComponent::Producer,
        1,
    );
    push(
        terminals.microtask.is_some(),
        RuntimeFailureComponent::Microtasks,
        1,
    );
    push(
        terminals.input_revision.is_some(),
        RuntimeFailureComponent::InputRevision,
        1,
    );
    push(
        terminals.source_id.is_some(),
        RuntimeFailureComponent::SourceIdentity,
        1,
    );
    push(
        !terminals.logical_timers().is_empty(),
        RuntimeFailureComponent::LogicalTimer,
        terminals.logical_timers().len() as u128,
    );
    push(
        !terminals.image_timers().is_empty(),
        RuntimeFailureComponent::AnimatedImageTimer,
        terminals.image_timers().len() as u128,
    );
    push(
        terminals.dom_generation.is_some(),
        RuntimeFailureComponent::DomGeneration,
        1,
    );
    push(
        terminals.state_generation.is_some(),
        RuntimeFailureComponent::StateGeneration,
        1,
    );
    push(
        terminals.navigation_revision.is_some(),
        RuntimeFailureComponent::NavigationRevision,
        1,
    );
    push(
        terminals.pipeline_membership_revision.is_some(),
        RuntimeFailureComponent::PipelineMembershipRevision,
        1,
    );
    push(
        terminals.source_epoch.is_some(),
        RuntimeFailureComponent::SourceEpoch,
        1,
    );
    failures
}

fn project_settle_persistent_work(
    work: Vec<SettlePersistentWork>,
    context: &mut WireProjectionContext,
) -> Vec<PersistentWork> {
    let mut projected = Vec::new();
    let mut infinite_animations = 0_u128;
    let mut infinite_animated_images = 0_u128;
    for work in work {
        match work {
            SettlePersistentWork::Source(source) => {
                if let Some(source) = project_persistent_source(source, context) {
                    projected.push(source);
                }
            },
            SettlePersistentWork::InfiniteRendering(rendering) => {
                infinite_animations += u128::from(rendering.infinite_animations);
                infinite_animated_images += u128::from(rendering.animated_images.infinite_images);
            },
        }
    }
    if infinite_animations != 0 {
        projected.push(PersistentWork {
            source_id: None,
            kind: SourceKind::Animation,
            count: infinite_animations.into(),
            description: PersistentDescription {
                reason: PersistentReason::InfiniteAnimation,
                requested_period_ns: None,
            },
        });
    }
    if infinite_animated_images != 0 {
        projected.push(PersistentWork {
            source_id: None,
            kind: SourceKind::TrackedPresence,
            count: infinite_animated_images.into(),
            description: PersistentDescription {
                reason: PersistentReason::InfiniteAnimatedImage,
                requested_period_ns: None,
            },
        });
    }
    projected
}

fn project_persistent_source(
    source: PendingSourceObservation,
    context: &mut WireProjectionContext,
) -> Option<PersistentWork> {
    let description = match source.disposition {
        PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
            requested_period,
        }) => PersistentDescription {
            reason: PersistentReason::Interval,
            requested_period_ns: Some(requested_period.as_nanos().into()),
        },
        PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::InfiniteAnimation) => {
            PersistentDescription {
                reason: PersistentReason::InfiniteAnimation,
                requested_period_ns: None,
            }
        },
        _ => return None,
    };
    Some(PersistentWork {
        source_id: Some(context.source_id(source.id.get())),
        kind: project_source_kind(source.kind),
        count: 1_u64.into(),
        description,
    })
}

struct ProjectedSettleFailure {
    outcome: SettleOutcome,
    code: SettleFailureCode,
    unsupported_work: Vec<UnsupportedWork>,
}

fn project_settle_failure(
    failure: &SettleRuntimeFailure,
    pending: &RawPendingSnapshot,
    context: &mut WireProjectionContext,
) -> ProjectedSettleFailure {
    context.observe_pending(pending);
    let (outcome, code, unsupported_work) = match failure {
        SettleRuntimeFailure::RuntimeTerminals(_) => {
            let unsupported_work = project_unsupported_time_surfaces(pending)
                .into_iter()
                .map(|surface| {
                    unsupported_aggregate(
                        SourceKind::Other,
                        1,
                        UnsupportedReason::TimeSurface,
                        Some(surface),
                    )
                })
                .collect();
            (
                SettleOutcome::RuntimeError,
                SettleFailureCode::RuntimeTerminals,
                unsupported_work,
            )
        },
        SettleRuntimeFailure::WebViewIdentityChanged => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::WebViewIdentityChanged,
            vec![],
        ),
        SettleRuntimeFailure::ClockNotControlled(_) => (
            SettleOutcome::UnsupportedWork,
            SettleFailureCode::ClockNotControlled,
            vec![unsupported_aggregate(
                SourceKind::Other,
                1,
                UnsupportedReason::ClockNotControlled,
                None,
            )],
        ),
        SettleRuntimeFailure::UnsupportedClockSurface => {
            let mut work: Vec<_> = project_unsupported_time_surfaces(pending)
                .into_iter()
                .map(|surface| {
                    unsupported_aggregate(
                        SourceKind::Other,
                        1,
                        UnsupportedReason::TimeSurface,
                        Some(surface),
                    )
                })
                .collect();
            if work.is_empty() {
                work.push(unsupported_aggregate(
                    SourceKind::Other,
                    1,
                    UnsupportedReason::TimeSurface,
                    None,
                ));
            }
            (
                SettleOutcome::UnsupportedWork,
                SettleFailureCode::UnsupportedClockSurface,
                work,
            )
        },
        SettleRuntimeFailure::ClockIdentityChanged => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::ClockIdentityChanged,
            vec![],
        ),
        SettleRuntimeFailure::VirtualTimeRegressed => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::VirtualTimeRegressed,
            vec![],
        ),
        SettleRuntimeFailure::UnsupportedSource(source) => (
            SettleOutcome::UnsupportedWork,
            SettleFailureCode::UnsupportedSource,
            project_unsupported_source(*source, context)
                .into_iter()
                .collect(),
        ),
        SettleRuntimeFailure::UnsupportedOpenEndedSource(source) => (
            SettleOutcome::UnsupportedWork,
            SettleFailureCode::UnsupportedOpenEndedSource,
            project_unsupported_open_ended_source(*source, context)
                .into_iter()
                .collect(),
        ),
        SettleRuntimeFailure::UnsupportedRendering(_) => (
            SettleOutcome::UnsupportedWork,
            SettleFailureCode::UnsupportedRendering,
            project_all_unsupported_rendering(pending),
        ),
        SettleRuntimeFailure::UnsupportedRetainedTasks(tasks) => {
            let mut work = Vec::new();
            if tasks.throttled != 0 {
                work.push(unsupported_aggregate(
                    SourceKind::Task,
                    u128::from(tasks.throttled),
                    UnsupportedReason::ThrottledTask,
                    None,
                ));
            }
            if tasks.inactive != 0 {
                work.push(unsupported_aggregate(
                    SourceKind::Task,
                    u128::from(tasks.inactive),
                    UnsupportedReason::InactiveTask,
                    None,
                ));
            }
            (
                SettleOutcome::UnsupportedWork,
                SettleFailureCode::UnsupportedRetainedTasks,
                work,
            )
        },
        SettleRuntimeFailure::IneligibleLogicalTimerHead(timer) => (
            SettleOutcome::UnsupportedWork,
            SettleFailureCode::IneligibleLogicalTimerHead,
            vec![UnsupportedWork {
                source_id: Some(context.source_id(timer.source_id.get())),
                kind: SourceKind::Timer,
                count: 1_u64.into(),
                description: UnsupportedDescription {
                    reason: UnsupportedReason::IneligibleLogicalTimer,
                    time_surface: None,
                },
            }],
        ),
        SettleRuntimeFailure::InconsistentPendingEvidence(_) => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::InconsistentPendingEvidence,
            vec![],
        ),
        SettleRuntimeFailure::MissingFiniteSchedulerHead => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::MissingFiniteSchedulerHead,
            vec![],
        ),
        SettleRuntimeFailure::UnclassifiedSchedulerHead => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::UnclassifiedSchedulerHead,
            vec![],
        ),
        SettleRuntimeFailure::MissingAdvanceAuthority => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::MissingAdvanceAuthority,
            vec![],
        ),
        SettleRuntimeFailure::MismatchedAdvanceAuthority => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::MismatchedAdvanceAuthority,
            vec![],
        ),
        SettleRuntimeFailure::QuietCheckpointDidNotAdvance => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::QuietCheckpointDidNotAdvance,
            vec![],
        ),
    };
    ProjectedSettleFailure {
        outcome,
        code,
        unsupported_work,
    }
}

fn project_unsupported_source(
    source: PendingSourceObservation,
    context: &mut WireProjectionContext,
) -> Option<UnsupportedWork> {
    let PendingSourceDisposition::Unsupported(reason) = source.disposition else {
        return None;
    };
    Some(UnsupportedWork {
        source_id: Some(context.source_id(source.id.get())),
        kind: project_source_kind(source.kind),
        count: 1_u64.into(),
        description: project_unsupported_reason(reason),
    })
}

fn project_unsupported_open_ended_source(
    source: PendingSourceObservation,
    context: &mut WireProjectionContext,
) -> Option<UnsupportedWork> {
    let PendingSourceDisposition::OpenEnded(reason) = source.disposition else {
        return None;
    };
    let reason = match reason {
        PendingOpenEndedSourceReason::WebSocket => UnsupportedReason::WebSocket,
        PendingOpenEndedSourceReason::EventSource => UnsupportedReason::EventSource,
        PendingOpenEndedSourceReason::BroadcastChannel => UnsupportedReason::BroadcastChannel,
        PendingOpenEndedSourceReason::MessagePort => UnsupportedReason::MessagePort,
        PendingOpenEndedSourceReason::EmbedderControl => UnsupportedReason::EmbedderControl,
        PendingOpenEndedSourceReason::MediaSessionActionHandler => {
            UnsupportedReason::MediaSessionActionHandler
        },
        PendingOpenEndedSourceReason::StorageEventListener => {
            UnsupportedReason::StorageEventListener
        },
        PendingOpenEndedSourceReason::Interval { .. } |
        PendingOpenEndedSourceReason::InfiniteAnimation => return None,
    };
    Some(UnsupportedWork {
        source_id: Some(context.source_id(source.id.get())),
        kind: project_source_kind(source.kind),
        count: 1_u64.into(),
        description: UnsupportedDescription {
            reason,
            time_surface: None,
        },
    })
}

fn project_unsupported_rendering(
    rendering: &PendingPipelineRenderingObservation,
) -> Vec<UnsupportedWork> {
    let mut work = Vec::new();
    let mut push = |count: u128, kind, reason| {
        if count != 0 {
            work.push(unsupported_aggregate(kind, count, reason, None));
        }
    };
    push(
        u128::from(rendering.unsupported_animations),
        SourceKind::Animation,
        UnsupportedReason::UnclassifiedAnimation,
    );
    push(
        u128::from(rendering.animated_images.unsupported.loop_count_unavailable) +
            u128::from(rendering.animated_images.unsupported.timeline_uncontrolled) +
            u128::from(
                rendering
                    .animated_images
                    .unsupported
                    .timer_binding_unavailable,
            ),
        SourceKind::TrackedPresence,
        UnsupportedReason::AnimatedImage,
    );
    let unsupported_canvases = u128::from(
        rendering
            .canvas
            .unsupported
            .live_source_inventory_unavailable,
    ) + u128::from(rendering.canvas.unsupported.offscreen_execution) +
        u128::from(rendering.canvas.unsupported.mutation_generation_unbound);
    push(
        unsupported_canvases,
        SourceKind::TrackedPresence,
        UnsupportedReason::GraphicsSource,
    );
    push(
        u128::from(rendering.canvas.awaiting_async_upload),
        SourceKind::TrackedPresence,
        UnsupportedReason::CanvasUpload,
    );
    push(
        u128::from(rendering.pending_fonts),
        SourceKind::TrackedPresence,
        UnsupportedReason::FontLoad,
    );
    push(
        u128::from(rendering.pending_images),
        SourceKind::TrackedPresence,
        UnsupportedReason::ImageLoad,
    );
    let retained_work = rendering.runnable_animation_frame_callbacks != 0 ||
        rendering.document_update_required ||
        rendering.pending_animation_events != 0 ||
        rendering.finite_animations != 0 ||
        rendering.infinite_animations != 0 ||
        rendering.animated_images.finite_images != 0 ||
        rendering.animated_images.infinite_images != 0 ||
        rendering.animated_images.update_ready ||
        rendering.animated_images.scheduled_timer.is_some() ||
        rendering.canvas.dirty_contexts != 0 ||
        rendering.pending_fonts != 0 ||
        rendering.pending_images != 0;
    if retained_work {
        match rendering.activity {
            PendingRenderingPipelineActivity::FullyActive => {},
            PendingRenderingPipelineActivity::Throttled => push(
                1,
                SourceKind::RenderingUpdate,
                UnsupportedReason::ThrottledRendering,
            ),
            PendingRenderingPipelineActivity::Inactive => push(
                1,
                SourceKind::RenderingUpdate,
                UnsupportedReason::InactiveRendering,
            ),
        }
    }
    work
}

fn project_all_unsupported_rendering(pending: &RawPendingSnapshot) -> Vec<UnsupportedWork> {
    let mut projected: Vec<UnsupportedWork> = Vec::new();
    for rendering in pending
        .rendering
        .pipelines()
        .iter()
        .filter(|rendering| rendering_is_unsupported(rendering))
    {
        for candidate in project_unsupported_rendering(rendering) {
            if let Some(existing) = projected.iter_mut().find(|existing| {
                existing.source_id.is_none() &&
                    existing.kind == candidate.kind &&
                    existing.description == candidate.description
            }) {
                existing.count = existing
                    .count
                    .get()
                    .checked_add(candidate.count.get())
                    .expect("in-memory rendering aggregate must fit u128")
                    .into();
            } else {
                projected.push(candidate);
            }
        }
    }
    projected
}

fn rendering_is_unsupported(rendering: &PendingPipelineRenderingObservation) -> bool {
    let unsupported_images = rendering.animated_images.unsupported.checked_total() != Some(0);
    let unsupported_canvas = rendering
        .canvas
        .unsupported
        .live_source_inventory_unavailable !=
        0 ||
        rendering.canvas.unsupported.offscreen_execution != 0 ||
        rendering.canvas.unsupported.mutation_generation_unbound != 0;
    let inactive_work = rendering.activity != PendingRenderingPipelineActivity::FullyActive &&
        (rendering.runnable_animation_frame_callbacks != 0 ||
            rendering.document_update_required ||
            rendering.pending_animation_events != 0 ||
            rendering.finite_animations != 0 ||
            rendering.infinite_animations != 0 ||
            rendering.animated_images.finite_images != 0 ||
            rendering.animated_images.infinite_images != 0 ||
            rendering.animated_images.update_ready ||
            rendering.animated_images.scheduled_timer.is_some() ||
            rendering.canvas.dirty_contexts != 0 ||
            rendering.pending_fonts != 0 ||
            rendering.pending_images != 0);
    rendering.unsupported_animations != 0 ||
        unsupported_images ||
        unsupported_canvas ||
        inactive_work ||
        rendering.canvas.awaiting_async_upload ||
        rendering.pending_fonts != 0 ||
        rendering.pending_images != 0
}

fn unsupported_aggregate(
    kind: SourceKind,
    count: u128,
    reason: UnsupportedReason,
    time_surface: Option<TimeSurface>,
) -> UnsupportedWork {
    UnsupportedWork {
        source_id: None,
        kind,
        count: count.into(),
        description: UnsupportedDescription {
            reason,
            time_surface,
        },
    }
}

#[cfg(test)]
mod tests {
    use embedder_traits::document_automation::{
        DocumentExtractionRow as EngineDocumentExtractionRow,
        DocumentExtractionValue as EngineDocumentExtractionValue,
    };
    use embedder_traits::document_pending::{
        DomEpoch, PendingClockObservation, PendingInputObservation, PendingLogicalTimerKind,
        PendingLogicalTimerObservation, PendingLogicalTimerSnapshot, PendingLogicalTimerStableId,
        PendingMicrotaskCheckpoint, PendingMicrotaskObservation, PendingNavigationRevision,
        PendingNetworkObservation, PendingParserObservation, PendingPipelineMembershipRevision,
        PendingProducerObservation, PendingProducerStability, PendingRenderingObservation,
        PendingRuntimeTerminals, PendingSchedulerObservation, PendingSourceEpoch, PendingSourceId,
        PendingSourceObservation, PendingSourceSnapshot, PendingTargetObservation,
        PendingTaskObservation, RawPendingSnapshot, RuntimeStateGeneration,
    };
    use serde_json::{Value, json};
    use servo_base::id::{
        BrowsingContextId, BrowsingContextIndex, Index, PipelineId, PipelineIndex,
        PipelineNamespaceId, ScriptEventLoopId, WebViewId,
    };
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentProducerCheckpoint,
        DocumentProducerFence, DocumentUnixTime, TimerDeadlineSnapshot, TimerEventRequest,
        TimerScheduler,
    };

    use super::*;

    const ABOVE_JS_SAFE_INTEGER: u64 = 9_007_199_254_740_993;
    const LARGE_VIRTUAL_TIME: u128 = (u64::MAX as u128) + 9_007_199_254_740_993;

    fn pending_fixture() -> RawPendingSnapshot {
        let event_loop_id = ScriptEventLoopId::new();
        let browsing_context_id = BrowsingContextId {
            namespace_id: PipelineNamespaceId(81),
            index: Index::<BrowsingContextIndex>::new(1).unwrap(),
        };
        let target = PendingTargetObservation::new_with_authority(
            WebViewId::mock_for_testing(browsing_context_id),
            event_loop_id,
            None,
            PendingNavigationRevision::new(ABOVE_JS_SAFE_INTEGER),
            PendingPipelineMembershipRevision::new(ABOVE_JS_SAFE_INTEGER + 1),
            None,
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: LARGE_VIRTUAL_TIME,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(1_000_000),
        });
        let scheduler = TimerScheduler::with_clock(clock.clone());
        let producer_fence = DocumentProducerFence::default();
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(1);
        let producer_checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let interval_source = PendingSourceObservation {
            id: PendingSourceId::new(ABOVE_JS_SAFE_INTEGER),
            // The fixture exercises source-state projection without fabricating a live outer
            // scheduler binding for the new authoritative logical-timer inventory.
            kind: PendingSourceKind::Other,
            disposition: PendingSourceDisposition::OpenEnded(
                PendingOpenEndedSourceReason::Interval {
                    requested_period: Duration::from_secs(5),
                },
            ),
        };
        let unsupported_source = PendingSourceObservation {
            id: PendingSourceId::new(ABOVE_JS_SAFE_INTEGER + 1),
            kind: PendingSourceKind::Other,
            disposition: PendingSourceDisposition::Unsupported(
                PendingUnsupportedSourceReason::TimeSurface(DocumentTimeSurface::Worker),
            ),
        };
        let snapshot = RawPendingSnapshot {
            target,
            state_generation: RuntimeStateGeneration::new(ABOVE_JS_SAFE_INTEGER),
            dom_epoch: DomEpoch::new(ABOVE_JS_SAFE_INTEGER + 2),
            clock: PendingClockObservation {
                clock_id: clock.id(),
                mode: PendingClockMode::Controlled,
                now: clock.now(),
                unsupported_surface: None,
            },
            scheduler: PendingSchedulerObservation {
                scheduler_id: scheduler.id(),
                next_deadline: None,
            },
            input: PendingInputObservation::default(),
            microtasks: PendingMicrotaskObservation {
                event_loop_id,
                queued: 0,
                completed_checkpoint: microtask_checkpoint,
                checkpoint_in_progress: false,
                terminal: None,
            },
            producers: PendingProducerObservation::new(
                event_loop_id,
                microtask_checkpoint,
                producer_checkpoint,
                producer_fence.snapshot(),
                PendingProducerStability::FirstEmpty,
                None,
            )
            .unwrap(),
            logical_timers: PendingLogicalTimerSnapshot::default(),
            parser: PendingParserObservation::default(),
            network: PendingNetworkObservation::default(),
            rendering: PendingRenderingObservation::default(),
            sources: PendingSourceSnapshot::new(
                PendingSourceEpoch::new(ABOVE_JS_SAFE_INTEGER + 3),
                vec![unsupported_source, interval_source],
            )
            .unwrap(),
            terminals: PendingRuntimeTerminals::default(),
        };
        snapshot.validate().unwrap();
        snapshot
    }

    #[test]
    fn automation_params_are_strict_bounded_and_generation_checked() {
        let maximum_generation = u64::MAX.to_string();
        let activate: ActionActivateParams = serde_json::from_value(json!({
            "selector": "#start",
            "expectedGeneration": maximum_generation,
        }))
        .unwrap();
        let activate = activate.resolve().unwrap();
        assert_eq!(activate.kind(), PublicAutomationKind::Activate);
        assert_eq!(activate.expected_generation().get(), u64::MAX);
        assert!(matches!(
            activate.operation,
            DocumentAutomationOperation::Activate { ref selector } if selector == "#start"
        ));

        let out_of_range: ActionActivateParams = serde_json::from_value(json!({
            "selector": "#start",
            "expectedGeneration": (u128::from(u64::MAX) + 1).to_string(),
        }))
        .unwrap();
        assert_eq!(
            out_of_range.resolve(),
            Err(AutomationParamsError::ExpectedGenerationOutOfRange)
        );
        for invalid in [
            json!({"selector": "#start", "expectedGeneration": u64::MAX}),
            json!({"selector": "#start", "expectedGeneration": "01"}),
            json!({"selector": "#start", "expectedGeneration": null}),
            json!({"selector": "#start", "expectedGeneration": "1", "extra": true}),
            json!([]),
        ] {
            assert!(serde_json::from_value::<ActionActivateParams>(invalid).is_err());
        }

        let oversized_selector = "x".repeat(
            usize::try_from(DocumentAutomationLimits::MVP.max_selector_bytes()).unwrap() + 1,
        );
        let oversized: DomTextParams = serde_json::from_value(json!({
            "selector": oversized_selector,
            "expectedGeneration": "1",
        }))
        .unwrap();
        assert!(matches!(
            oversized.resolve(),
            Err(AutomationParamsError::InvalidOperation(
                DocumentAutomationRequestError::SelectorTooLong { .. }
            ))
        ));
    }

    #[test]
    fn extraction_params_preserve_order_and_reject_unbounded_plans() {
        let params: DomExtractParams = serde_json::from_value(json!({
            "rootSelector": ".row",
            "fields": [
                {"name": "title", "selector": ".title", "read": "text"},
                {"name": "markup", "selector": ".body", "read": "html"}
            ],
            "expectedGeneration": "9",
        }))
        .unwrap();
        let resolved = params.resolve().unwrap();
        assert_eq!(resolved.kind(), PublicAutomationKind::Extract);
        let DocumentAutomationOperation::Extract(plan) = resolved.operation else {
            panic!("expected a native extraction operation");
        };
        assert_eq!(plan.root_selector(), ".row");
        assert_eq!(plan.fields()[0].name(), "title");
        assert_eq!(plan.fields()[0].read(), DocumentExtractionRead::TextContent);
        assert_eq!(plan.fields()[1].name(), "markup");
        assert_eq!(plan.fields()[1].read(), DocumentExtractionRead::InnerHtml);

        for invalid in [
            json!({
                "rootSelector": ".row",
                "fields": [],
                "expectedGeneration": "1",
            }),
            json!({
                "rootSelector": ".row",
                "fields": [
                    {"name": "same", "selector": ".a", "read": "text"},
                    {"name": "same", "selector": ".b", "read": "text"}
                ],
                "expectedGeneration": "1",
            }),
        ] {
            let params: DomExtractParams = serde_json::from_value(invalid).unwrap();
            assert!(matches!(
                params.resolve(),
                Err(AutomationParamsError::InvalidOperation(_))
            ));
        }
        assert!(
            serde_json::from_value::<DomExtractParams>(json!({
                "rootSelector": ".row",
                "fields": [{"name": "x", "selector": ".x", "read": "inner_html"}],
                "expectedGeneration": "1",
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<DomExtractParams>(json!({
                "rootSelector": ".row",
                "fields": [{
                    "name": "x",
                    "selector": ".x",
                    "read": "text",
                    "unexpected": true
                }],
                "expectedGeneration": "1",
            }))
            .is_err()
        );

        let too_many_fields: Vec<_> = (0..=DocumentAutomationLimits::MVP.max_extraction_fields())
            .map(|index| {
                json!({
                    "name": format!("field-{index}"),
                    "selector": ".value",
                    "read": "text",
                })
            })
            .collect();
        let too_many: DomExtractParams = serde_json::from_value(json!({
            "rootSelector": ".row",
            "fields": too_many_fields,
            "expectedGeneration": "1",
        }))
        .unwrap();
        assert!(matches!(
            too_many.resolve(),
            Err(AutomationParamsError::InvalidOperation(
                DocumentAutomationRequestError::TooManyExtractionFields { .. }
            ))
        ));
    }

    #[test]
    fn resolved_automation_binds_private_target_only_after_wire_validation() {
        let raw = pending_fixture();
        let params: DomTextParams = serde_json::from_value(json!({
            "selector": "#status",
            "expectedGeneration": ABOVE_JS_SAFE_INTEGER.to_string(),
        }))
        .unwrap();
        let request = params
            .resolve()
            .unwrap()
            .bind_to_target(raw.target.clone())
            .unwrap();
        assert_eq!(request.expected_generation(), raw.state_generation);
        assert_eq!(request.target(), &raw.target);
        assert!(matches!(
            request.operation(),
            DocumentAutomationOperation::TextContent { selector } if selector == "#status"
        ));
    }

    #[test]
    fn automation_results_are_minimal_exact_and_order_preserving() {
        let raw = pending_fixture();
        let state_generation = ABOVE_JS_SAFE_INTEGER.to_string();

        let activated = PublicAutomationResult::project(
            PublicAutomationKind::Activate,
            EngineDocumentAutomationResult::Activated,
            &raw,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(activated).unwrap(),
            json!({"stateGeneration": state_generation})
        );

        let queried = PublicAutomationResult::project(
            PublicAutomationKind::Query,
            EngineDocumentAutomationResult::QueryCount { count: 7 },
            &raw,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(queried).unwrap(),
            json!({"count": "7", "stateGeneration": ABOVE_JS_SAFE_INTEGER.to_string()})
        );

        let text = PublicAutomationResult::project(
            PublicAutomationKind::Text,
            EngineDocumentAutomationResult::TextContent {
                value: "ready".to_owned(),
            },
            &raw,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(text).unwrap(),
            json!({
                "value": "ready",
                "stateGeneration": ABOVE_JS_SAFE_INTEGER.to_string(),
            })
        );

        let extracted = PublicAutomationResult::project(
            PublicAutomationKind::Extract,
            EngineDocumentAutomationResult::Extract {
                rows: vec![EngineDocumentExtractionRow {
                    fields: vec![
                        EngineDocumentExtractionValue {
                            name: "second".to_owned(),
                            value: "2".to_owned(),
                        },
                        EngineDocumentExtractionValue {
                            name: "first".to_owned(),
                            value: "1".to_owned(),
                        },
                    ],
                }],
            },
            &raw,
        )
        .unwrap();
        let extracted = serde_json::to_value(extracted).unwrap();
        assert_eq!(
            extracted["stateGeneration"],
            ABOVE_JS_SAFE_INTEGER.to_string()
        );
        assert_eq!(extracted["rows"][0]["fields"][0]["name"], "second");
        assert_eq!(extracted["rows"][0]["fields"][1]["name"], "first");

        assert_eq!(
            PublicAutomationResult::project(
                PublicAutomationKind::Activate,
                EngineDocumentAutomationResult::TextContent {
                    value: String::new(),
                },
                &raw,
            ),
            Err(AutomationResultProjectionError::UnexpectedResult)
        );
        assert_eq!(
            PublicAutomationResult::project(
                PublicAutomationKind::Text,
                EngineDocumentAutomationResult::InnerHtml {
                    value: String::new(),
                },
                &raw,
            ),
            Err(AutomationResultProjectionError::InternalOnlyResult)
        );
        assert_eq!(
            PublicAutomationResult::project(
                PublicAutomationKind::Activate,
                EngineDocumentAutomationResult::Filled,
                &raw,
            ),
            Err(AutomationResultProjectionError::InternalOnlyResult)
        );
    }

    #[test]
    fn exact_values_above_javascript_safe_integer_are_decimal_strings() {
        let mut context = WireProjectionContext::new();
        let result = RuntimePendingResult::project(&pending_fixture(), &mut context);
        let value = serde_json::to_value(result).unwrap();

        assert_eq!(
            value["stateGeneration"],
            Value::String(ABOVE_JS_SAFE_INTEGER.to_string())
        );
        assert_eq!(
            value["domEpoch"],
            Value::String((ABOVE_JS_SAFE_INTEGER + 2).to_string())
        );
        assert_eq!(
            value["virtualTimeNs"],
            Value::String(LARGE_VIRTUAL_TIME.to_string())
        );
        assert_eq!(
            value["sources"][0]["openEnded"]["requestedPeriodNs"],
            "5000000000"
        );
    }

    #[test]
    fn timer_deadline_comes_from_logical_inventory_not_the_global_scheduler_head() {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(0),
        });
        let mut scheduler = TimerScheduler::with_clock(clock);
        scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(5),
        });
        let logical_timer_id = scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(20),
        });
        let joined = scheduler
            .join_live_deadlines(scheduler.id(), &[logical_timer_id])
            .unwrap()[0];
        let logical_wake = TimerDeadlineSnapshot {
            scheduler_id: joined.scheduler_id,
            id: joined.id,
            deadline: joined.deadline.unwrap(),
        };
        assert_ne!(
            scheduler.finite_deadline_snapshot().unwrap(),
            Some(logical_wake)
        );

        let pipeline_id = PipelineId {
            namespace_id: PipelineNamespaceId(82),
            index: Index::<PipelineIndex>::new(1).unwrap(),
        };
        let logical_timers =
            PendingLogicalTimerSnapshot::new(vec![PendingLogicalTimerObservation {
                source_id: PendingSourceId::new(1),
                pipeline_id,
                stable_id: PendingLogicalTimerStableId::JavaScriptHandle(1),
                creation_sequence: 1,
                kind: PendingLogicalTimerKind::JavaScriptOneShot,
                logical_deadline: logical_wake.deadline,
                suspended: false,
                eligible_in_controlled_turn: true,
                is_ordering_head: true,
                delivery_ready: false,
                outer_wake: Some(logical_wake),
            }])
            .unwrap();

        assert_eq!(
            project_next_logical_timer_deadline(&logical_timers)
                .unwrap()
                .as_str(),
            logical_wake.deadline.as_nanos().to_string()
        );
    }

    #[test]
    fn opaque_source_ids_are_stable_without_exposing_allocator_values() {
        let raw = pending_fixture();
        let mut context = WireProjectionContext::new();
        let before =
            serde_json::to_value(RuntimePendingResult::project(&raw, &mut context)).unwrap();
        let surviving_id = before["sources"][1]["sourceId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            !surviving_id.is_empty() &&
                surviving_id.bytes().all(|byte| byte.is_ascii_digit()) &&
                (surviving_id == "0" || !surviving_id.starts_with('0'))
        );
        assert_ne!(surviving_id, (ABOVE_JS_SAFE_INTEGER + 1).to_string());

        let mut later = raw.clone();
        later.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(ABOVE_JS_SAFE_INTEGER + 4),
            vec![raw.sources.sources()[1]],
        )
        .unwrap();
        later.validate().unwrap();
        let after =
            serde_json::to_value(RuntimePendingResult::project(&later, &mut context)).unwrap();
        assert_eq!(after["sources"][0]["sourceId"], surviving_id);

        context.observe_event_loop(ScriptEventLoopId::new());
        let recycled_engine_id = context.source_id(ABOVE_JS_SAFE_INTEGER + 1);
        assert_ne!(
            serde_json::to_value(recycled_engine_id).unwrap(),
            after["sources"][0]["sourceId"]
        );
    }

    #[test]
    fn request_params_are_strict_and_wide_values_must_be_canonical_strings() {
        let pending: RuntimePendingParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(pending, RuntimePendingParams {});
        let advance: RuntimeAdvanceToNextParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(advance, RuntimeAdvanceToNextParams {});
        let defaults: RuntimeSettleParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(defaults, RuntimeSettleParams::default());
        assert!(serde_json::from_value::<RuntimePendingParams>(json!({"surprise": true})).is_err());
        assert!(serde_json::from_value::<RuntimePendingParams>(json!([])).is_err());
        assert!(
            serde_json::from_value::<RuntimeAdvanceToNextParams>(json!({"advanceToken": "x"}))
                .is_err()
        );
        assert!(serde_json::from_value::<RuntimeAdvanceToNextParams>(json!([])).is_err());

        let params: RuntimeSettleParams = serde_json::from_value(json!({
            "persistentWork": "strict",
            "wallIoTimeoutNs": ABOVE_JS_SAFE_INTEGER.to_string(),
            "maxVirtualTimeNs": (ABOVE_JS_SAFE_INTEGER + 1).to_string(),
            "maxControlTurns": (ABOVE_JS_SAFE_INTEGER + 2).to_string()
        }))
        .unwrap();
        assert_eq!(params.persistent_work, PersistentWorkPolicy::Strict);
        assert_eq!(
            params.wall_io_timeout_ns.as_ref().unwrap().get(),
            u128::from(ABOVE_JS_SAFE_INTEGER)
        );
        assert_eq!(
            params.max_virtual_time_ns.as_ref().unwrap().get(),
            u128::from(ABOVE_JS_SAFE_INTEGER + 1)
        );
        assert_eq!(
            params.max_control_turns.as_ref().unwrap().get(),
            u128::from(ABOVE_JS_SAFE_INTEGER + 2)
        );
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "persistentWork": "Strict"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "wallIoTimeoutNs": ABOVE_JS_SAFE_INTEGER
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "wallIoTimeoutNs": "01"
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<RuntimeSettleParams>(json!([])).is_err());
        assert!(serde_json::from_value::<RuntimeSettleParams>(json!({"policy": []})).is_err());
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "limits": []
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "wallIoTimeoutNs": null
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "maxEvents": "1"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RuntimeSettleParams>(json!({
                "maxTasks": "1"
            }))
            .is_err()
        );

        let excessive_turns: RuntimeSettleParams = serde_json::from_value(json!({
            "maxControlTurns": (u64::MAX as u128 + 1).to_string()
        }))
        .unwrap();
        assert_eq!(
            excessive_turns.resolve(EngineSettlePolicy::default()),
            Err(SettleParamsError::CountOutOfRange("maxControlTurns"))
        );
        let excessive_duration: RuntimeSettleParams = serde_json::from_value(json!({
            "maxVirtualTimeNs": u128::MAX.to_string()
        }))
        .unwrap();
        assert_eq!(
            excessive_duration.resolve(EngineSettlePolicy::default()),
            Err(SettleParamsError::DurationOutOfRange("maxVirtualTimeNs"))
        );
    }

    #[test]
    fn producer_fallback_is_not_projected_as_a_physical_request() {
        let (kind, count_index) = project_network_kind(PendingNetworkKind::ProducerFallback);
        assert_eq!(kind, NetworkKind::UnclassifiedProducerIo);
        assert_eq!(count_index, 7);
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            "unclassified_producer_io"
        );
    }

    #[test]
    fn all_time_surfaces_project_to_exact_wire_values() {
        for (surface, wire_name) in [
            (DocumentTimeSurface::WindowTimers, "window_timers"),
            (
                DocumentTimeSurface::SameEventLoopIframe,
                "same_event_loop_iframe",
            ),
            (DocumentTimeSurface::JavaScriptDate, "java_script_date"),
            (DocumentTimeSurface::Performance, "performance"),
            (DocumentTimeSurface::HostTimestamp, "host_timestamp"),
            (DocumentTimeSurface::UpdateRendering, "update_rendering"),
            (DocumentTimeSurface::AnimationFrame, "animation_frame"),
            (DocumentTimeSurface::DocumentTimeline, "document_timeline"),
            (DocumentTimeSurface::Worker, "worker"),
            (DocumentTimeSurface::Worklet, "worklet"),
            (
                DocumentTimeSurface::CrossEventLoopIframe,
                "cross_event_loop_iframe",
            ),
            (
                DocumentTimeSurface::CrossEventLoopNavigation,
                "cross_event_loop_navigation",
            ),
            (DocumentTimeSurface::AuxiliaryWebView, "auxiliary_web_view"),
            (DocumentTimeSurface::ResourceThreadIo, "resource_thread_io"),
            (
                DocumentTimeSurface::ExternalSubscription,
                "external_subscription",
            ),
            (DocumentTimeSurface::NativeMedia, "native_media"),
            (DocumentTimeSurface::EmbedderControl, "embedder_control"),
        ] {
            assert_eq!(
                serde_json::to_value(project_time_surface(surface)).unwrap(),
                wire_name
            );
        }
    }

    #[test]
    fn settle_and_advance_results_have_the_mvp_shape() {
        let raw = pending_fixture();
        let mut context = WireProjectionContext::new();
        let interval = raw.sources.sources()[0];
        let policy = RuntimeSettleParams::default()
            .resolve(EngineSettlePolicy::default())
            .unwrap();
        let settled = RuntimeSettleResult::project(
            SettleCompletion::QuiescentWithPersistentWork {
                pending: Box::new(raw.clone()),
                persistent: vec![SettlePersistentWork::Source(interval)],
                control_turns: 18,
            },
            Duration::from_nanos(22),
            policy,
            &mut context,
        );
        let settled = serde_json::to_value(settled).unwrap();
        assert_eq!(settled["outcome"], "quiescent_with_persistent_work");
        assert_eq!(settled["wallTimeNs"], "22");
        assert_eq!(settled["processed"]["controlTurns"], "18");
        assert_eq!(
            settled["processed"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["controlTurns"]
        );
        assert_eq!(settled["persistentWork"][0]["reason"], "interval");
        assert_eq!(
            settled["persistentWork"][0]["sourceId"],
            settled["snapshot"]["sources"][0]["sourceId"]
        );
        assert!(settled["unsupportedWork"].as_array().unwrap().is_empty());

        let strict_policy = RuntimeSettleParams {
            persistent_work: PersistentWorkPolicy::Strict,
            ..RuntimeSettleParams::default()
        }
        .resolve(EngineSettlePolicy::default())
        .unwrap();
        let strict = RuntimeSettleResult::project(
            SettleCompletion::QuiescentWithPersistentWork {
                pending: Box::new(raw.clone()),
                persistent: vec![SettlePersistentWork::Source(interval)],
                control_turns: 18,
            },
            Duration::from_nanos(22),
            strict_policy,
            &mut context,
        );
        let strict = serde_json::to_value(strict).unwrap();
        assert_eq!(strict["outcome"], "blocked_on_open_ended_work");
        assert_eq!(strict["effectivePolicy"]["persistentWork"], "strict");

        let unsupported = RuntimeSettleResult::project(
            SettleCompletion::RuntimeError {
                pending: Box::new(raw.clone()),
                failure: SettleRuntimeFailure::UnsupportedSource(raw.sources.sources()[1]),
                control_turns: 3,
            },
            Duration::from_nanos(7),
            RuntimeSettleParams::default()
                .resolve(EngineSettlePolicy::default())
                .unwrap(),
            &mut context,
        );
        let unsupported = serde_json::to_value(unsupported).unwrap();
        assert_eq!(unsupported["outcome"], "unsupported_work");
        assert_eq!(unsupported["failure"]["code"], "unsupported_source");
        assert_eq!(unsupported["unsupportedWork"][0]["reason"], "time_surface");

        let retained_tasks = RuntimeSettleResult::project(
            SettleCompletion::RuntimeError {
                pending: Box::new(raw.clone()),
                failure: SettleRuntimeFailure::UnsupportedRetainedTasks(PendingTaskObservation {
                    ready: 0,
                    throttled: 2,
                    inactive: 3,
                }),
                control_turns: 4,
            },
            Duration::from_nanos(8),
            RuntimeSettleParams::default()
                .resolve(EngineSettlePolicy::default())
                .unwrap(),
            &mut context,
        );
        let retained_tasks = serde_json::to_value(retained_tasks).unwrap();
        assert_eq!(retained_tasks["outcome"], "unsupported_work");
        assert_eq!(retained_tasks["unsupportedWork"][0]["count"], "2");
        assert_eq!(
            retained_tasks["unsupportedWork"][1]["reason"],
            "inactive_task"
        );

        let identity_changed = RuntimeSettleResult::project(
            SettleCompletion::RuntimeError {
                pending: Box::new(raw.clone()),
                failure: SettleRuntimeFailure::WebViewIdentityChanged,
                control_turns: 5,
            },
            Duration::from_nanos(9),
            RuntimeSettleParams::default()
                .resolve(EngineSettlePolicy::default())
                .unwrap(),
            &mut context,
        );
        let identity_changed = serde_json::to_value(identity_changed).unwrap();
        assert_eq!(identity_changed["outcome"], "runtime_error");
        assert_eq!(
            identity_changed["failure"]["code"],
            "web_view_identity_changed"
        );

        let control_turn_limit = RuntimeSettleResult::project(
            SettleCompletion::ControlTurnLimitExceeded {
                pending: Box::new(raw.clone()),
                limit: 11,
                control_turns: 11,
            },
            Duration::from_nanos(10),
            RuntimeSettleParams::default()
                .resolve(EngineSettlePolicy::default())
                .unwrap(),
            &mut context,
        );
        let control_turn_limit = serde_json::to_value(control_turn_limit).unwrap();
        assert_eq!(control_turn_limit["outcome"], "control_turn_limit_exceeded");
        assert_eq!(control_turn_limit["processed"]["controlTurns"], "11");
        assert_eq!(control_turn_limit["limit"]["kind"], "control_turns");
        assert_eq!(control_turn_limit["limit"]["limit"], "11");
        assert!(
            control_turn_limit["limit"]
                .get("startVirtualTimeNs")
                .is_none()
        );
        assert!(
            control_turn_limit["limit"]
                .get("requestedVirtualTimeNs")
                .is_none()
        );

        let advanced = RuntimeAdvanceToNextResult::project(
            RuntimeAdvanceToNextFacts::Advanced {
                from_virtual_time_ns: 1,
                final_snapshot: &raw,
            },
            &mut context,
        );
        let advanced = serde_json::to_value(advanced).unwrap();
        assert_eq!(advanced["outcome"], "advanced");
        assert_eq!(advanced["fromVirtualTimeNs"], "1");
        assert_eq!(advanced["virtualTimeNs"], LARGE_VIRTUAL_TIME.to_string());

        let no_deadline = RuntimeAdvanceToNextResult::project(
            RuntimeAdvanceToNextFacts::NoFiniteDeadline {
                final_snapshot: &raw,
            },
            &mut context,
        );
        let no_deadline = serde_json::to_value(no_deadline).unwrap();
        assert_eq!(no_deadline["outcome"], "no_finite_deadline");
        assert!(no_deadline.get("fromVirtualTimeNs").is_none());
    }

    #[test]
    fn golden_projection_never_exposes_engine_authority_or_urls() {
        let raw = pending_fixture();
        let mut context = WireProjectionContext::new();
        let projected = RuntimeAdvanceToNextResult::project(
            RuntimeAdvanceToNextFacts::Advanced {
                from_virtual_time_ns: LARGE_VIRTUAL_TIME - 1,
                final_snapshot: &raw,
            },
            &mut context,
        );
        let value = serde_json::to_value(projected).unwrap();
        let mut top_level_keys: Vec<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        top_level_keys.sort_unstable();
        assert_eq!(
            top_level_keys,
            [
                "fromVirtualTimeNs",
                "outcome",
                "snapshot",
                "stateGeneration",
                "virtualTimeNs",
            ]
        );

        fn audit(value: &Value) {
            match value {
                Value::Object(object) => {
                    for (key, child) in object {
                        let normalized: String = key
                            .chars()
                            .filter(|character| *character != '_')
                            .flat_map(char::to_lowercase)
                            .collect();
                        let authority_fragments = [
                            "clockid",
                            "schedulerid",
                            "timerid",
                            "fenceid",
                            "producerfenceid",
                            "advancetoken",
                            "pipelineid",
                            "eventloopid",
                            "webviewid",
                        ];
                        assert!(
                            !authority_fragments
                                .iter()
                                .any(|fragment| normalized.contains(fragment)) &&
                                !normalized.contains("url"),
                            "forbidden wire field {key}"
                        );
                        audit(child);
                    }
                },
                Value::Array(values) => values.iter().for_each(audit),
                Value::String(text) => {
                    assert!(
                        url::Url::parse(text).is_err(),
                        "raw URL leaked into wire value"
                    )
                },
                _ => {},
            }
        }
        let pending =
            serde_json::to_value(RuntimePendingResult::project(&raw, &mut context)).unwrap();
        let settled = serde_json::to_value(RuntimeSettleResult::project(
            SettleCompletion::Quiescent {
                pending: Box::new(raw),
                control_turns: 0,
            },
            Duration::ZERO,
            RuntimeSettleParams::default()
                .resolve(EngineSettlePolicy::default())
                .unwrap(),
            &mut context,
        ))
        .unwrap();
        for result in [&pending, &settled, &value] {
            audit(result);
        }
    }
}
