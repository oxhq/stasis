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
    DocumentSelectorGrammar,
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
use embedder_traits::document_session::SessionNavigationAuthority;
use serde::de::value::MapAccessDeserializer;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use servo_base::id::ScriptEventLoopId;
use timers::{DocumentExecutionBudget, DocumentTimeSurface};

use crate::settle::{
    PersistentWork as SettlePersistentWork, SettleCompletion, SettlePolicy as EngineSettlePolicy,
    SettleRuntimeFailure,
};
use crate::token_namespace::{
    OpaqueTokenNamespace, format_namespaced_token, split_namespaced_token,
};

const DOCUMENT_STATE_TOKEN_MAX_BYTES: usize = 81;

/// The public automation request/result envelope must fit the protocol's one-MiB frame limit even
/// when every user string takes serde_json's six-byte `\u00xx` escape form. These product limits
/// are intentionally narrower than Servo's same-build MVP ceilings: besides logical output, an
/// extraction response pays JSON structure overhead for every row and field.
const PUBLIC_AUTOMATION_MAX_SELECTOR_BYTES: u32 = 4 * 1024;
const PUBLIC_AUTOMATION_MAX_FILL_VALUE_BYTES: u32 = 128 * 1024;
const PUBLIC_AUTOMATION_MAX_FIELD_NAME_BYTES: u32 = 256;
const PUBLIC_AUTOMATION_MAX_EXTRACTION_FIELDS: u32 = 16;
const PUBLIC_AUTOMATION_MAX_MATCHES: u32 = 128;
const PUBLIC_AUTOMATION_MAX_DOM_NODES_VISITED: u32 = 1_000_000;
const PUBLIC_AUTOMATION_MAX_OUTPUT_BYTES: u64 = 128 * 1024;
#[cfg(test)]
const PUBLIC_AUTOMATION_FRAME_BUDGET_BYTES: usize = 1024 * 1024;

fn public_automation_limits() -> DocumentAutomationLimits {
    DocumentAutomationLimits::new_internal(
        PUBLIC_AUTOMATION_MAX_SELECTOR_BYTES,
        PUBLIC_AUTOMATION_MAX_FILL_VALUE_BYTES,
        PUBLIC_AUTOMATION_MAX_FIELD_NAME_BYTES,
        PUBLIC_AUTOMATION_MAX_EXTRACTION_FIELDS,
        PUBLIC_AUTOMATION_MAX_MATCHES,
        PUBLIC_AUTOMATION_MAX_DOM_NODES_VISITED,
        PUBLIC_AUTOMATION_MAX_OUTPUT_BYTES,
    )
    .expect("the product automation limits stay within Servo's hard MVP ceilings")
}

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
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
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
/// Standalone `InnerHtml` deliberately has no public variant; bounded extraction can request an
/// HTML field without adding another exact-one inspection method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicAutomationKind {
    Activate,
    Fill,
    Focus,
    Check,
    Uncheck,
    Select,
    Submit,
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
    limits: DocumentAutomationLimits,
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
        let limits = public_automation_limits();
        operation
            .validate(limits)
            .map_err(AutomationParamsError::InvalidOperation)?;
        Ok(Self {
            kind,
            expected_generation,
            operation,
            limits,
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
            self.limits,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutomationParamsError {
    ExpectedGenerationOutOfRange,
    InvalidOperation(DocumentAutomationRequestError),
}

/// A strict, bounded controlled-session automation request before owner authorization.
///
/// Unlike the frozen v0.1 request, this retains only an opaque product token. The engine
/// generation and complete private target are taken from the fresh Observe which authorizes the
/// token, immediately before constructing the same-build request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSessionAutomationParams {
    kind: PublicAutomationKind,
    expected_state_token: DocumentStateToken,
    operation: DocumentAutomationOperation,
    limits: DocumentAutomationLimits,
}

impl ResolvedSessionAutomationParams {
    fn new(
        kind: PublicAutomationKind,
        expected_state_token: DocumentStateToken,
        operation: DocumentAutomationOperation,
    ) -> Result<Self, AutomationParamsError> {
        let limits = public_automation_limits();
        operation
            .validate(limits)
            .map_err(AutomationParamsError::InvalidOperation)?;
        Ok(Self {
            kind,
            expected_state_token,
            operation,
            limits,
        })
    }

    pub const fn kind(&self) -> PublicAutomationKind {
        self.kind
    }

    /// Authorize the opaque product token against a fresh owner observation, then bind all
    /// private authority and the exact observed generation into the existing Servo request.
    pub fn authorize_and_bind(
        self,
        observed: &RawPendingSnapshot,
        navigation: &SessionNavigationAuthority,
        context: &mut WireProjectionContext,
    ) -> Result<DocumentAutomationRequest, SessionAutomationBindError> {
        let authorized = context
            .authorizes_document_state(observed, navigation, &self.expected_state_token)
            .map_err(SessionAutomationBindError::Authority)?;
        if !authorized {
            return Err(SessionAutomationBindError::StaleStateToken);
        }
        DocumentAutomationRequest::new_with_selector_grammar_internal(
            observed.target.clone(),
            observed.state_generation,
            self.operation,
            self.limits,
            DocumentSelectorGrammar::PracticalV2,
        )
        .map_err(SessionAutomationBindError::InvalidRequest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionAutomationBindError {
    StaleStateToken,
    Authority(DocumentStateAuthorityError),
    InvalidRequest(DocumentAutomationRequestError),
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

/// Strict parameters for `action.fill`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ActionFillParams {
    selector: String,
    value: String,
    expected_generation: DecimalU128,
}

impl ActionFillParams {
    pub fn resolve(self) -> Result<ResolvedAutomationParams, AutomationParamsError> {
        ResolvedAutomationParams::new(
            PublicAutomationKind::Fill,
            self.expected_generation,
            DocumentAutomationOperation::Fill {
                selector: self.selector,
                value: self.value,
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

/// Token-authorized controlled-session parameters for `action.activate`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionActionActivateParams {
    selector: String,
    expected_state_token: DocumentStateToken,
}

impl SessionActionActivateParams {
    pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
        ResolvedSessionAutomationParams::new(
            PublicAutomationKind::Activate,
            self.expected_state_token,
            DocumentAutomationOperation::Activate {
                selector: self.selector,
            },
        )
    }
}

/// Token-authorized controlled-session parameters for `action.fill`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionActionFillParams {
    selector: String,
    value: String,
    expected_state_token: DocumentStateToken,
}

macro_rules! session_selector_action {
    ($params:ident, $kind:ident, $operation:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct $params {
            selector: String,
            expected_state_token: DocumentStateToken,
        }

        impl $params {
            pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
                ResolvedSessionAutomationParams::new(
                    PublicAutomationKind::$kind,
                    self.expected_state_token,
                    DocumentAutomationOperation::$operation {
                        selector: self.selector,
                    },
                )
            }
        }
    };
}

session_selector_action!(SessionActionFocusParams, Focus, Focus);
session_selector_action!(SessionActionCheckParams, Check, Check);
session_selector_action!(SessionActionUncheckParams, Uncheck, Uncheck);
session_selector_action!(SessionActionSubmitParams, Submit, Submit);

/// Token-authorized controlled-session parameters for semantic select replacement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionActionSelectParams {
    selector: String,
    values: Vec<String>,
    expected_state_token: DocumentStateToken,
}

impl SessionActionSelectParams {
    pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
        ResolvedSessionAutomationParams::new(
            PublicAutomationKind::Select,
            self.expected_state_token,
            DocumentAutomationOperation::Select {
                selector: self.selector,
                values: self.values,
            },
        )
    }
}

impl SessionActionFillParams {
    pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
        ResolvedSessionAutomationParams::new(
            PublicAutomationKind::Fill,
            self.expected_state_token,
            DocumentAutomationOperation::Fill {
                selector: self.selector,
                value: self.value,
            },
        )
    }
}

/// Token-authorized controlled-session parameters for `dom.query`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionDomQueryParams {
    selector: String,
    expected_state_token: DocumentStateToken,
}

impl SessionDomQueryParams {
    pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
        ResolvedSessionAutomationParams::new(
            PublicAutomationKind::Query,
            self.expected_state_token,
            DocumentAutomationOperation::QueryCount {
                selector: self.selector,
            },
        )
    }
}

/// Token-authorized controlled-session parameters for `dom.text`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionDomTextParams {
    selector: String,
    expected_state_token: DocumentStateToken,
}

impl SessionDomTextParams {
    pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
        ResolvedSessionAutomationParams::new(
            PublicAutomationKind::Text,
            self.expected_state_token,
            DocumentAutomationOperation::TextContent {
                selector: self.selector,
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionDomExtractRead {
    Text,
    Html,
    Attribute,
    ResolvedUrl,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionDomExtractFieldParams {
    name: String,
    selector: String,
    read: SessionDomExtractRead,
    attribute: Option<String>,
}

/// Token-authorized controlled-session parameters for text, HTML, nullable raw-attribute, and
/// nullable document-base-resolved URL extraction. The frozen v0.1 decoder remains separate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionDomExtractParams {
    root_selector: String,
    fields: Vec<SessionDomExtractFieldParams>,
    expected_state_token: DocumentStateToken,
}

impl SessionDomExtractParams {
    pub fn resolve(self) -> Result<ResolvedSessionAutomationParams, AutomationParamsError> {
        let fields = self
            .fields
            .into_iter()
            .map(|field| match (field.read, field.attribute) {
                (SessionDomExtractRead::Text, None) => DocumentExtractionField::new_internal(
                    field.name,
                    field.selector,
                    DocumentExtractionRead::TextContent,
                ),
                (SessionDomExtractRead::Html, None) => DocumentExtractionField::new_internal(
                    field.name,
                    field.selector,
                    DocumentExtractionRead::InnerHtml,
                ),
                (SessionDomExtractRead::Attribute, Some(attribute)) => {
                    DocumentExtractionField::new_attribute_internal(
                        field.name,
                        field.selector,
                        DocumentExtractionRead::Attribute,
                        attribute,
                    )
                },
                (SessionDomExtractRead::ResolvedUrl, Some(attribute)) => {
                    DocumentExtractionField::new_attribute_internal(
                        field.name,
                        field.selector,
                        DocumentExtractionRead::ResolvedUrl,
                        attribute,
                    )
                },
                (read, attribute) => DocumentExtractionField::new_attribute_internal(
                    field.name,
                    field.selector,
                    match read {
                        SessionDomExtractRead::Text => DocumentExtractionRead::TextContent,
                        SessionDomExtractRead::Html => DocumentExtractionRead::InnerHtml,
                        SessionDomExtractRead::Attribute => DocumentExtractionRead::Attribute,
                        SessionDomExtractRead::ResolvedUrl => DocumentExtractionRead::ResolvedUrl,
                    },
                    attribute.unwrap_or_default(),
                ),
            })
            .collect();
        ResolvedSessionAutomationParams::new(
            PublicAutomationKind::Extract,
            self.expected_state_token,
            DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
                self.root_selector,
                fields,
            )),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionMutationResult {
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionCheckedResult {
    changed: bool,
    checked: bool,
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionSelectedResult {
    changed: bool,
    values: Vec<String>,
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionFocusedResult {
    focused: bool,
    state_generation: DecimalU128,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionSubmittedResult {
    submitted: bool,
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
    value: Option<String>,
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
    Activate(ActionMutationResult),
    Fill(ActionMutationResult),
    Focus(ActionFocusedResult),
    Check(ActionCheckedResult),
    Uncheck(ActionCheckedResult),
    Select(ActionSelectedResult),
    Submit(ActionSubmittedResult),
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
                Ok(Self::Activate(ActionMutationResult { state_generation }))
            },
            (PublicAutomationKind::Fill, EngineDocumentAutomationResult::Filled) => {
                Ok(Self::Fill(ActionMutationResult { state_generation }))
            },
            (PublicAutomationKind::Focus, EngineDocumentAutomationResult::Focused { focused }) => {
                Ok(Self::Focus(ActionFocusedResult {
                    focused,
                    state_generation,
                }))
            },
            (
                PublicAutomationKind::Check,
                EngineDocumentAutomationResult::Checked { changed, checked },
            ) => Ok(Self::Check(ActionCheckedResult {
                changed,
                checked,
                state_generation,
            })),
            (
                PublicAutomationKind::Uncheck,
                EngineDocumentAutomationResult::Checked { changed, checked },
            ) => Ok(Self::Uncheck(ActionCheckedResult {
                changed,
                checked,
                state_generation,
            })),
            (
                PublicAutomationKind::Select,
                EngineDocumentAutomationResult::Selected { changed, values },
            ) => Ok(Self::Select(ActionSelectedResult {
                changed,
                values,
                state_generation,
            })),
            (PublicAutomationKind::Submit, EngineDocumentAutomationResult::Submitted) => {
                Ok(Self::Submit(ActionSubmittedResult {
                    submitted: true,
                    state_generation,
                }))
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
            (_, EngineDocumentAutomationResult::InnerHtml { .. }) => {
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

/// Opaque authorization for one exact controlled-session document state.
///
/// The prefixed value is a product-owned alias, not an engine generation, pipeline, epoch, or
/// allocator identity. A token is meaningful only inside the shell session which issued it.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentStateToken(String);

impl DocumentStateToken {
    fn from_alias(namespace: &OpaqueTokenNamespace, value: u128) -> Self {
        debug_assert_ne!(value, 0);
        Self(format_namespaced_token("document:", namespace, value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DocumentStateToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DocumentStateToken(<redacted>)")
    }
}

impl Serialize for DocumentStateToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DocumentStateToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let token = String::deserialize(deserializer)?;
        if token.len() > DOCUMENT_STATE_TOKEN_MAX_BYTES {
            return Err(de::Error::custom("canonical document token required"));
        }
        let (_, alias) = split_namespaced_token(&token, "document:")
            .map_err(|_| de::Error::custom("canonical document token required"))?;
        alias
            .parse::<u128>()
            .map_err(|_| de::Error::custom("document token alias exceeds u128"))?;
        Ok(Self(token))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DocumentStateAuthority {
    target: PendingTargetObservation,
    state_generation: RuntimeStateGeneration,
    document_epoch: u64,
    navigation_id: u64,
    history_revision: u64,
    navigation: SessionNavigationAuthority,
}

/// Private authority retained for the sole current document-state token.
///
/// `runtime.settle` uses this capability to distinguish an internally progressed controlled
/// source from an arbitrary stale or foreign token. It is deliberately not serializable or
/// debuggable: public consumers receive only the opaque token, while the shell can recover the
/// exact owner-attested navigation and monotonic generation which minted it.
#[derive(Clone)]
#[doc(hidden)]
pub struct CurrentDocumentStateAuthority {
    authority: DocumentStateAuthority,
    token: DocumentStateToken,
}

impl CurrentDocumentStateAuthority {
    #[doc(hidden)]
    pub fn matches_navigation(&self, navigation: &SessionNavigationAuthority) -> bool {
        navigation == &self.authority.navigation
    }

    #[doc(hidden)]
    pub fn target(&self) -> &PendingTargetObservation {
        self.authority.navigation.target()
    }

    #[doc(hidden)]
    pub const fn state_generation(&self) -> RuntimeStateGeneration {
        self.authority.state_generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentStateAuthorityError {
    NavigationTargetDoesNotMatchPending,
    TokenEntropyUnavailable,
    TokenSpaceExhausted,
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
    document_state_authority: Option<DocumentStateAuthority>,
    document_state_token: Option<DocumentStateToken>,
    /// A strict request observed that the latest public token no longer matched the current
    /// document state. Keep the public capability available to the one narrow `runtime.settle`
    /// recovery path, but never let strict authorization regain authority through an ABA return.
    document_state_strictly_invalidated: bool,
    document_token_namespace: Option<OpaqueTokenNamespace>,
    next_document_state_alias: Option<u128>,
}

impl Default for WireProjectionContext {
    fn default() -> Self {
        Self {
            event_loop_id: None,
            source_ids: BTreeMap::new(),
            next_source_alias: 1,
            document_state_authority: None,
            document_state_token: None,
            document_state_strictly_invalidated: false,
            document_token_namespace: None,
            next_document_state_alias: Some(1),
        }
    }
}

impl WireProjectionContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a projection context with an owner-supplied token namespace.
    ///
    /// This is an internal seam for deterministic same-build tests. Production contexts must use
    /// [`Self::new`] so their namespace comes from the operating system CSPRNG on first use.
    #[doc(hidden)]
    pub fn new_with_namespace_internal(namespace: OpaqueTokenNamespace) -> Self {
        Self {
            document_token_namespace: Some(namespace),
            ..Self::default()
        }
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

    /// Return a stable session-local token for this exact target and complete-state generation.
    ///
    /// Only the current binding is retained. Any target transition or state-generation change
    /// rotates the token, so an earlier document can never become authorized again through an
    /// ABA transition.
    pub fn document_state_token(
        &mut self,
        raw: &RawPendingSnapshot,
        navigation: &SessionNavigationAuthority,
    ) -> Result<DocumentStateToken, DocumentStateAuthorityError> {
        if navigation.target() != &raw.target {
            return Err(DocumentStateAuthorityError::NavigationTargetDoesNotMatchPending);
        }
        let authority = DocumentStateAuthority {
            target: raw.target.clone(),
            state_generation: raw.state_generation,
            document_epoch: navigation.document_epoch().get(),
            navigation_id: navigation.navigation_id().get(),
            history_revision: navigation.history_revision().get(),
            navigation: navigation.clone(),
        };
        if self.document_state_authority.as_ref() == Some(&authority)
            && !self.document_state_strictly_invalidated
        {
            return Ok(self
                .document_state_token
                .clone()
                .expect("an observed document-state authority always has a token"));
        }

        let alias = self
            .next_document_state_alias
            .ok_or(DocumentStateAuthorityError::TokenSpaceExhausted)?;
        let namespace = match self.document_token_namespace.as_ref() {
            Some(namespace) => namespace,
            None => {
                let namespace = OpaqueTokenNamespace::generate()
                    .map_err(|_| DocumentStateAuthorityError::TokenEntropyUnavailable)?;
                self.document_token_namespace.insert(namespace)
            },
        };
        let token = DocumentStateToken::from_alias(namespace, alias);
        // Entropy acquisition must fail before consuming public token authority. The shell fails
        // closed on that error, but keeping allocation transactional also preserves the invariant
        // for direct library users and deterministic fault-injection tests.
        self.next_document_state_alias = alias.checked_add(1);
        self.document_state_authority = Some(authority);
        self.document_state_token = Some(token.clone());
        self.document_state_strictly_invalidated = false;
        Ok(token)
    }

    /// Check supplied document authorization against a freshly observed owner snapshot.
    ///
    /// A foreign or superseded token is rejected before fresh authority is inspected. A mismatch
    /// for the exact latest public token latches that token strict-stale without minting a hidden
    /// replacement; only the narrow `runtime.settle` capability resolver may still recover it.
    /// The latch prevents a later byte-identical ABA observation from re-authorizing the token.
    pub fn authorizes_document_state(
        &mut self,
        raw: &RawPendingSnapshot,
        navigation: &SessionNavigationAuthority,
        supplied: &DocumentStateToken,
    ) -> Result<bool, DocumentStateAuthorityError> {
        if self.document_state_token.as_ref() != Some(supplied) {
            return Ok(false);
        }
        if self.document_state_strictly_invalidated {
            return Ok(false);
        }
        if navigation.target() != &raw.target {
            self.document_state_strictly_invalidated = true;
            return Err(DocumentStateAuthorityError::NavigationTargetDoesNotMatchPending);
        }
        let observed = DocumentStateAuthority {
            target: raw.target.clone(),
            state_generation: raw.state_generation,
            document_epoch: navigation.document_epoch().get(),
            navigation_id: navigation.navigation_id().get(),
            history_revision: navigation.history_revision().get(),
            navigation: navigation.clone(),
        };
        if self.document_state_authority.as_ref() == Some(&observed) {
            Ok(true)
        } else {
            self.document_state_strictly_invalidated = true;
            Ok(false)
        }
    }

    /// Resolve only the exact token which is still the current public document capability.
    ///
    /// This does not observe or rotate authority. The caller must independently bracket fresh
    /// document and session-owner observations before using the result, and no earlier token is
    /// retained for recovery.
    #[doc(hidden)]
    pub fn current_document_state_authority(
        &self,
        supplied: &DocumentStateToken,
    ) -> Option<CurrentDocumentStateAuthority> {
        if self.document_state_token.as_ref() != Some(supplied) {
            return None;
        }
        let authority = self
            .document_state_authority
            .as_ref()
            .expect("the current document token always has retained authority");
        Some(CurrentDocumentStateAuthority {
            authority: authority.clone(),
            token: supplied.clone(),
        })
    }

    /// Permanently make the exact current public document capability strict-stale.
    ///
    /// The expected private authority must still be the complete current binding. This makes a
    /// post-resolution stale decision transactional: an intervening public projection can never
    /// cause a newer token to be invalidated, while a byte-identical ABA return cannot revive the
    /// capability which the settle bracket already rejected.
    #[doc(hidden)]
    pub fn latch_current_document_state_strictly_invalidated(
        &mut self,
        expected: &CurrentDocumentStateAuthority,
    ) -> bool {
        if self.document_state_token.as_ref() != Some(&expected.token)
            || self.document_state_authority.as_ref() != Some(&expected.authority)
        {
            return false;
        }
        self.document_state_strictly_invalidated = true;
        true
    }
}

/// Additive controlled-session result envelope. The flattened legacy projection stays byte-for-
/// byte unchanged for `controlled-webapp-v1`; only the new profile uses this wrapper.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDocumentResult<T> {
    #[serde(flatten)]
    result: T,
    state_token: DocumentStateToken,
}

impl<T> SessionDocumentResult<T> {
    pub fn new(
        result: T,
        raw: &RawPendingSnapshot,
        navigation: &SessionNavigationAuthority,
        context: &mut WireProjectionContext,
    ) -> Result<Self, DocumentStateAuthorityError> {
        Ok(Self {
            result,
            state_token: context.document_state_token(raw, navigation)?,
        })
    }
}

/// Strict parameters for `runtime.pending`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimePendingParams {}

/// Strict parameters for `runtime.advance_to_next`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeAdvanceToNextParams {}

/// Strict token-authorized parameters for controlled-session advancement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionRuntimeAdvanceToNextParams {
    pub expected_state_token: DocumentStateToken,
}

/// Strict parameters for an explicit controlled-session top-level navigation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionNavigateParams {
    pub url: String,
    pub expected_state_token: DocumentStateToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionNavigateBoundary {
    ControlledReady,
}

/// Authoritative result after the admitted replacement has reached a controlled terminal
/// settlement observation and the final navigation/document authority has been re-observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNavigateResult {
    pub requested_url: String,
    pub url: String,
    pub boundary: SessionNavigateBoundary,
    pub state_generation: DecimalU128,
    pub dom_epoch: DecimalU128,
    pub document_epoch: DecimalU128,
    pub navigation_id: DecimalU128,
    pub history_revision: DecimalU128,
    pub state_token: DocumentStateToken,
}

impl SessionNavigateResult {
    pub fn project(
        requested_url: String,
        pending: &RawPendingSnapshot,
        navigation: &SessionNavigationAuthority,
        context: &mut WireProjectionContext,
    ) -> Result<Self, DocumentStateAuthorityError> {
        Ok(Self {
            requested_url,
            url: navigation.url().to_string(),
            boundary: SessionNavigateBoundary::ControlledReady,
            state_generation: pending.state_generation.get().into(),
            dom_epoch: pending.dom_epoch.get().into(),
            document_epoch: navigation.document_epoch().get().into(),
            navigation_id: navigation.navigation_id().get().into(),
            history_revision: navigation.history_revision().get().into(),
            state_token: context.document_state_token(pending, navigation)?,
        })
    }
}

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

/// Strict token-authorized parameters for controlled-session settlement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionRuntimeSettleParams {
    pub expected_state_token: DocumentStateToken,
    #[serde(default)]
    persistent_work: PersistentWorkPolicy,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    max_virtual_time_ns: Option<DecimalU128>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    max_control_turns: Option<DecimalU128>,
    #[serde(default, deserialize_with = "deserialize_optional_decimal")]
    wall_io_timeout_ns: Option<DecimalU128>,
}

impl SessionRuntimeSettleParams {
    pub fn resolve(
        self,
        defaults: EngineSettlePolicy,
    ) -> Result<(DocumentStateToken, ResolvedSettlePolicy), SettleParamsError> {
        let policy = RuntimeSettleParams {
            persistent_work: self.persistent_work,
            max_virtual_time_ns: self.max_virtual_time_ns,
            max_control_turns: self.max_control_turns,
            wall_io_timeout_ns: self.wall_io_timeout_ns,
        }
        .resolve(defaults)?;
        Ok((self.expected_state_token, policy))
    }
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
    HistoryTraversal,
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
    RenderBlockingElement,
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
    TaskLimitExceeded,
    MicrotaskLimitExceeded,
    RenderingLimitExceeded,
    MutationLimitExceeded,
    ControlTurnLimitExceeded,
    RuntimeError,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedWorkSnapshot {
    pub control_turns: DecimalU128,
    pub tasks: DecimalU128,
    pub microtasks: DecimalU128,
    pub rendering_opportunities: DecimalU128,
    pub mutations: DecimalU128,
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
    OrdinaryTasks,
    Microtasks,
    RenderingOpportunities,
    Mutations,
    ControlTurns,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleLimitSnapshot {
    pub kind: SettleLimitKind,
    pub limit: DecimalU128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<DecimalU128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_virtual_time_ns: Option<DecimalU128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_virtual_time_ns: Option<DecimalU128>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleFailureCode {
    RuntimeTerminals,
    ExecutionCounterOverflow,
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

/// Bounded settlement diagnostics used when a navigation cannot cross the
/// `controlled_ready` boundary. This deliberately reuses the public settle failure/source
/// projection while omitting opaque source identities and the raw engine failure payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct ControlledReadyFailureDetails {
    pub failure: SettleFailureSnapshot,
    pub unsupported_work: Vec<ControlledReadyUnsupportedWork>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct ControlledReadyUnsupportedWork {
    pub kind: SourceKind,
    pub count: DecimalU128,
    #[serde(flatten)]
    pub description: UnsupportedDescription,
}

#[doc(hidden)]
pub fn project_controlled_ready_failure_details(
    failure: &SettleRuntimeFailure,
    pending: &RawPendingSnapshot,
) -> (SettleOutcome, ControlledReadyFailureDetails) {
    let mut context = WireProjectionContext::new();
    let projected = project_settle_failure(failure, pending, &mut context);
    let details = ControlledReadyFailureDetails {
        failure: SettleFailureSnapshot {
            code: projected.code,
        },
        unsupported_work: projected
            .unsupported_work
            .into_iter()
            .map(|work| ControlledReadyUnsupportedWork {
                kind: work.kind,
                count: work.count,
                description: work.description,
            })
            .collect(),
    };
    (projected.outcome, details)
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
                    observed: None,
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
                    observed: None,
                    start_virtual_time_ns: None,
                    requested_virtual_time_ns: None,
                });
                (
                    SettleOutcome::ControlTurnLimitExceeded,
                    pending,
                    control_turns,
                )
            },
            SettleCompletion::ExecutionLimitExceeded {
                pending,
                budget,
                limit: execution_limit,
                observed,
                control_turns,
            } => {
                let (outcome, kind) = match budget {
                    DocumentExecutionBudget::OrdinaryTasks => (
                        SettleOutcome::TaskLimitExceeded,
                        SettleLimitKind::OrdinaryTasks,
                    ),
                    DocumentExecutionBudget::Microtasks => (
                        SettleOutcome::MicrotaskLimitExceeded,
                        SettleLimitKind::Microtasks,
                    ),
                    DocumentExecutionBudget::RenderingOpportunities => (
                        SettleOutcome::RenderingLimitExceeded,
                        SettleLimitKind::RenderingOpportunities,
                    ),
                    DocumentExecutionBudget::MutationRecords => (
                        SettleOutcome::MutationLimitExceeded,
                        SettleLimitKind::Mutations,
                    ),
                };
                limit = Some(SettleLimitSnapshot {
                    kind,
                    limit: execution_limit.into(),
                    observed: Some(observed.into()),
                    start_virtual_time_ns: None,
                    requested_virtual_time_ns: None,
                });
                (outcome, pending, control_turns)
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
        if policy.persistent_work == PersistentWorkPolicy::Strict
            && outcome == SettleOutcome::QuiescentWithPersistentWork
        {
            outcome = SettleOutcome::BlockedOnOpenEndedWork;
        }
        let execution = pending
            .execution
            .map(|observation| observation.counters)
            .unwrap_or_default();
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
                tasks: execution.ordinary_tasks.into(),
                microtasks: execution.microtasks.into(),
                rendering_opportunities: execution.rendering_opportunities.into(),
                mutations: execution.mutations.into(),
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
        DocumentTimeSurface::HistoryTraversal => TimeSurface::HistoryTraversal,
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
            u128::from(pipeline.animated_images.unsupported.loop_count_unavailable)
                + u128::from(pipeline.animated_images.unsupported.timeline_uncontrolled)
                + u128::from(
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
        ) + u128::from(pipeline.canvas.unsupported.offscreen_execution)
            + u128::from(pipeline.canvas.unsupported.mutation_generation_unbound);
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
        SettleRuntimeFailure::ExecutionCounterOverflow(_) => (
            SettleOutcome::RuntimeError,
            SettleFailureCode::ExecutionCounterOverflow,
            vec![],
        ),
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
        PendingOpenEndedSourceReason::Interval { .. }
        | PendingOpenEndedSourceReason::InfiniteAnimation => return None,
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
        u128::from(rendering.animated_images.unsupported.loop_count_unavailable)
            + u128::from(rendering.animated_images.unsupported.timeline_uncontrolled)
            + u128::from(
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
    ) + u128::from(rendering.canvas.unsupported.offscreen_execution)
        + u128::from(rendering.canvas.unsupported.mutation_generation_unbound);
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
        u128::from(rendering.render_blocking_elements),
        SourceKind::RenderingUpdate,
        UnsupportedReason::RenderBlockingElement,
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
    let retained_work = rendering.runnable_animation_frame_callbacks != 0
        || rendering.document_update_required
        || rendering.pending_animation_events != 0
        || rendering.finite_animations != 0
        || rendering.infinite_animations != 0
        || rendering.animated_images.finite_images != 0
        || rendering.animated_images.infinite_images != 0
        || rendering.animated_images.update_ready
        || rendering.animated_images.scheduled_timer.is_some()
        || rendering.canvas.dirty_contexts != 0
        || rendering.pending_fonts != 0
        || rendering.pending_images != 0;
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
                existing.source_id.is_none()
                    && existing.kind == candidate.kind
                    && existing.description == candidate.description
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
        .live_source_inventory_unavailable
        != 0
        || rendering.canvas.unsupported.offscreen_execution != 0
        || rendering.canvas.unsupported.mutation_generation_unbound != 0;
    let inactive_work = rendering.activity != PendingRenderingPipelineActivity::FullyActive
        && (rendering.runnable_animation_frame_callbacks != 0
            || rendering.document_update_required
            || rendering.pending_animation_events != 0
            || rendering.finite_animations != 0
            || rendering.infinite_animations != 0
            || rendering.animated_images.finite_images != 0
            || rendering.animated_images.infinite_images != 0
            || rendering.animated_images.update_ready
            || rendering.animated_images.scheduled_timer.is_some()
            || rendering.canvas.dirty_contexts != 0
            || rendering.pending_fonts != 0
            || rendering.pending_images != 0);
    rendering.render_blocking_elements != 0
        || rendering.unsupported_animations != 0
        || unsupported_images
        || unsupported_canvas
        || inactive_work
        || rendering.canvas.awaiting_async_upload
        || rendering.pending_fonts != 0
        || rendering.pending_images != 0
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
    use embedder_traits::document_session::{
        DocumentEpoch, HistoryRevision, SessionNavigationAuthority, SessionNavigationId,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use servo::ServoUrl;
    use servo_base::id::{
        BrowsingContextId, BrowsingContextIndex, Index, PipelineId, PipelineIndex,
        PipelineNamespaceId, ScriptEventLoopId, WebViewId,
    };
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentExecutionBudget,
        DocumentExecutionCounters, DocumentExecutionLimits, DocumentExecutionObservation,
        DocumentExecutionTerminal, DocumentProducerCheckpoint, DocumentProducerFence,
        DocumentUnixTime, TimerDeadlineSnapshot, TimerEventRequest, TimerScheduler,
    };

    use super::*;

    const ABOVE_JS_SAFE_INTEGER: u64 = 9_007_199_254_740_993;
    const LARGE_VIRTUAL_TIME: u128 = (u64::MAX as u128) + 9_007_199_254_740_993;
    const TEST_DOCUMENT_NAMESPACE_HEX: &str = "61616161616161616161616161616161";

    fn test_projection_context() -> WireProjectionContext {
        WireProjectionContext::new_with_namespace_internal(OpaqueTokenNamespace::new_internal(
            [0x61; 16],
        ))
    }

    fn test_document_token(alias: u128) -> String {
        format!("document:{TEST_DOCUMENT_NAMESPACE_HEX}:{alias}")
    }

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
            execution: Some(DocumentExecutionObservation {
                clock_id: clock.id(),
                limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
                counters: DocumentExecutionCounters::default(),
                terminal: None,
            }),
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

    fn navigation_fixture(pending: &RawPendingSnapshot) -> SessionNavigationAuthority {
        SessionNavigationAuthority::new_internal(
            Box::new(pending.target.clone()),
            DocumentEpoch::new(1),
            SessionNavigationId::new(0),
            HistoryRevision::new(0),
            0,
            ServoUrl::parse("https://example.test/").unwrap(),
            None,
        )
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

        let oversized_selector = "x"
            .repeat(usize::try_from(public_automation_limits().max_selector_bytes()).unwrap() + 1);
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

        let maximum_fill = "\0"
            .repeat(usize::try_from(public_automation_limits().max_fill_value_bytes()).unwrap());
        let fill: ActionFillParams = serde_json::from_value(json!({
            "selector": "#field",
            "value": maximum_fill,
            "expectedGeneration": "1",
        }))
        .unwrap();
        let fill = fill.resolve().unwrap();
        assert_eq!(fill.kind(), PublicAutomationKind::Fill);
        assert!(matches!(
            fill.operation,
            DocumentAutomationOperation::Fill { ref selector, ref value }
                if selector == "#field" &&
                    value.len() == public_automation_limits().max_fill_value_bytes() as usize
        ));

        let oversized_fill: ActionFillParams = serde_json::from_value(json!({
            "selector": "#field",
            "value": "x".repeat(
                usize::try_from(public_automation_limits().max_fill_value_bytes()).unwrap() + 1,
            ),
            "expectedGeneration": "1",
        }))
        .unwrap();
        assert!(matches!(
            oversized_fill.resolve(),
            Err(AutomationParamsError::InvalidOperation(
                DocumentAutomationRequestError::FillValueTooLong { .. }
            ))
        ));
    }

    #[test]
    fn session_automation_uses_only_the_current_opaque_document_token() {
        let pending = pending_fixture();
        let navigation = navigation_fixture(&pending);
        let mut context = test_projection_context();
        let current = context.document_state_token(&pending, &navigation).unwrap();
        assert_eq!(current.as_str(), test_document_token(1));

        let params: SessionActionActivateParams = serde_json::from_value(json!({
            "selector": "#start",
            "expectedStateToken": test_document_token(1),
        }))
        .unwrap();
        let request = params
            .resolve()
            .unwrap()
            .authorize_and_bind(&pending, &navigation, &mut context)
            .unwrap();
        assert_eq!(request.expected_generation(), pending.state_generation);
        assert_eq!(request.target(), &pending.target);
        assert_eq!(
            request.selector_grammar(),
            DocumentSelectorGrammar::PracticalV2
        );

        let mut changed = pending.clone();
        changed.state_generation = RuntimeStateGeneration::new(ABOVE_JS_SAFE_INTEGER + 1);
        let stale: SessionActionActivateParams = serde_json::from_value(json!({
            "selector": "#start",
            "expectedStateToken": test_document_token(1),
        }))
        .unwrap();
        assert_eq!(
            stale
                .resolve()
                .unwrap()
                .authorize_and_bind(&changed, &navigation, &mut context),
            Err(SessionAutomationBindError::StaleStateToken)
        );

        for invalid in [
            json!({"selector": "#start", "expectedStateToken": 1}),
            json!({"selector": "#start", "expectedStateToken": format!("document:{TEST_DOCUMENT_NAMESPACE_HEX}:01")}),
            json!({"selector": "#start", "expectedStateToken": test_document_token(1), "extra": true}),
        ] {
            assert!(serde_json::from_value::<SessionActionActivateParams>(invalid).is_err());
        }
    }

    #[test]
    fn session_forms_and_extraction_are_strict_and_v2_only() {
        let pending = pending_fixture();
        let navigation = navigation_fixture(&pending);
        let mut context = test_projection_context();
        let token = context.document_state_token(&pending, &navigation).unwrap();

        let select: SessionActionSelectParams = serde_json::from_value(json!({
            "selector": "form > select[name=kind]",
            "values": ["primary", "secondary"],
            "expectedStateToken": token,
        }))
        .unwrap();
        let select = select
            .resolve()
            .unwrap()
            .authorize_and_bind(&pending, &navigation, &mut context)
            .unwrap();
        assert!(matches!(
            select.operation(),
            DocumentAutomationOperation::Select { selector, values }
                if selector == "form > select[name=kind]" &&
                    values == &["primary".to_owned(), "secondary".to_owned()]
        ));

        let extract: SessionDomExtractParams = serde_json::from_value(json!({
            "rootSelector": ".card",
            "fields": [
                {"name": "raw", "selector": "a", "read": "attribute", "attribute": "href"},
                {"name": "url", "selector": "a", "read": "resolved_url", "attribute": "href"}
            ],
            "expectedStateToken": context.document_state_token(&pending, &navigation).unwrap(),
        }))
        .unwrap();
        let extract = extract
            .resolve()
            .unwrap()
            .authorize_and_bind(&pending, &navigation, &mut context)
            .unwrap();
        let DocumentAutomationOperation::Extract(plan) = extract.operation() else {
            panic!("expected session extraction")
        };
        assert_eq!(plan.fields()[0].read(), DocumentExtractionRead::Attribute);
        assert_eq!(plan.fields()[0].attribute(), Some("href"));
        assert_eq!(plan.fields()[1].read(), DocumentExtractionRead::ResolvedUrl);

        for invalid in [
            json!({
                "rootSelector": ".card",
                "fields": [{"name": "x", "selector": "a", "read": "attribute"}],
                "expectedStateToken": context.document_state_token(&pending, &navigation).unwrap(),
            }),
            json!({
                "rootSelector": ".card",
                "fields": [{"name": "x", "selector": "a", "read": "text", "attribute": "href"}],
                "expectedStateToken": context.document_state_token(&pending, &navigation).unwrap(),
            }),
        ] {
            let invalid: SessionDomExtractParams = serde_json::from_value(invalid).unwrap();
            assert!(matches!(
                invalid.resolve(),
                Err(AutomationParamsError::InvalidOperation(_))
            ));
        }
    }

    #[test]
    fn session_document_result_adds_token_without_changing_legacy_projection() {
        let pending = pending_fixture();
        let navigation = navigation_fixture(&pending);
        let mut context = test_projection_context();
        let legacy = PendingWorkSnapshot::project(&pending, &mut context);
        let legacy_value = serde_json::to_value(&legacy).unwrap();
        assert!(legacy_value.get("stateToken").is_none());

        let session =
            SessionDocumentResult::new(legacy, &pending, &navigation, &mut context).unwrap();
        let session_value = serde_json::to_value(session).unwrap();
        assert_eq!(session_value["stateToken"], test_document_token(1));
        assert_eq!(
            session_value["stateGeneration"],
            ABOVE_JS_SAFE_INTEGER.to_string()
        );
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

        let too_many_fields: Vec<_> = (0..=public_automation_limits().max_extraction_fields())
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

        let filled = PublicAutomationResult::project(
            PublicAutomationKind::Fill,
            EngineDocumentAutomationResult::Filled,
            &raw,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(filled).unwrap(),
            json!({"stateGeneration": ABOVE_JS_SAFE_INTEGER.to_string()})
        );

        let checked = PublicAutomationResult::project(
            PublicAutomationKind::Check,
            EngineDocumentAutomationResult::Checked {
                changed: true,
                checked: true,
            },
            &raw,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(checked).unwrap(),
            json!({
                "changed": true,
                "checked": true,
                "stateGeneration": ABOVE_JS_SAFE_INTEGER.to_string(),
            })
        );

        let selected = PublicAutomationResult::project(
            PublicAutomationKind::Select,
            EngineDocumentAutomationResult::Selected {
                changed: false,
                values: vec!["primary".to_owned()],
            },
            &raw,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(selected).unwrap(),
            json!({
                "changed": false,
                "values": ["primary"],
                "stateGeneration": ABOVE_JS_SAFE_INTEGER.to_string(),
            })
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
                            value: Some("2".to_owned()),
                        },
                        EngineDocumentExtractionValue {
                            name: "first".to_owned(),
                            value: Some("1".to_owned()),
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
            Err(AutomationResultProjectionError::UnexpectedResult)
        );
    }

    #[test]
    fn worst_case_public_automation_frames_fit_the_protocol_budget() {
        let limits = public_automation_limits();
        let escaped_selector = "\0".repeat(limits.max_selector_bytes() as usize);
        let escaped_fill = "\0".repeat(limits.max_fill_value_bytes() as usize);
        let fill_request = json!({
            "v": 1,
            "type": "request",
            "id": "18446744073709551615",
            "sessionId": "s-1",
            "method": "action.fill",
            "params": {
                "selector": escaped_selector,
                "value": escaped_fill,
                "expectedGeneration": u64::MAX.to_string(),
            },
        });
        assert_frame_fits(&fill_request, "maximum escaped fill request");

        let extraction_fields: Vec<_> = (0..limits.max_extraction_fields())
            .map(|index| {
                let prefix = index.to_string();
                let name = format!(
                    "{prefix}{}",
                    "\0".repeat(limits.max_field_name_bytes() as usize - prefix.len()),
                );
                json!({
                    "name": name,
                    "selector": "\0".repeat(limits.max_selector_bytes() as usize),
                    "read": "html",
                })
            })
            .collect();
        let extract_request = json!({
            "v": 1,
            "type": "request",
            "id": "18446744073709551615",
            "sessionId": "s-1",
            "method": "dom.extract",
            "params": {
                "rootSelector": "\0".repeat(limits.max_selector_bytes() as usize),
                "fields": extraction_fields,
                "expectedGeneration": u64::MAX.to_string(),
            },
        });
        assert_frame_fits(&extract_request, "maximum escaped extraction request");

        let field_names: Vec<String> = (0..limits.max_extraction_fields())
            .map(|index| format!("{index}\0"))
            .collect();
        let mut rows: Vec<_> = (0..limits.max_matches())
            .map(|_| EngineDocumentExtractionRow {
                fields: field_names
                    .iter()
                    .map(|name| EngineDocumentExtractionValue {
                        name: name.clone(),
                        value: Some("\0".repeat(55)),
                    })
                    .collect(),
            })
            .collect();
        let logical_output_bytes: usize = rows
            .iter()
            .flat_map(|row| row.fields.iter())
            .map(|field| field.name.len() + field.value.as_ref().map_or(0, String::len))
            .sum();
        let remaining = limits.max_output_bytes() as usize - logical_output_bytes;
        rows[0].fields[0]
            .value
            .as_mut()
            .unwrap()
            .push_str(&"\0".repeat(remaining));
        let logical_output_bytes: usize = rows
            .iter()
            .flat_map(|row| row.fields.iter())
            .map(|field| field.name.len() + field.value.as_ref().map_or(0, String::len))
            .sum();
        assert_eq!(logical_output_bytes, limits.max_output_bytes() as usize);
        let result = PublicAutomationResult::project(
            PublicAutomationKind::Extract,
            EngineDocumentAutomationResult::Extract { rows },
            &pending_fixture(),
        )
        .unwrap();
        let extract_response = json!({
            "v": 1,
            "type": "response",
            "wireSeq": u128::MAX.to_string(),
            "id": "18446744073709551615",
            "sessionId": "s-1",
            "result": result,
        });
        assert_frame_fits(&extract_response, "maximum structured extraction response");
    }

    fn assert_frame_fits(frame: &serde_json::Value, label: &str) {
        let encoded = serde_json::to_vec(frame).unwrap();
        assert!(
            encoded.len() + 1 <= PUBLIC_AUTOMATION_FRAME_BUDGET_BYTES,
            "{label} encoded to {} bytes, exceeding the {}-byte frame budget",
            encoded.len() + 1,
            PUBLIC_AUTOMATION_FRAME_BUDGET_BYTES,
        );
    }

    #[test]
    fn exact_values_above_javascript_safe_integer_are_decimal_strings() {
        let mut context = test_projection_context();
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
    fn document_state_tokens_are_stable_for_exact_authority_and_never_aba() {
        let first = pending_fixture();
        let navigation = navigation_fixture(&first);
        let mut context = test_projection_context();

        let first_token = context.document_state_token(&first, &navigation).unwrap();
        assert_eq!(first_token.as_str(), test_document_token(1));
        assert_eq!(
            context.document_state_token(&first, &navigation).unwrap(),
            first_token
        );
        assert!(
            context
                .authorizes_document_state(&first, &navigation, &first_token)
                .unwrap()
        );

        let mut changed = first.clone();
        changed.state_generation = RuntimeStateGeneration::new(ABOVE_JS_SAFE_INTEGER + 1);
        let changed_token = context.document_state_token(&changed, &navigation).unwrap();
        assert_eq!(changed_token.as_str(), test_document_token(2));
        assert_ne!(changed_token, first_token);
        assert!(
            !context
                .authorizes_document_state(&changed, &navigation, &first_token)
                .unwrap()
        );

        // Returning to byte-identical engine authority still receives a new alias. Only the
        // current binding is retained, so a stale token cannot regain authority through ABA.
        let returned_token = context.document_state_token(&first, &navigation).unwrap();
        assert_eq!(returned_token.as_str(), test_document_token(3));
        assert_ne!(returned_token, first_token);
        assert!(
            !context
                .authorizes_document_state(&first, &navigation, &first_token)
                .unwrap()
        );
        assert!(
            context
                .authorizes_document_state(&first, &navigation, &returned_token)
                .unwrap()
        );
    }

    #[test]
    fn failed_document_authorization_does_not_rotate_the_latest_public_binding() {
        let first = pending_fixture();
        let navigation = navigation_fixture(&first);
        let mut context = test_projection_context();
        let first_token = context.document_state_token(&first, &navigation).unwrap();

        let mut progressed = first.clone();
        progressed.state_generation = RuntimeStateGeneration::new(ABOVE_JS_SAFE_INTEGER + 1);
        assert!(
            !context
                .authorizes_document_state(&progressed, &navigation, &first_token)
                .unwrap()
        );
        let retained = context
            .current_document_state_authority(&first_token)
            .expect("a failed strict authorization must retain the last issued capability");
        assert!(retained.matches_navigation(&navigation));
        assert_eq!(retained.target(), &first.target);
        assert_eq!(retained.state_generation(), first.state_generation);

        let contradictory_navigation = SessionNavigationAuthority::new_internal(
            Box::new(first.target.clone()),
            navigation.document_epoch(),
            navigation.navigation_id(),
            navigation.history_revision(),
            navigation.successful_document_replacements(),
            ServoUrl::parse("https://example.test/contradictory").unwrap(),
            navigation.terminal(),
        );
        assert!(!retained.matches_navigation(&contradictory_navigation));
        assert!(
            !context
                .authorizes_document_state(&first, &contradictory_navigation, &first_token)
                .unwrap()
        );
        assert!(
            context
                .current_document_state_authority(&first_token)
                .is_some()
        );

        let foreign: DocumentStateToken =
            serde_json::from_value(json!(test_document_token(99))).unwrap();
        let mut contradictory_pending = first.clone();
        contradictory_pending.target.navigation_revision = contradictory_pending
            .target
            .navigation_revision
            .checked_next()
            .unwrap();
        assert_eq!(
            context.authorizes_document_state(&contradictory_pending, &navigation, &foreign,),
            Ok(false),
            "a foreign token must fail before contradictory fresh authority is inspected",
        );
        assert!(
            context
                .current_document_state_authority(&first_token)
                .is_some()
        );

        assert!(
            !context
                .authorizes_document_state(&first, &navigation, &first_token)
                .unwrap(),
            "a strict-stale token must not regain authority after an ABA return",
        );

        context.next_document_state_alias = None;
        assert_eq!(
            context.document_state_token(&progressed, &navigation),
            Err(DocumentStateAuthorityError::TokenSpaceExhausted),
        );
        assert!(
            context
                .current_document_state_authority(&first_token)
                .is_some(),
            "failed fresh-token allocation must leave the settle capability intact",
        );
        assert!(
            !context
                .authorizes_document_state(&first, &navigation, &first_token)
                .unwrap(),
            "failed fresh-token allocation must leave strict invalidation intact",
        );
        context.next_document_state_alias = Some(2);

        let progressed_token = context
            .document_state_token(&progressed, &navigation)
            .unwrap();
        assert_eq!(progressed_token.as_str(), test_document_token(2));
        assert!(
            context
                .current_document_state_authority(&first_token)
                .is_none()
        );
    }

    #[test]
    fn settle_stale_latch_is_transactional_and_prevents_strict_aba_authorization() {
        let pending = pending_fixture();
        let navigation = navigation_fixture(&pending);
        let mut context = test_projection_context();
        let first_token = context.document_state_token(&pending, &navigation).unwrap();
        let first_authority = context
            .current_document_state_authority(&first_token)
            .expect("the current public token retains private authority");

        assert!(context.latch_current_document_state_strictly_invalidated(&first_authority));
        assert!(
            !context
                .authorizes_document_state(&pending, &navigation, &first_token)
                .unwrap(),
            "a byte-identical ABA observation must not revive a settle-rejected token",
        );

        let mut progressed = pending.clone();
        progressed.state_generation = RuntimeStateGeneration::new(ABOVE_JS_SAFE_INTEGER + 1);
        let progressed_token = context
            .document_state_token(&progressed, &navigation)
            .unwrap();
        let progressed_authority = context
            .current_document_state_authority(&progressed_token)
            .expect("the newly projected public token retains private authority");

        assert!(
            !context.latch_current_document_state_strictly_invalidated(&first_authority),
            "an old settle capability must not invalidate a newer public binding",
        );
        assert!(
            context
                .authorizes_document_state(&progressed, &navigation, &progressed_token)
                .unwrap(),
            "a failed old-authority latch must leave the newer token authorized",
        );
        assert!(context.latch_current_document_state_strictly_invalidated(&progressed_authority));

        let returned_token = context.document_state_token(&pending, &navigation).unwrap();
        assert_ne!(returned_token, first_token);
        assert_ne!(returned_token, progressed_token);
        assert!(
            !context.latch_current_document_state_strictly_invalidated(&first_authority),
            "an old A capability must not invalidate a newer A token after an A-B-A return",
        );
        assert!(
            context
                .authorizes_document_state(&pending, &navigation, &returned_token)
                .unwrap(),
            "the failed token1 latch must leave byte-identical token3 authorized",
        );
    }

    #[test]
    fn document_state_tokens_are_domain_separated_and_strict() {
        let wire = test_document_token(7);
        let token: DocumentStateToken = serde_json::from_value(json!(wire)).unwrap();
        assert_eq!(token.as_str(), test_document_token(7));
        assert_eq!(
            serde_json::to_value(&token).unwrap(),
            json!(test_document_token(7))
        );
        assert_eq!(format!("{token:?}"), "DocumentStateToken(<redacted>)");

        let maximum = test_document_token(u128::MAX);
        assert_eq!(maximum.len(), DOCUMENT_STATE_TOKEN_MAX_BYTES);
        let maximum_token: DocumentStateToken =
            serde_json::from_value(json!(maximum.clone())).unwrap();
        assert_eq!(maximum_token.as_str(), maximum);

        for rejected in [
            "session:61616161616161616161616161616161:7",
            "7",
            "document:6161616161616161616161616161616:7",
            "document:616161616161616161616161616161611:7",
            "document:6161616161616161616161616161616g:7",
            "document:6161616161616161616161616161616A:7",
            "document:61616161616161616161616161616161:0",
            "document:61616161616161616161616161616161:07",
            "document:61616161616161616161616161616161:+7",
            "document:61616161616161616161616161616161:-7",
            "document:61616161616161616161616161616161: 7",
            "document:61616161616161616161616161616161:340282366920938463463374607431768211456",
        ] {
            assert!(
                serde_json::from_value::<DocumentStateToken>(json!(rejected)).is_err(),
                "accepted invalid document token {rejected:?}",
            );
        }
    }

    #[test]
    fn document_state_token_space_exhaustion_is_checked() {
        let pending = pending_fixture();
        let navigation = navigation_fixture(&pending);
        let mut context = test_projection_context();
        context.next_document_state_alias = Some(u128::MAX);
        let first = context.document_state_token(&pending, &navigation).unwrap();
        assert_eq!(first.as_str(), test_document_token(u128::MAX));

        let mut changed = pending.clone();
        changed.state_generation = RuntimeStateGeneration::new(2);
        assert_eq!(
            context.document_state_token(&changed, &navigation),
            Err(DocumentStateAuthorityError::TokenSpaceExhausted),
        );
    }

    #[test]
    fn document_state_authority_rejects_a_token_from_another_fresh_session() {
        let pending = pending_fixture();
        let navigation = navigation_fixture(&pending);
        let mut first = test_projection_context();
        let mut second = WireProjectionContext::new_with_namespace_internal(
            OpaqueTokenNamespace::new_internal([0x62; 16]),
        );
        let foreign = first.document_state_token(&pending, &navigation).unwrap();
        let local = second.document_state_token(&pending, &navigation).unwrap();

        assert_ne!(foreign, local);
        assert!(
            !second
                .authorizes_document_state(&pending, &navigation, &foreign)
                .unwrap()
        );
        assert!(!format!("{foreign:?}").contains(TEST_DOCUMENT_NAMESPACE_HEX));
    }

    #[test]
    fn session_navigation_authority_rotates_tokens_and_projects_controlled_ready() {
        let pending = pending_fixture();
        let initial = navigation_fixture(&pending);
        let changed_history = SessionNavigationAuthority::new_internal(
            Box::new(pending.target.clone()),
            DocumentEpoch::new(2),
            SessionNavigationId::new(7),
            HistoryRevision::new(9),
            1,
            ServoUrl::parse("https://example.test/final").unwrap(),
            None,
        );
        let mut context = test_projection_context();
        let initial_token = context.document_state_token(&pending, &initial).unwrap();
        let changed_token = context
            .document_state_token(&pending, &changed_history)
            .unwrap();
        assert_ne!(initial_token, changed_token);
        assert!(
            !context
                .authorizes_document_state(&pending, &changed_history, &initial_token)
                .unwrap()
        );

        let value = serde_json::to_value(
            SessionNavigateResult::project(
                "https://example.test/requested".into(),
                &pending,
                &changed_history,
                &mut context,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(value["requestedUrl"], "https://example.test/requested");
        assert_eq!(value["url"], "https://example.test/final");
        assert_eq!(value["boundary"], "controlled_ready");
        assert_eq!(value["documentEpoch"], "2");
        assert_eq!(value["navigationId"], "7");
        assert_eq!(value["historyRevision"], "9");
        assert_eq!(value["stateToken"], changed_token.as_str());
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
        let mut context = test_projection_context();
        let before =
            serde_json::to_value(RuntimePendingResult::project(&raw, &mut context)).unwrap();
        let surviving_id = before["sources"][1]["sourceId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            !surviving_id.is_empty()
                && surviving_id.bytes().all(|byte| byte.is_ascii_digit())
                && (surviving_id == "0" || !surviving_id.starts_with('0'))
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
    fn render_blockers_project_as_exact_unsupported_work() {
        let pipeline_id = PipelineId {
            namespace_id: PipelineNamespaceId(82),
            index: Index::<PipelineIndex>::new(1).unwrap(),
        };
        let rendering = PendingPipelineRenderingObservation {
            pipeline_id,
            activity: PendingRenderingPipelineActivity::FullyActive,
            render_blocking_elements: 2,
            retained_animation_frame_callbacks: 0,
            runnable_animation_frame_callbacks: 0,
            document_update_required: false,
            pending_animation_events: 0,
            finite_animations: 0,
            infinite_animations: 0,
            unsupported_animations: 0,
            animated_images: Default::default(),
            canvas: Default::default(),
            pending_fonts: 0,
            pending_images: 0,
        };

        let projected = project_unsupported_rendering(&rendering);
        assert_eq!(projected.len(), 1);
        let projected = serde_json::to_value(&projected[0]).unwrap();
        assert_eq!(projected["kind"], "rendering_update");
        assert_eq!(projected["count"], "2");
        assert_eq!(projected["reason"], "render_blocking_element");
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
            (DocumentTimeSurface::HistoryTraversal, "history_traversal"),
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
        let mut context = test_projection_context();
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
        let processed = settled["processed"].as_object().unwrap();
        assert_eq!(processed.len(), 5);
        assert_eq!(processed["tasks"], "0");
        assert_eq!(processed["microtasks"], "0");
        assert_eq!(processed["renderingOpportunities"], "0");
        assert_eq!(processed["mutations"], "0");
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

        let (outcome, controlled_ready_details) = project_controlled_ready_failure_details(
            &SettleRuntimeFailure::UnsupportedSource(raw.sources.sources()[1]),
            &raw,
        );
        assert_eq!(outcome, SettleOutcome::UnsupportedWork);
        let controlled_ready_details = serde_json::to_value(controlled_ready_details).unwrap();
        assert_eq!(
            controlled_ready_details["failure"]["code"],
            "unsupported_source"
        );
        assert_eq!(
            controlled_ready_details["unsupportedWork"][0]["kind"],
            "other"
        );
        assert_eq!(
            controlled_ready_details["unsupportedWork"][0]["reason"],
            "time_surface"
        );
        assert_eq!(
            controlled_ready_details["unsupportedWork"][0]["timeSurface"],
            "worker"
        );
        assert!(
            controlled_ready_details["unsupportedWork"][0]
                .get("sourceId")
                .is_none()
        );

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
    fn execution_limits_project_typed_outcomes_and_processed_counts() {
        for (budget, outcome, kind, processed_key) in [
            (
                DocumentExecutionBudget::OrdinaryTasks,
                "task_limit_exceeded",
                "ordinary_tasks",
                "tasks",
            ),
            (
                DocumentExecutionBudget::Microtasks,
                "microtask_limit_exceeded",
                "microtasks",
                "microtasks",
            ),
            (
                DocumentExecutionBudget::RenderingOpportunities,
                "rendering_limit_exceeded",
                "rendering_opportunities",
                "renderingOpportunities",
            ),
            (
                DocumentExecutionBudget::MutationRecords,
                "mutation_limit_exceeded",
                "mutations",
                "mutations",
            ),
        ] {
            let mut raw = pending_fixture();
            let mut limits = DocumentExecutionLimits {
                ordinary_tasks: 100,
                microtasks: 100,
                rendering_opportunities: 100,
                mutations: 100,
            };
            let mut counters = DocumentExecutionCounters::default();
            match budget {
                DocumentExecutionBudget::OrdinaryTasks => {
                    limits.ordinary_tasks = 3;
                    counters.ordinary_tasks = 3;
                },
                DocumentExecutionBudget::Microtasks => {
                    limits.microtasks = 3;
                    counters.microtasks = 3;
                },
                DocumentExecutionBudget::RenderingOpportunities => {
                    limits.rendering_opportunities = 3;
                    counters.rendering_opportunities = 3;
                },
                DocumentExecutionBudget::MutationRecords => {
                    limits.mutations = 3;
                    counters.mutations = 4;
                },
            }
            raw.execution = Some(DocumentExecutionObservation {
                clock_id: raw.clock.clock_id,
                limits,
                counters,
                terminal: Some(DocumentExecutionTerminal::BudgetExceeded {
                    budget,
                    limit: 3,
                    observed: 4,
                }),
            });
            raw.validate().unwrap();

            let mut context = test_projection_context();
            let projected = RuntimeSettleResult::project(
                SettleCompletion::ExecutionLimitExceeded {
                    pending: Box::new(raw),
                    budget,
                    limit: 3,
                    observed: 4,
                    control_turns: 2,
                },
                Duration::from_nanos(9),
                RuntimeSettleParams::default()
                    .resolve(EngineSettlePolicy::default())
                    .unwrap(),
                &mut context,
            );
            let projected = serde_json::to_value(projected).unwrap();
            assert_eq!(projected["outcome"], outcome);
            assert_eq!(projected["limit"]["kind"], kind);
            assert_eq!(projected["limit"]["limit"], "3");
            assert_eq!(projected["limit"]["observed"], "4");
            let processed = if budget == DocumentExecutionBudget::MutationRecords {
                "4"
            } else {
                "3"
            };
            assert_eq!(projected["processed"][processed_key], processed);
        }
    }

    #[test]
    fn golden_projection_never_exposes_engine_authority_or_urls() {
        let raw = pending_fixture();
        let mut context = test_projection_context();
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
                                .any(|fragment| normalized.contains(fragment))
                                && !normalized.contains("url"),
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

    #[test]
    fn controlled_web_session_v2_profile_is_an_explicit_execution_and_presentation_expansion() {
        let profile_bytes = include_bytes!("../../../profiles/controlled-web-session-v2.json");
        assert_eq!(
            Sha256::digest(profile_bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "ced49928c0c5f77669285a658434209101d27907bd26d07296d5d40e2ad7a412",
            "the candidate test must be closed over every advertised profile field",
        );
        let profile: Value = serde_json::from_slice(profile_bytes)
            .expect("the controlled-web-session-v2 candidate profile must be valid JSON");

        assert_eq!(profile["schemaVersion"], 1);
        assert_eq!(profile["id"], "controlled-web-session-v2");
        assert_eq!(profile["compatibility"]["predecessor"], "controlled-web-session-v1");
        assert_eq!(
            profile["compatibility"]["predecessorProfileSha256"],
            "9b62b9245b2c6a6f9620b117da6787a18df9298be1115cbce2e6c3d5439cc41a"
        );
        assert_eq!(profile["compatibility"]["predecessorContractUnchanged"], true);
        assert_eq!(
            profile["compatibility"]["profileExpansion"],
            "execution_and_headless_presentation_surfaces_only"
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["construction"]["interface"],
            "MessageChannel"
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["construction"]
                ["maximumRetainedNativePortEntriesPerGlobal"],
            32
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["construction"]["capacityUnit"],
            "retained_native_port_entry"
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["construction"]
                ["completePairCapacityFromEmptyGlobal"],
            16
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["construction"]
                ["completePairCapacityCondition"],
            "no_one_ended_terminal_identities_retained"
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["construction"]
                ["oneEndedTerminalIdentityCapacity"],
            "each_retained_identity_consumes_one_of_32_entries_and_reduces_available_complete_pair_capacity"
        );
        assert!(
            profile["execution"]["messageChannel"]["construction"]
                .get("maximumRetainedPairsPerGlobal")
                .is_none(),
            "pair capacity must not be advertised independently of retained entry shape",
        );
        assert_eq!(
            profile["execution"]["messageChannel"]["delivery"]["retainedWorkProjection"],
            json!({
                "reservationIdentity": "exact_destination_port_before_retention_in_ordinary_task_queue_or_native_disabled_port_buffer",
                "accountingReconciliation": "global_retained_equals_sum_per_destination_queued_plus_sum_native_buffered",
                "reciprocalPairWithOwnedWork": "one_deterministic_minimum_port_identity_per_pair",
                "zeroRetainedMessages": "does_not_make_idle_open_pair_pending",
                "invalidMissingOrZeroDestinationAssociation": "pending_observation_failure",
            })
        );
        assert_eq!(
            profile["execution"]["controlledInputMethodFocus"],
            json!({
                "scope": "exact_public_controlled_non_auxiliary_top_level_WebView_document_global",
                "request": "page_driven_InputMethod_Text_nonmultiline_allowVirtualKeyboard_false_only",
                "trigger": "page_driven_programmatic_DOM_focus_including_React_autoFocus",
                "semanticAutomation": "preexisting_profile_independent_suppression_unchanged",
                "domSemantics": "focus_events_value_and_selection_preserved",
                "embedderPresentation": "suppressed_before_time_surface_admission",
                "visibleOwner": "not_published",
                "embedderRequest": "not_sent",
                "callback": "not_created_or_awaited",
                "pendingAuthority": "no_external_work_created",
                "otherEmbedderControls": "unchanged_unsupported_boundaries",
                "predecessorBehavior": "controlled_web_session_v1_unchanged",
            })
        );
        assert_eq!(
            profile["execution"]["controlledFocusEventTimestamp"],
            json!({
                "scope": "exact_public_controlled_non_auxiliary_top_level_WebView_document_global",
                "events": ["focus", "blur", "focusin", "focusout"],
                "creation": "engine_generated_document_focus_transition_only",
                "clock": "document_performance_clock_sampled_at_event_creation",
                "observableValue": "Event_timeStamp_equals_document_relative_performance_time",
                "hostValue": "sampled_implementation_value_is_overwritten_and_not_observable",
                "scriptCreatedFocusEvent": "host_timestamp",
                "otherEventsOutsideControlledAutomationScope": "host_timestamp",
                "predecessorBehavior": "controlled_web_session_v1_unchanged",
                "realtimeBehavior": "unchanged",
            })
        );
        assert_eq!(
            profile["execution"]["controlledAutomationEventTimestamps"],
            json!({
                "scope": "active_controlled_top_level_document_global",
                "automationScope": "synchronous_public_mutating_automation_action_only",
                "clock": "document_performance_clock_sampled_once_before_mutation",
                "lifetime": "RAII_scope_restored_before_action_response",
                "coverage": "every_browser_created_event_constructed_synchronously_during_the_admitted_action",
                "implementationSeams": {
                    "fillInputEvent": "explicit_internal_fill_InputEvent_stamp",
                    "internalPointerEvent": "internal_PointerEvent_new_stamp",
                    "genericEventTargetFire": "browser_created_simple_Event_stamp",
                    "internalSubmitEvent": "internal_SubmitEvent_new_stamp",
                    "internalFormDataEvent": "internal_FormDataEvent_new_stamp",
                },
                "representativeProofEvents": "fill_input_activate_click_reset_check_click_input_change_select_input_change_invalid_submit_formdata",
                "observableValue": "all_browser_created_events_synchronously_constructed_during_one_admitted_action_share_its_document_relative_timestamp",
                "samplingFailure": "reject_action_before_mutation_without_host_fallback",
                "scriptCreatedConstructors": "Event_InputEvent_PointerEvent_SubmitEvent_FormDataEvent_remain_host_timestamp",
                "genericEventConstructor": "Event_new_inherited_unchanged",
                "predecessorBehavior": "controlled_web_session_v1_unchanged",
                "nestedAndRealtimeBehavior": "unchanged",
            })
        );
        assert_eq!(
            profile["execution"]["controlledCssAnimationEventTimestamps"],
            json!({
                "scope": "exact_public_controlled_non_auxiliary_top_level_WebView_document_global",
                "source": "nonempty_Animations_pending_event_dispatch_batch_already_retained_by_document_rendering_authority",
                "eventKinds": [
                    "animationstart",
                    "animationiteration",
                    "animationend",
                    "animationcancel",
                    "transitionrun",
                    "transitionstart",
                    "transitionend",
                    "transitioncancel",
                ],
                "clock": "document_performance_clock_sampled_once_before_pending_queue_take",
                "targetAdmission": "ScriptThread_current_controlled_top_level_target_matches_conservative_singleton_reconstruction_with_undiscarded_non_auxiliary_WindowProxy",
                "recordAdmission": "queued_pipeline_and_rooted_node_owner_match_exact_public_controlled_non_auxiliary_top_level_target_and_fully_active_Document",
                "construction": "internal_AnimationEvent_and_TransitionEvent_timestamp_overwrite_immediately_before_fire",
                "observableValue": "every_admitted_internal_event_in_one_nonempty_dispatch_batch_shares_its_document_relative_timestamp",
                "samplingFailure": "latch_controlled_clock_terminal_and_leave_batch_undispatched_without_host_fallback",
                "pendingAuthority": "existing_pending_event_and_finite_infinite_unsupported_animation_rendering_facts_unchanged",
                "settlementScheduling": {
                    "scheduledPendingEventBatch": "finite_rendering_demand_advanced_to_exact_retained_scheduler_head_including_deadline_equal_to_now",
                    "driveReadiness": "pending_animation_events_are_Drive_ready_only_without_a_live_scheduled_opportunity",
                    "reason": "Drive_cannot_detach_a_controlled_scheduler_entry",
                    "surfaceEffect": "liveness_correction_only_no_new_producer_task_source_or_execution_limit",
                },
                "executionLimit": "existing_10000_rendering_opportunity_limit",
                "representativeExecutableProof": "instant_finite_animationstart_and_animationend_only",
                "transitionSettlementCompatibility": "not_claimed_timestamp_adapter_applies_only_if_an_existing_owned_transition_record_reaches_pending_dispatch",
                "scriptCreatedConstructors": "AnimationEvent_and_TransitionEvent_remain_host_timestamp",
                "auxiliaryStaleMismatchedNestedAndRealtime": "host_timestamp_predecessor_behavior",
                "semanticBoundary": "timestamp_only_event_order_cardinality_elapsedTime_and_CSS_animation_semantics_unchanged",
                "predecessorBehavior": "controlled_web_session_v1_unchanged",
            })
        );
        assert_eq!(
            profile["execution"]["controlledImageElement"],
            json!({
                "mode": "controlled_top_level_direct_data_svg",
                "selection": {
                    "interface": "HTMLImageElement",
                    "scope": "exact_public_controlled_non_auxiliary_top_level_WebView_document_global",
                    "source": "direct_src_selected_without_srcset_picture_or_environment_change",
                    "parser": "canonical_DataUrl",
                    "mimeType": "image/svg+xml",
                    "maximumSerializedUrlBytes": 65536,
                    "requestProvenance": "captured_at_selection_and_carried_with_request_generation",
                    "retainedVectorAuthority": "controlled_cache_id_stored_on_request_only_after_successful_registration_or_synchronous_exact_owner_retain",
                    "executionDomain": "same_ScriptThread_and_ImageCache",
                },
                "retention": {
                    "maximumRetainedControlledOwnershipRecordsPerWindow": 512,
                    "recordKinds": [
                        "pending_callback",
                        "exact_cache_id_DOM_owner_identity",
                        "vector_rasterization_key",
                    ],
                    "reservationUnit": "one_record_per_controlled_pending_callback_exact_cache_id_DOM_owner_identity_or_vector_rasterization_key",
                    "overflow": "sticky_Image_producer_admission_limit_terminal_without_baseline_fallback",
                    "decodeRequestAdmission": "ReadyForRequest_callback_and_identity_reservations_succeed_before_cache_request_issue",
                    "teardown": "callback_identity_layout_and_raster_collections_cleared_together_releasing_all_records",
                },
                "completion": {
                    "synchronousCacheHit": "admitted_provenance_bound_current_turn_without_async_producer_lease",
                    "asyncCacheDecode": "Image_producer_fenced_through_ScriptThread_handoff",
                    "callbackRetirement": "cache_owned_callback_drop_before_protocol_terminal_is_owned_cancellation_completed_without_producer_terminal",
                    "retiredTargetDelivery": "dequeued_after_navigation_with_closed_pipeline_tombstone_and_without_live_Window_is_owned_cancellation_completed_without_producer_terminal",
                    "retainedHandlerRejection": "normal_handler_Err_preserves_rejected_key_or_owner_in_Window_pending_collections_completes_scoped_message_guard_and_settles_as_unsupported_rendering_image_load",
                    "handlerUnwind": "ControlledImageMessageCompletion_abandons_during_unwind_and_completes_every_normal_handler_return",
                    "explicitAbandonment": "message_admission_failure_enqueue_rejection_producer_callback_panic_ScriptThread_handler_unwind_missing_untombstoned_or_live_tombstoned_target_prehandler_profile_or_exact_public_target_mismatch_clock_sampling_failure_or_guarded_transport_loss_latches_sticky_Image_producer_terminal",
                    "leaseClassMatch": "completion_and_abandonment_require_exact_fence_sequence_and_registered_Image_kind_before_terminal_or_watermark_mutation",
                    "vectorRasterization": "fenced_only_when_joined_from_a_retained_exact_cache_id_DOM_owner_identity",
                    "vectorRasterizationStart": "may_begin_in_layout_before_post_reflow_exact_key_reservation",
                    "vectorRasterizationAdmission": "post_reflow_exact_key_reservation_and_fenced_listener_install_before_next_ScriptThread_pending_snapshot_publish_or_observe",
                    "vectorRasterizationCapacityFailure": "sticky_Image_producer_terminal_without_baseline_fallback_even_if_task_already_started",
                    "terminalResponses": [
                        "loaded",
                        "failed_to_load_or_decode",
                        "vector_rasterization_complete",
                    ],
                    "queuedDomCallback": "ordinary_task_after_guarded_handoff_with_request_generation_check",
                    "preHandlerMismatchOrAbandonment": "sticky_producer_terminal_without_baseline_fallback",
                    "requestAuthorityLifecycle": "pending_to_current_move_preserves_exact_cache_id_and_abort_replace_or_different_id_releases_exact_owner",
                    "sameIdAbaProtection": "stale_generation_releases_only_when_neither_request_slot_owns_exact_cache_id",
                    "decoderResourceBudget": "not_claimed_existing_wall_task_and_rendering_limits_only",
                },
                "pending": {
                    "logicalIdentity": "union_of_callback_and_layout_PendingImageId_plus_exact_image_id_size_rasterization_keys",
                    "layoutOwnerProvenance": "captured_per_exact_cache_id_DOM_owner_at_first_post_reflow_retention",
                    "controlledClassification": "image_id_controlled_only_when_every_retained_callback_is_controlled_and_no_retained_layout_owner_is_baseline",
                    "mixedLayoutOwnership": "baseline_layout_owner_globally_downgrades_cache_id_and_live_raster_keys_and_delivery_mismatch_rejects_before_any_callback_while_retained_as_unsupported_rendering",
                    "mixedMissingOrBaseline": "unsupported_pending_rendering_image_load",
                    "controlledProjection": "Image_producer_fence_not_pending_rendering_image_load",
                    "reservationReconciliation": "live_controlled_records_equal_retained_controlled_callbacks_plus_exact_cache_id_DOM_owner_identities_plus_controlled_rasterization_keys",
                    "producerReconciliation": "pending_Image_producers_greater_than_or_equal_to_controlled_logical_work_absent_terminal",
                },
                "eventTimestamp": {
                    "events": ["load", "error", "loadend"],
                    "creation": "engine_generated_HTMLImageElement_completion_only",
                    "clock": "document_performance_clock_sampled_once_per_completion",
                    "observableValue": "every_event_emitted_for_one_completion_shares_the_document_relative_timestamp",
                    "ordinaryTerminalCardinality": "load_then_loadend_or_error_then_loadend",
                    "existingCacheHitCardinality": "load_only",
                    "cacheHit": "sampled_before_queued_DOM_manipulation_task_and_carried_with_request_generation",
                    "async": "sampled_at_guarded_ScriptThread_delivery_and_carried_through_queued_callback",
                    "hostFallback": "forbidden_for_admitted_work",
                    "predecessorBehavior": "controlled_web_session_v1_unchanged",
                },
                "unsupported": {
                    "httpHttpsBlobFileAndNonSvgDataUrls": "not_admitted_baseline_image_authorities_unchanged",
                    "oversizeUrl": "not_admitted_baseline_image_authorities_unchanged",
                    "srcsetPictureAndEnvironmentChange": "not_admitted_baseline_image_authorities_unchanged",
                    "cssBackgroundListStyleAndContent": "not_admitted_unless_joining_a_retained_exact_cache_id_DOM_owner_identity",
                    "faviconAndVideoPoster": "not_admitted_baseline_image_authorities_unchanged",
                    "imageBitmapAndCanvasUpload": "not_admitted_baseline_image_authorities_unchanged",
                    "animatedImages": "not_admitted_by_this_slice_existing_rendering_authority_unchanged",
                    "iframeWorkerWorkletAndCrossLoop": "not_admitted_existing_context_boundaries_unchanged",
                    "unadmittedSharedVectorCacheIdentity": "remove_all_controlled_owners_and_downgrade_live_raster_keys_to_baseline",
                    "nestedOrExternalSvgResources": "not_content_inspected_not_proven_by_this_slice",
                },
            })
        );
        assert_eq!(
            profile["execution"]["controlledInlineSvgRendering"],
            json!({
                "mode": "controlled_top_level_internal_serialized_data_svg",
                "admission": {
                    "interface": "SVGSVGElement",
                    "scope": "exact_public_controlled_non_auxiliary_top_level_WebView_document_global",
                    "source": "internally_serialized_inline_svg_subtree_only",
                    "requestKind": "InternalRequest_Yes",
                    "cachedUrlIdentity": "candidate_exactly_equals_element_cached_serialized_data_url",
                    "parser": "canonical_DataUrl",
                    "mimeType": "image/svg+xml",
                    "maximumSerializedUrlBytes": 65536,
                    "executionDomain": "same_ScriptThread_and_ImageCache",
                },
                "ownership": {
                    "cacheIdJoin": "exact_PendingImageId_DOM_owner_identity_required",
                    "retentionBudget": "shared_512_record_controlled_image_ownership_limit",
                    "mixedOwnership": "baseline_owner_globally_downgrades_shared_cache_id_and_live_raster_keys",
                    "hostFallback": "forbidden_for_admitted_work",
                },
                "completion": {
                    "asyncCacheDecode": "Image_producer_fenced_through_ScriptThread_handoff",
                    "vectorRasterization": "fenced_only_from_exact_retained_inline_svg_cache_id_DOM_owner_identity",
                    "pendingProjection": "Image_producer_fence_not_pending_rendering_image_load",
                    "domLoadEvent": "not_emitted_by_internal_inline_svg_rendering_completion",
                },
                "unsupported": {
                    "baselineAndV1": "unchanged",
                    "generalSvgRendering": "not_admitted_by_this_slice",
                    "nestedOrExternalResources": "not_admitted_or_proven_existing_resource_authority_unchanged",
                    "nonInternalOrMismatchedUrl": "baseline_image_authorities_unchanged",
                    "iframeWorkerWorkletAndCrossLoop": "not_admitted_existing_context_boundaries_unchanged",
                },
            })
        );
        assert_eq!(
            profile["unsupportedClasses"]["embedderControls"],
            json!({
                "controlledTopLevelSingleLineTextInputMethodPresentationWithoutVirtualKeyboard":
                    "suppressed_without_external_work",
                "selectElementColorPickerFilePickerContextMenuAndOtherControls":
                    "embedder_control",
            })
        );
        let input_method_product_surfaces: Vec<_> = profile["supportedProductSurface"]
            .as_array()
            .expect("supportedProductSurface must be an array")
            .iter()
            .filter_map(Value::as_str)
            .filter(|surface| surface.contains("input_method"))
            .collect();
        assert_eq!(
            input_method_product_surfaces,
            [
                "controlled_top_level_single_line_text_input_method_presentation_suppression_without_virtual_keyboard",
            ]
        );
        let focus_timestamp_product_surfaces: Vec<_> = profile["supportedProductSurface"]
            .as_array()
            .expect("supportedProductSurface must be an array")
            .iter()
            .filter_map(Value::as_str)
            .filter(|surface| surface.contains("focus_event_timestamp"))
            .collect();
        assert_eq!(
            focus_timestamp_product_surfaces,
            ["controlled_top_level_engine_focus_event_timestamp_from_document_clock"]
        );
        let automation_timestamp_product_surfaces: Vec<_> = profile["supportedProductSurface"]
            .as_array()
            .expect("supportedProductSurface must be an array")
            .iter()
            .filter_map(Value::as_str)
            .filter(|surface| surface.contains("synchronous_public_automation_event_timestamps"))
            .collect();
        assert_eq!(
            automation_timestamp_product_surfaces,
            [
                "controlled_top_level_synchronous_public_automation_event_timestamps_from_document_clock"
            ]
        );
        let css_animation_timestamp_product_surfaces: Vec<_> = profile["supportedProductSurface"]
            .as_array()
            .expect("supportedProductSurface must be an array")
            .iter()
            .filter_map(Value::as_str)
            .filter(|surface| surface.contains("internal_CSS_animation_event_timestamps"))
            .collect();
        assert_eq!(
            css_animation_timestamp_product_surfaces,
            [
                "controlled_public_non_auxiliary_top_level_internal_CSS_animation_event_timestamps_from_document_clock"
            ]
        );
        let image_product_surfaces: Vec<_> = profile["supportedProductSurface"]
            .as_array()
            .expect("supportedProductSurface must be an array")
            .iter()
            .filter_map(Value::as_str)
            .filter(|surface| surface.contains("data_svg_HTMLImageElement"))
            .collect();
        assert_eq!(
            image_product_surfaces,
            ["bounded_controlled_top_level_direct_data_svg_HTMLImageElement_completion"]
        );
        let inline_svg_product_surfaces: Vec<_> = profile["supportedProductSurface"]
            .as_array()
            .expect("supportedProductSurface must be an array")
            .iter()
            .filter_map(Value::as_str)
            .filter(|surface| surface.contains("serialized_data_svg_inline_rendering"))
            .collect();
        assert_eq!(
            inline_svg_product_surfaces,
            ["bounded_controlled_top_level_internal_serialized_data_svg_inline_rendering"]
        );
        assert_eq!(
            profile["unsupportedClasses"]["hostTimestamp"],
            json!({
                "controlledTopLevelEngineGeneratedFocusTransitionInV2":
                    "document_clock_timestamp",
                "controlledTopLevelAdmittedImageCompletionEventsInV2":
                    "shared_document_clock_timestamp",
                "controlledTopLevelSynchronousPublicAutomationEventsInV2":
                    "one_document_clock_timestamp_per_mutating_action",
                "controlledPublicNonAuxiliaryTopLevelInternalCssAnimationAndTransitionEventsInV2":
                    "one_document_clock_timestamp_per_nonempty_pending_event_dispatch_batch",
                "scriptCreatedEventConstructorsAndAllUnlistedHostTimestampSurfaces": "host_timestamp",
            })
        );
        assert_eq!(
            profile["unsupportedClasses"]["imageElement"],
            json!({
                "admittedDirectDataSvg": "owned_bounded_Image_producer_work",
                "baselineMixedOrUnownedRetainedWork": "unsupported_rendering_image_load",
                "excludedSynchronousCacheHit": "predecessor_behavior_no_universal_new_typed_rejection",
                "nestedOrExternalSvgResources": "not_proven",
            })
        );
        // SessionStateV1 is intentionally the portable artifact for both runtime profiles. Its
        // frozen state.profile identity remains controlled-web-session-v1.
        assert_eq!(
            profile["compatibility"]["stateArtifactProfile"],
            "controlled-web-session-v1"
        );
        assert_eq!(
            profile["sessionState"]["artifactProfile"],
            "controlled-web-session-v1"
        );
        assert_eq!(
            profile["sessionState"]["compatibleSelectedProfiles"],
            json!(["controlled-web-session-v1", "controlled-web-session-v2"])
        );
    }

    #[test]
    fn controlled_web_session_profile_matches_public_numeric_contracts() {
        use crate::session_state::{
            MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES, MAX_SESSION_COOKIE_BYTES, MAX_SESSION_COOKIES,
            MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST, MAX_SESSION_STATE_BYTES,
            MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES, MAX_SESSION_STORAGE_BYTES_PER_ORIGIN,
            MAX_SESSION_STORAGE_ENTRIES_PER_AREA, MAX_SESSION_STORAGE_KEY_BYTES,
            MAX_SESSION_STORAGE_ORIGINS, MAX_SESSION_STORAGE_VALUE_BYTES,
            SESSION_STATE_SCHEMA_VERSION_V1, SessionStateToken, WireU64,
        };
        use net_traits::{
            CONTROLLED_COOKIE_MAX_BATCH_VALUES_V1, CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1,
            COOKIE_STATE_MAX_COOKIE_BYTES_V1, COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1,
            COOKIE_STATE_MAX_COOKIES_V1, COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
            COOKIE_STATE_MAX_TOTAL_BYTES_V1, network_evidence::MAX_EVIDENCE_METHOD_BYTES,
        };
        use storage_traits::webstorage_thread::{
            WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1, WEB_STORAGE_STATE_MAX_KEY_BYTES_V1,
            WEB_STORAGE_STATE_MAX_ORIGIN_BYTES_V1, WEB_STORAGE_STATE_MAX_ORIGINS_V1,
            WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1, WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1,
        };

        let profile: Value = serde_json::from_str(include_str!(
            "../../../profiles/controlled-web-session-v1.json"
        ))
        .expect("the frozen controlled-web-session-v1 profile must be valid JSON");
        assert_eq!(profile["schemaVersion"], 1);
        assert_eq!(SESSION_STATE_SCHEMA_VERSION_V1, 1);
        assert_eq!(profile["id"], "controlled-web-session-v1");
        assert_eq!(profile["releaseStatus"], "stable_contract");
        assert_eq!(profile["targetRelease"], "0.2.0");
        assert_eq!(profile["compatibility"]["maximumOpaqueTokenBytes"], 256);
        assert_eq!(profile["documentAuthority"]["namespaceBits"], 128);
        assert_eq!(
            profile["documentAuthority"]["rotationLinearization"],
            "next_successful_public_token_projection_after_a_bound_fact_change"
        );
        assert_eq!(
            profile["documentAuthority"]["strictAuthorization"]["foreignOrSupersededToken"],
            "reject_before_fresh_authority_inspection_without_changing_the_current_binding"
        );
        assert_eq!(
            profile["documentAuthority"]["strictAuthorization"]["currentMismatch"],
            "retain_latest_public_binding_but_latch_it_strict_stale_without_hidden_token_issuance"
        );
        assert_eq!(
            profile["documentAuthority"]["strictAuthorization"]["abaReturn"],
            "remains_strict_stale_until_a_fresh_public_token_is_successfully_issued"
        );
        assert_eq!(
            profile["documentAuthority"]["strictAuthorization"]["failedFreshTokenAllocation"],
            "latest_public_binding_and_strict_stale_latch_remain_unchanged"
        );
        let settle_continuation = &profile["documentAuthority"]["settleContinuation"];
        assert_eq!(settle_continuation["method"], "runtime.settle");
        assert_eq!(
            settle_continuation["eligibleToken"],
            "exact_latest_publicly_issued_binding_including_a_strict_stale_latch"
        );
        assert_eq!(
            settle_continuation["scope"],
            "same_document_only_no_cross_document_successor"
        );
        assert_eq!(
            settle_continuation["authorizationBracket"],
            "pump_suppressed_passive_N1_then_document_observe_D_then_pump_suppressed_passive_N2"
        );
        assert_eq!(
            settle_continuation["navigationAuthority"],
            "exact_full_N1_equals_N2_and_equals_retained_authority"
        );
        assert_eq!(
            settle_continuation["runtimeStateGeneration"],
            "D_generation_greater_than_or_equal_to_retained_generation"
        );
        assert_eq!(
            settle_continuation["nearMiss"],
            "validated_nonterminal_N1_D_or_N2_authority_mismatch_latches_the_exact_current_binding_then_stale_state_token_nonfatal_state_effect_none_before_coordinator_start"
        );
        assert_eq!(
            settle_continuation["documentObservationRejection"],
            "validated_target_changed_or_replacement_pipeline_bootstrap_required_is_a_near_miss_invalid_payload_is_fatal"
        );
        assert_eq!(
            settle_continuation["terminalPrecedence"],
            "typed_session_navigation_terminal_or_application_failure_precedes_stale_state_token_at_N1_or_N2_and_never_starts_the_coordinator"
        );
        assert_eq!(
            settle_continuation["coordinatorSeed"],
            "the_exact_completed_D_observation_through_the_normal_initial_observe_transition"
        );
        assert_eq!(
            settle_continuation["knownStalePreflight"],
            "sticky_controlled_network_failure_precedes_stale_rejection_otherwise_no_engine_work_or_pump"
        );
        assert_eq!(
            profile["documentAuthority"]["recoveryMethod"],
            "runtime.pending"
        );
        assert_eq!(
            profile["automation"]["linearization"]["strictMethods"],
            "script_owner_revalidates_exact_document_identity_and_generation_before_execution"
        );
        assert_eq!(
            profile["automation"]["linearization"]["runtime.settle"],
            "documentAuthority.settleContinuation"
        );
        assert_eq!(profile["sessionStateAuthority"]["namespaceBits"], 128);
        assert_eq!(
            profile["sessionStateAuthority"]["successfulMutationMethods"],
            json!(["session.cookies.set", "session.storage.set"])
        );
        assert_eq!(
            profile["sessionStateAuthority"]["successfulMutationsRequireExpectedSessionStateToken"],
            true
        );
        assert_eq!(
            profile["sessionStateAuthority"]["initialImport"]["method"],
            "session.open.state"
        );
        assert_eq!(
            profile["sessionStateAuthority"]["initialImport"]["onlySuccessfulEntryPoint"],
            true
        );
        assert_eq!(
            profile["sessionStateAuthority"]["initialImport"]["requiresCallerExpectedSessionStateToken"],
            false
        );
        assert_eq!(
            profile["sessionStateAuthority"]["initialImport"]["authorization"],
            "unpublished_builder_hidden_current_token"
        );
        assert_eq!(
            profile["sessionStateAuthority"]["postPublicationImport"]["method"],
            "session.state.import"
        );
        assert_eq!(
            profile["sessionStateAuthority"]["postPublicationImport"]["successfulMutation"],
            false
        );
        assert_eq!(
            profile["sessionStateAuthority"]["postPublicationImport"]["requestPayloadInspected"],
            false
        );
        assert_eq!(profile["automation"]["fill"]["inputEventBubbles"], true);
        assert_eq!(profile["automation"]["fill"]["inputEventComposed"], true);
        assert_eq!(profile["automation"]["fill"]["inputEventCancelable"], false);
        assert!(
            profile["deferredProductSurface"]
                .as_array()
                .unwrap()
                .iter()
                .any(|surface| surface == "geolocation")
        );

        let maximum_document_token = format!("document:{}:{}", "f".repeat(32), u128::MAX);
        assert_eq!(maximum_document_token.len(), DOCUMENT_STATE_TOKEN_MAX_BYTES);
        assert_eq!(
            profile["documentAuthority"]["maximumBytes"],
            maximum_document_token.len()
        );
        let _: DocumentStateToken = serde_json::from_value(json!(maximum_document_token)).unwrap();

        let maximum_session_token = format!("session:{}:{}", "f".repeat(32), u64::MAX);
        assert_eq!(
            profile["sessionStateAuthority"]["maximumBytes"],
            maximum_session_token.len()
        );
        let _: SessionStateToken = serde_json::from_value(json!(maximum_session_token)).unwrap();

        let state = &profile["stateLimits"];
        assert_eq!(state["maximumSerializedBytes"], MAX_SESSION_STATE_BYTES);
        assert_eq!(state["maximumCookies"], MAX_SESSION_COOKIES);
        assert_eq!(MAX_SESSION_COOKIES, COOKIE_STATE_MAX_COOKIES_V1);
        assert_eq!(
            state["maximumCookiesPerRegistrableHost"],
            MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST
        );
        assert_eq!(
            MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST,
            COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1
        );
        assert_eq!(
            state["perRegistrableHostCookieLimitError"]["code"],
            "too_many_session_cookies_per_registrable_host"
        );
        assert_eq!(state["maximumCookieBytes"], MAX_SESSION_COOKIE_BYTES);
        assert_eq!(MAX_SESSION_COOKIE_BYTES, COOKIE_STATE_MAX_COOKIE_BYTES_V1);
        assert_eq!(
            state["maximumTotalCookieBytes"],
            COOKIE_STATE_MAX_TOTAL_BYTES_V1
        );
        assert_eq!(
            state["maximumRawControlledCookieBytes"],
            CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1
        );
        assert_eq!(
            state["maximumControlledCookieBatchValues"],
            CONTROLLED_COOKIE_MAX_BATCH_VALUES_V1
        );
        assert_eq!(
            state["maximumCookieArrayEncodedBytes"],
            MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES
        );
        assert_eq!(
            MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES,
            COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1
        );
        assert_eq!(
            state["maximumOriginsArrayEncodedBytes"],
            MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES
        );
        assert_eq!(
            MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES,
            WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
        );
        assert_eq!(state["maximumOrigins"], MAX_SESSION_STORAGE_ORIGINS);
        assert_eq!(
            MAX_SESSION_STORAGE_ORIGINS,
            WEB_STORAGE_STATE_MAX_ORIGINS_V1
        );
        assert_eq!(
            state["maximumLocalStorageEntriesPerOrigin"],
            MAX_SESSION_STORAGE_ENTRIES_PER_AREA
        );
        assert_eq!(
            state["maximumSessionStorageEntriesPerOrigin"],
            MAX_SESSION_STORAGE_ENTRIES_PER_AREA
        );
        assert_eq!(
            MAX_SESSION_STORAGE_ENTRIES_PER_AREA,
            WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1
        );
        assert_eq!(
            state["maximumStorageKeyBytes"],
            MAX_SESSION_STORAGE_KEY_BYTES
        );
        assert_eq!(
            MAX_SESSION_STORAGE_KEY_BYTES,
            WEB_STORAGE_STATE_MAX_KEY_BYTES_V1
        );
        assert_eq!(
            state["maximumStorageValueBytes"],
            MAX_SESSION_STORAGE_VALUE_BYTES
        );
        assert_eq!(
            MAX_SESSION_STORAGE_VALUE_BYTES,
            WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1
        );
        assert_eq!(
            state["maximumTotalStorageBytesPerOrigin"],
            MAX_SESSION_STORAGE_BYTES_PER_ORIGIN
        );
        assert_eq!(
            MAX_SESSION_STORAGE_BYTES_PER_ORIGIN,
            WEB_STORAGE_STATE_MAX_ORIGIN_BYTES_V1
        );

        let partition = &state["encodedBudgetPartition"];
        assert_eq!(
            partition["cookiesArrayBytes"],
            MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES
        );
        assert_eq!(
            partition["originsArrayBytes"],
            MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES
        );
        assert_eq!(partition["envelopeAndSlackBytes"], 12_288);
        assert_eq!(partition["fixedV1EnvelopeBytes"], 147);
        assert_eq!(partition["remainingSlackBytes"], 12_141);
        assert_eq!(
            partition["cookiesArrayBytes"].as_u64().unwrap()
                + partition["originsArrayBytes"].as_u64().unwrap()
                + partition["envelopeAndSlackBytes"].as_u64().unwrap(),
            MAX_SESSION_STATE_BYTES as u64
        );
        assert_eq!(
            partition["fixedV1EnvelopeBytes"].as_u64().unwrap()
                + partition["remainingSlackBytes"].as_u64().unwrap(),
            partition["envelopeAndSlackBytes"].as_u64().unwrap()
        );

        let sequence = &state["cookieSequenceFields"];
        assert_eq!(sequence["wireSyntax"], "canonical_decimal_u64_string");
        assert_eq!(sequence["maximum"], u64::MAX.to_string());
        assert_eq!(sequence["creationSequenceUniqueWithinCookies"], true);
        assert_eq!(sequence["lastAccessSequenceUniqueWithinCookies"], true);
        let maximum_sequence: WireU64 =
            serde_json::from_value(json!(sequence["maximum"].as_str().unwrap())).unwrap();
        assert_eq!(maximum_sequence.get(), u64::MAX);

        assert_eq!(
            profile["navigation"]["sameDocument"]["historyTraversalBoundary"]["timeSurface"],
            "history_traversal"
        );
        assert_eq!(
            profile["unsupportedClasses"]["historyTraversal"],
            "history_traversal"
        );
        assert_eq!(
            profile["network"]["reachableStickyFailures"]["activeOperationLimit"]["code"],
            "controlled_network_active_operation_limit_exceeded"
        );
        assert_eq!(
            profile["network"]["reachableStickyFailures"]["unknownRequestBodyLength"]["code"],
            "unsupported_network_request_body_length"
        );
        assert_eq!(
            profile["network"]["reachableStickyFailures"]["rejectedRequestMetadata"]["code"],
            "unsupported_network_request_metadata"
        );
        assert_eq!(
            profile["sessionEvidence"]["bounds"]["maximumMethodBytes"],
            MAX_EVIDENCE_METHOD_BYTES
        );

        let automation = public_automation_limits();
        let automation_profile = &profile["automationLimits"];
        assert_eq!(
            automation_profile["maxSelectorBytes"],
            automation.max_selector_bytes()
        );
        assert_eq!(
            automation_profile["maxFillValueBytes"],
            automation.max_fill_value_bytes()
        );
        assert_eq!(
            automation_profile["maxFieldNameBytes"],
            automation.max_field_name_bytes()
        );
        assert_eq!(
            automation_profile["maxAttributeNameBytes"],
            automation.max_field_name_bytes()
        );
        assert_eq!(
            automation_profile["maxExtractionFields"],
            automation.max_extraction_fields()
        );
        assert_eq!(
            automation_profile["maxSelectValues"],
            automation.max_extraction_fields()
        );
        assert_eq!(
            automation_profile["maxSelectValueBytesTotal"],
            automation.max_fill_value_bytes()
        );
        assert_eq!(automation_profile["maxMatches"], automation.max_matches());
        assert_eq!(
            automation_profile["maxDomNodesVisited"],
            automation.max_dom_nodes_visited()
        );
        assert_eq!(
            automation_profile["maxOutputBytes"],
            automation.max_output_bytes()
        );
    }

    #[test]
    fn controlled_webapp_profile_matches_engine_wire_limits_and_outcomes() {
        let profile: Value =
            serde_json::from_str(include_str!("../../../profiles/controlled-webapp-v1.json"))
                .unwrap();
        assert_eq!(profile["schemaVersion"], 1);
        assert_eq!(profile["id"], "controlled-webapp-v1");
        assert_eq!(profile["clockMode"], "controlled");
        assert_eq!(
            profile["documentScope"],
            json!({
                "webViews": 1,
                "activeTopLevelDocuments": 1,
                "scriptEventLoops": 1,
                "childBrowsingContexts": "unsupported",
                "auxiliaryWebViews": "unsupported",
            })
        );
        assert_eq!(
            profile["navigation"],
            json!({
                "initial": {"schemes": ["http", "https"], "fetchBacked": true},
                "applicationTopLevel": {
                    "sameOriginHttpHttps": "unsupported",
                    "crossEventLoop": "unsupported",
                },
                "explicitNavigateMethod": false,
            })
        );
        assert_eq!(
            profile["selectors"],
            json!({
                "grammar": "local_compound_v1",
                "supportedComponents": ["type", "universal", "id", "class", "attribute"],
                "namedNamespacePrefixes": false,
                "combinators": false,
                "pseudoClasses": false,
                "persistentHandles": false,
            })
        );
        assert_eq!(
            profile["automation"],
            json!({
                "fill": {
                    "elements": [
                        "input:text", "input:search", "input:url", "input:tel",
                        "input:email", "input:password", "textarea",
                    ],
                    "effect": "replace_value_then_one_input_event",
                    "inputType": "insertReplacementText",
                    "focus": false,
                    "keyboardEvents": false,
                    "changeEvent": false,
                },
                "activate": {
                    "effect": "html_element_click",
                    "layoutHitTesting": false,
                    "pointerEvents": false,
                },
            })
        );
        assert_eq!(
            profile["inspection"],
            json!({
                "generationBound": true,
                "operations": ["query_count", "text", "extract_text", "extract_html"],
            })
        );
        assert_eq!(
            profile["execution"],
            json!({
                "tasks": true,
                "microtasks": true,
                "mutationObserver": true,
                "oneShotTimers": "controlled",
                "intervals": "persistent_work",
                "finiteRendering": true,
                "animationFrame": true,
                "date": true,
                "performance": true,
            })
        );

        let automation = public_automation_limits();
        assert_eq!(
            profile["automationLimits"],
            json!({
                "maxSelectorBytes": automation.max_selector_bytes(),
                "maxFillValueBytes": automation.max_fill_value_bytes(),
                "maxFieldNameBytes": automation.max_field_name_bytes(),
                "maxExtractionFields": automation.max_extraction_fields(),
                "maxMatches": automation.max_matches(),
                "maxDomNodesVisited": automation.max_dom_nodes_visited(),
                "maxOutputBytes": automation.max_output_bytes(),
            })
        );

        let execution = DocumentExecutionLimits::CONTROLLED_WEBAPP_V1;
        assert_eq!(
            profile["executionLimits"],
            json!({
                "ordinaryTasks": execution.ordinary_tasks,
                "microtasks": execution.microtasks,
                "renderingOpportunities": execution.rendering_opportunities,
                "mutations": execution.mutations,
            })
        );

        let outcomes = [
            SettleOutcome::Quiescent,
            SettleOutcome::QuiescentWithPersistentWork,
            SettleOutcome::BlockedOnExternalIo,
            SettleOutcome::BlockedOnOpenEndedWork,
            SettleOutcome::UnsupportedWork,
            SettleOutcome::VirtualTimeLimitExceeded,
            SettleOutcome::TaskLimitExceeded,
            SettleOutcome::MicrotaskLimitExceeded,
            SettleOutcome::RenderingLimitExceeded,
            SettleOutcome::MutationLimitExceeded,
            SettleOutcome::ControlTurnLimitExceeded,
            SettleOutcome::RuntimeError,
        ];
        assert_eq!(
            profile["settlementOutcomes"],
            serde_json::to_value(outcomes).unwrap()
        );
        assert_eq!(
            profile["network"],
            json!({
                "fetch": "asynchronous",
                "xmlHttpRequest": "asynchronous",
                "synchronousXmlHttpRequest": "rejected_before_start",
                "externalIoTimeout": "typed_blocker",
                "reproducibility": "local_or_intercepted_fixture_only",
            })
        );
        assert_eq!(profile["unsupportedRetention"], "first_per_authority");
        assert_eq!(
            profile["unsupportedClasses"],
            json!({
                "iframe": ["same_event_loop_iframe", "cross_event_loop_iframe"],
                "applicationTopLevelNavigation": "cross_event_loop_navigation",
                "worker": "worker",
                "worklet": "worklet",
                "auxiliaryWebView": "auxiliary_web_view",
                "webSocketAndServerSentEvents": "external_subscription",
                "externalChannels": "external_subscription",
                "storageAndUntrackedResourceIo": "resource_thread_io",
                "media": "native_media",
                "embedderControls": "embedder_control",
                "hostTimestamp": "host_timestamp",
            })
        );
        assert_eq!(
            profile["determinismExcludes"],
            json!([
                "live_network",
                "uncontrolled_randomness",
                "ambient_system_state"
            ])
        );
    }
}
