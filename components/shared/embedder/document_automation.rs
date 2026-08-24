/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Same-build commands for bounded, native automation against one rooted document.
//!
//! These types are an internal Servo protocol, not the product wire format. In particular, a
//! request binds private target authority and a complete-state generation which the Script owner
//! must revalidate immediately before executing the operation. Results never contain DOM handles.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::document_pending::{
    PendingSnapshotInvariantError, PendingTargetObservation, RuntimeStateGeneration,
};

/// Resource limits applied before and while executing one document automation operation.
///
/// `max_output_bytes` counts the UTF-8 bytes of strings in the logical result. A product protocol
/// must independently bound its framing and serialization overhead.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentAutomationLimits {
    max_selector_bytes: u32,
    max_fill_value_bytes: u32,
    max_field_name_bytes: u32,
    max_extraction_fields: u32,
    max_matches: u32,
    max_dom_nodes_visited: u32,
    max_output_bytes: u64,
}

impl DocumentAutomationLimits {
    /// Conservative limits for the first single-document automation slice.
    pub const MVP: Self = Self {
        max_selector_bytes: 16 * 1024,
        max_fill_value_bytes: 1024 * 1024,
        max_field_name_bytes: 1024,
        max_extraction_fields: 128,
        max_matches: 10_000,
        max_dom_nodes_visited: 1_000_000,
        max_output_bytes: 8 * 1024 * 1024,
    };

    /// Construct checked limits for a trusted same-build caller.
    #[doc(hidden)]
    pub fn new_internal(
        max_selector_bytes: u32,
        max_fill_value_bytes: u32,
        max_field_name_bytes: u32,
        max_extraction_fields: u32,
        max_matches: u32,
        max_dom_nodes_visited: u32,
        max_output_bytes: u64,
    ) -> Result<Self, DocumentAutomationRequestError> {
        let limits = Self {
            max_selector_bytes,
            max_fill_value_bytes,
            max_field_name_bytes,
            max_extraction_fields,
            max_matches,
            max_dom_nodes_visited,
            max_output_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Maximum UTF-8 byte length of any CSS selector in the request.
    pub const fn max_selector_bytes(self) -> u32 {
        self.max_selector_bytes
    }

    /// Maximum UTF-8 byte length of a fill value.
    pub const fn max_fill_value_bytes(self) -> u32 {
        self.max_fill_value_bytes
    }

    /// Maximum UTF-8 byte length of an extraction field name.
    pub const fn max_field_name_bytes(self) -> u32 {
        self.max_field_name_bytes
    }

    /// Maximum number of fields in one extraction plan.
    pub const fn max_extraction_fields(self) -> u32 {
        self.max_extraction_fields
    }

    /// Maximum number of matches returned by any one CSS query.
    pub const fn max_matches(self) -> u32 {
        self.max_matches
    }

    /// Maximum cumulative DOM nodes visited by selector and read work in one operation.
    ///
    /// Every selector evaluation charges its scope root before walking descendants, so this also
    /// bounds extraction plans which repeatedly query empty roots. The executor separately uses
    /// this value as the ceiling for precharged selector-list arm/component evaluations, avoiding
    /// a multiplicative selector-complexity bypass without weakening the node-visit accounting.
    pub const fn max_dom_nodes_visited(self) -> u32 {
        self.max_dom_nodes_visited
    }

    /// Maximum logical UTF-8 string payload returned by an operation.
    pub const fn max_output_bytes(self) -> u64 {
        self.max_output_bytes
    }

    /// Validate limits which may have crossed a same-build serialization boundary.
    #[doc(hidden)]
    pub fn validate(self) -> Result<(), DocumentAutomationRequestError> {
        for (kind, value, hard_maximum) in [
            (
                DocumentAutomationLimitKind::SelectorBytes,
                u64::from(self.max_selector_bytes),
                u64::from(Self::MVP.max_selector_bytes),
            ),
            (
                DocumentAutomationLimitKind::FillValueBytes,
                u64::from(self.max_fill_value_bytes),
                u64::from(Self::MVP.max_fill_value_bytes),
            ),
            (
                DocumentAutomationLimitKind::FieldNameBytes,
                u64::from(self.max_field_name_bytes),
                u64::from(Self::MVP.max_field_name_bytes),
            ),
            (
                DocumentAutomationLimitKind::ExtractionFields,
                u64::from(self.max_extraction_fields),
                u64::from(Self::MVP.max_extraction_fields),
            ),
            (
                DocumentAutomationLimitKind::Matches,
                u64::from(self.max_matches),
                u64::from(Self::MVP.max_matches),
            ),
            (
                DocumentAutomationLimitKind::DomNodesVisited,
                u64::from(self.max_dom_nodes_visited),
                u64::from(Self::MVP.max_dom_nodes_visited),
            ),
            (
                DocumentAutomationLimitKind::OutputBytes,
                self.max_output_bytes,
                Self::MVP.max_output_bytes,
            ),
        ] {
            if value == 0 {
                return Err(DocumentAutomationRequestError::ZeroLimit(kind));
            }
            if value > hard_maximum {
                return Err(DocumentAutomationRequestError::LimitExceedsHardMaximum {
                    kind,
                    actual: value,
                    maximum: hard_maximum,
                });
            }
        }
        Ok(())
    }
}

impl Default for DocumentAutomationLimits {
    fn default() -> Self {
        Self::MVP
    }
}

/// A configurable document-automation resource limit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAutomationLimitKind {
    SelectorBytes,
    FillValueBytes,
    FieldNameBytes,
    ExtractionFields,
    Matches,
    DomNodesVisited,
    OutputBytes,
}

/// The explicitly selected, bounded selector grammar for one same-build automation request.
///
/// The legacy constructor always selects [`Self::LocalCompoundV1`]. New public profiles must opt
/// into a broader grammar explicitly so a native capability addition cannot silently widen a
/// frozen profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentSelectorGrammar {
    LocalCompoundV1,
    PracticalV2,
}

/// How a strict extraction field reads its one matched element.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentExtractionRead {
    TextContent,
    InnerHtml,
    /// Raw, nullable value of one no-namespace attribute.
    Attribute,
    /// Raw attribute resolved against the document's effective base URL, or `None` when the
    /// attribute is absent or is not a valid relative/absolute URL.
    ResolvedUrl,
}

/// One named, exact-one field in an extraction plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentExtractionField {
    name: String,
    selector: String,
    read: DocumentExtractionRead,
    attribute: Option<String>,
}

impl DocumentExtractionField {
    /// Construct a field for a trusted same-build caller. The enclosing request validates it.
    #[doc(hidden)]
    pub fn new_internal(name: String, selector: String, read: DocumentExtractionRead) -> Self {
        Self {
            name,
            selector,
            read,
            attribute: None,
        }
    }

    /// Construct a nullable raw or resolved-URL attribute field for a trusted same-build caller.
    /// The enclosing request validates the attribute name and read kind.
    #[doc(hidden)]
    pub fn new_attribute_internal(
        name: String,
        selector: String,
        read: DocumentExtractionRead,
        attribute: String,
    ) -> Self {
        Self {
            name,
            selector,
            read,
            attribute: Some(attribute),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn selector(&self) -> &str {
        &self.selector
    }

    pub const fn read(&self) -> DocumentExtractionRead {
        self.read
    }

    pub fn attribute(&self) -> Option<&str> {
        self.attribute.as_deref()
    }
}

/// A bounded extraction plan whose root selector may match zero or more rows.
///
/// Every field selector is evaluated relative to a row root and must match exactly one element.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentExtractionPlan {
    root_selector: String,
    fields: Vec<DocumentExtractionField>,
}

impl DocumentExtractionPlan {
    /// Construct a plan for a trusted same-build caller. The enclosing request validates it.
    #[doc(hidden)]
    pub fn new_internal(root_selector: String, fields: Vec<DocumentExtractionField>) -> Self {
        Self {
            root_selector,
            fields,
        }
    }

    pub fn root_selector(&self) -> &str {
        &self.root_selector
    }

    pub fn fields(&self) -> &[DocumentExtractionField] {
        &self.fields
    }
}

/// A native operation against one document. CSS selectors never produce reusable handles.
///
/// The executor accepts a conservative selector subset whose local and structural matching work
/// is explicitly charged. It rejects selector features whose hidden traversal is not covered by
/// that budget.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAutomationOperation {
    QueryCount {
        selector: String,
    },
    TextContent {
        selector: String,
    },
    InnerHtml {
        selector: String,
    },
    Extract(DocumentExtractionPlan),
    Fill {
        selector: String,
        value: String,
    },
    Activate {
        selector: String,
    },
    Check {
        selector: String,
    },
    Uncheck {
        selector: String,
    },
    Select {
        selector: String,
        values: Vec<String>,
    },
    Focus {
        selector: String,
    },
    Submit {
        selector: String,
    },
}

impl DocumentAutomationOperation {
    /// Whether execution can synchronously mutate page state or run page event handlers.
    ///
    /// Callers must conservatively classify a lost response for these operations as
    /// indeterminate: the Script owner may already have crossed the native mutation boundary.
    pub const fn may_mutate_document(&self) -> bool {
        matches!(
            self,
            Self::Fill { .. }
                | Self::Activate { .. }
                | Self::Check { .. }
                | Self::Uncheck { .. }
                | Self::Select { .. }
                | Self::Focus { .. }
                | Self::Submit { .. }
        )
    }

    /// Validate all bounded request data without parsing engine-specific CSS syntax.
    #[doc(hidden)]
    pub fn validate(
        &self,
        limits: DocumentAutomationLimits,
    ) -> Result<(), DocumentAutomationRequestError> {
        limits.validate()?;
        match self {
            Self::QueryCount { selector }
            | Self::TextContent { selector }
            | Self::InnerHtml { selector }
            | Self::Activate { selector }
            | Self::Check { selector }
            | Self::Uncheck { selector }
            | Self::Focus { selector }
            | Self::Submit { selector } => validate_selector(selector, limits),
            Self::Fill { selector, value } => {
                validate_selector(selector, limits)?;
                validate_length(
                    value,
                    u64::from(limits.max_fill_value_bytes),
                    |actual, limit| DocumentAutomationRequestError::FillValueTooLong {
                        actual,
                        limit,
                    },
                )
            },
            Self::Extract(plan) => {
                validate_selector(plan.root_selector(), limits)?;
                if plan.fields().is_empty() {
                    return Err(DocumentAutomationRequestError::EmptyExtractionPlan);
                }
                let actual = plan.fields().len() as u64;
                if actual > u64::from(limits.max_extraction_fields) {
                    return Err(DocumentAutomationRequestError::TooManyExtractionFields {
                        actual,
                        limit: limits.max_extraction_fields,
                    });
                }

                let mut names = HashSet::with_capacity(plan.fields().len());
                for (index, field) in plan.fields().iter().enumerate() {
                    if field.name().is_empty() {
                        return Err(DocumentAutomationRequestError::EmptyExtractionFieldName {
                            index: index as u32,
                        });
                    }
                    validate_length(
                        field.name(),
                        u64::from(limits.max_field_name_bytes),
                        |actual, limit| {
                            DocumentAutomationRequestError::ExtractionFieldNameTooLong {
                                actual,
                                limit,
                            }
                        },
                    )?;
                    if !names.insert(field.name()) {
                        return Err(
                            DocumentAutomationRequestError::DuplicateExtractionFieldName {
                                name: field.name().to_owned(),
                            },
                        );
                    }
                    validate_selector(field.selector(), limits)?;
                    match (field.read(), field.attribute()) {
                        (
                            DocumentExtractionRead::Attribute | DocumentExtractionRead::ResolvedUrl,
                            Some(attribute),
                        ) => validate_attribute_name(index as u32, attribute, limits)?,
                        (
                            DocumentExtractionRead::TextContent | DocumentExtractionRead::InnerHtml,
                            None,
                        ) => {},
                        _ => {
                            return Err(
                                DocumentAutomationRequestError::InvalidExtractionFieldRead {
                                    index: index as u32,
                                },
                            );
                        },
                    }
                }
                Ok(())
            },
            Self::Select { selector, values } => {
                validate_selector(selector, limits)?;
                let actual = values.len() as u64;
                if actual > u64::from(limits.max_extraction_fields) {
                    return Err(DocumentAutomationRequestError::TooManySelectValues {
                        actual,
                        limit: limits.max_extraction_fields,
                    });
                }
                let mut unique = HashSet::with_capacity(values.len());
                let mut bytes = 0u64;
                for value in values {
                    if !unique.insert(value) {
                        return Err(DocumentAutomationRequestError::DuplicateSelectValue {
                            value: value.to_owned(),
                        });
                    }
                    bytes = bytes.checked_add(value.len() as u64).ok_or(
                        DocumentAutomationRequestError::SelectValuesTooLong {
                            actual: u64::MAX,
                            limit: u64::from(limits.max_fill_value_bytes),
                        },
                    )?;
                }
                if bytes > u64::from(limits.max_fill_value_bytes) {
                    return Err(DocumentAutomationRequestError::SelectValuesTooLong {
                        actual: bytes,
                        limit: u64::from(limits.max_fill_value_bytes),
                    });
                }
                Ok(())
            },
        }
    }
}

fn validate_attribute_name(
    index: u32,
    attribute: &str,
    limits: DocumentAutomationLimits,
) -> Result<(), DocumentAutomationRequestError> {
    let actual = attribute.len() as u64;
    let limit = u64::from(limits.max_field_name_bytes);
    if actual == 0 || actual > limit {
        return Err(
            DocumentAutomationRequestError::InvalidExtractionAttributeName {
                index,
                actual,
                limit,
            },
        );
    }
    if attribute
        .chars()
        .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
    {
        return Err(
            DocumentAutomationRequestError::InvalidExtractionAttributeName {
                index,
                actual,
                limit,
            },
        );
    }
    Ok(())
}

fn validate_selector(
    selector: &str,
    limits: DocumentAutomationLimits,
) -> Result<(), DocumentAutomationRequestError> {
    validate_length(
        selector,
        u64::from(limits.max_selector_bytes),
        |actual, limit| DocumentAutomationRequestError::SelectorTooLong { actual, limit },
    )
}

fn validate_length(
    value: &str,
    limit: u64,
    error: fn(u64, u64) -> DocumentAutomationRequestError,
) -> Result<(), DocumentAutomationRequestError> {
    let actual = value.len() as u64;
    if actual > limit {
        return Err(error(actual, limit));
    }
    Ok(())
}

/// One operation bound to immutable target authority and an expected complete-state generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentAutomationRequest {
    target: PendingTargetObservation,
    expected_generation: RuntimeStateGeneration,
    operation: DocumentAutomationOperation,
    limits: DocumentAutomationLimits,
    selector_grammar: DocumentSelectorGrammar,
}

impl DocumentAutomationRequest {
    /// Construct and validate a request on a trusted same-build boundary.
    #[doc(hidden)]
    pub fn new_internal(
        target: PendingTargetObservation,
        expected_generation: RuntimeStateGeneration,
        operation: DocumentAutomationOperation,
        limits: DocumentAutomationLimits,
    ) -> Result<Self, DocumentAutomationRequestError> {
        Self::new_with_selector_grammar_internal(
            target,
            expected_generation,
            operation,
            limits,
            DocumentSelectorGrammar::LocalCompoundV1,
        )
    }

    /// Construct a request with an explicit selector grammar for a trusted same-build caller.
    /// Public callers must choose this only after selecting a profile which advertises it.
    #[doc(hidden)]
    pub fn new_with_selector_grammar_internal(
        target: PendingTargetObservation,
        expected_generation: RuntimeStateGeneration,
        operation: DocumentAutomationOperation,
        limits: DocumentAutomationLimits,
        selector_grammar: DocumentSelectorGrammar,
    ) -> Result<Self, DocumentAutomationRequestError> {
        target
            .validate()
            .map_err(DocumentAutomationRequestError::InvalidTarget)?;
        operation.validate(limits)?;
        Ok(Self {
            target,
            expected_generation,
            operation,
            limits,
            selector_grammar,
        })
    }

    /// Exact target authority which the Script owner must revalidate before execution.
    pub const fn target(&self) -> &PendingTargetObservation {
        &self.target
    }

    /// Complete-state generation which the Script owner must revalidate before execution.
    pub const fn expected_generation(&self) -> RuntimeStateGeneration {
        self.expected_generation
    }

    pub const fn operation(&self) -> &DocumentAutomationOperation {
        &self.operation
    }

    pub const fn limits(&self) -> DocumentAutomationLimits {
        self.limits
    }

    pub const fn selector_grammar(&self) -> DocumentSelectorGrammar {
        self.selector_grammar
    }

    /// Revalidate bounded data after deserialization. Target freshness is an owner responsibility.
    #[doc(hidden)]
    pub fn validate(&self) -> Result<(), DocumentAutomationRequestError> {
        self.target
            .validate()
            .map_err(DocumentAutomationRequestError::InvalidTarget)?;
        self.operation.validate(self.limits)
    }

    /// Revalidate the complete owner authority immediately before native execution.
    ///
    /// The caller must collect both observations at the same Script-thread linearization point
    /// at which it roots the active document. In particular, a product-level decimal generation
    /// is only caller input; this comparison against the owner's checked generation is the actual
    /// authority check.
    pub fn validate_for_execution(
        &self,
        observed_target: &PendingTargetObservation,
        observed_generation: RuntimeStateGeneration,
    ) -> Result<(), DocumentAutomationError> {
        self.validate()
            .map_err(DocumentAutomationError::InvalidRequest)?;
        if &self.target != observed_target {
            return Err(DocumentAutomationError::TargetChanged);
        }
        if self.expected_generation != observed_generation {
            return Err(DocumentAutomationError::StaleStateGeneration {
                expected: self.expected_generation,
                observed: observed_generation,
            });
        }
        Ok(())
    }
}

/// Why a same-build request is malformed before any DOM operation is attempted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAutomationRequestError {
    InvalidTarget(PendingSnapshotInvariantError),
    ZeroLimit(DocumentAutomationLimitKind),
    LimitExceedsHardMaximum {
        kind: DocumentAutomationLimitKind,
        actual: u64,
        maximum: u64,
    },
    SelectorTooLong {
        actual: u64,
        limit: u64,
    },
    FillValueTooLong {
        actual: u64,
        limit: u64,
    },
    ExtractionFieldNameTooLong {
        actual: u64,
        limit: u64,
    },
    EmptyExtractionPlan,
    TooManyExtractionFields {
        actual: u64,
        limit: u32,
    },
    EmptyExtractionFieldName {
        index: u32,
    },
    DuplicateExtractionFieldName {
        name: String,
    },
    InvalidExtractionFieldRead {
        index: u32,
    },
    InvalidExtractionAttributeName {
        index: u32,
        actual: u64,
        limit: u64,
    },
    TooManySelectValues {
        actual: u64,
        limit: u32,
    },
    SelectValuesTooLong {
        actual: u64,
        limit: u64,
    },
    DuplicateSelectValue {
        value: String,
    },
}

/// Stable operation names used in checked DOM-failure results.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAutomationOperationKind {
    InnerHtml,
    Fill,
    Select,
    Focus,
    Submit,
}

/// A typed rejection from authority validation or native DOM execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAutomationError {
    InvalidRequest(DocumentAutomationRequestError),
    TargetChanged,
    /// A sticky execution terminal was already authoritative before a requested mutation.
    ExecutionTerminated,
    StaleStateGeneration {
        expected: RuntimeStateGeneration,
        observed: RuntimeStateGeneration,
    },
    InvalidSelector {
        selector: String,
    },
    UnsupportedSelector {
        selector: String,
    },
    MatchLimitExceeded {
        selector: String,
        observed: u64,
        limit: u32,
    },
    DomTraversalLimitExceeded {
        observed: u64,
        limit: u32,
    },
    SelectorEvaluationLimitExceeded {
        observed: u64,
        limit: u32,
    },
    ElementNotFound {
        selector: String,
    },
    SelectorAmbiguous {
        selector: String,
        matches: u64,
    },
    ExtractionFieldNotFound {
        row: u32,
        field: String,
        selector: String,
    },
    ExtractionFieldAmbiguous {
        row: u32,
        field: String,
        selector: String,
        matches: u64,
    },
    UnsupportedFillElement {
        selector: String,
    },
    ImmutableFillElement {
        selector: String,
    },
    UnsupportedActivationElement {
        selector: String,
    },
    DisabledActivationElement {
        selector: String,
    },
    UnsupportedCheckElement {
        selector: String,
    },
    ImmutableCheckElement {
        selector: String,
    },
    UnsupportedUncheckElement {
        selector: String,
    },
    ImmutableUncheckElement {
        selector: String,
    },
    UnsupportedSelectElement {
        selector: String,
    },
    ImmutableSelectElement {
        selector: String,
    },
    InvalidSelectMultiplicity {
        selector: String,
        multiple: bool,
        requested: u32,
    },
    SelectValueNotFound {
        selector: String,
        value: String,
    },
    SelectValueDisabled {
        selector: String,
        value: String,
    },
    UnsupportedFocusElement {
        selector: String,
    },
    UnsupportedSubmitElement {
        selector: String,
    },
    UnsupportedLazyAttributeSerialization {
        attribute: String,
    },
    DomOperationFailed {
        operation: DocumentAutomationOperationKind,
    },
    OutputLimitExceeded {
        attempted: u64,
        limit: u64,
    },
}

/// One named value in an extraction row, retained in plan order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentExtractionValue {
    pub name: String,
    pub value: Option<String>,
}

/// One root match and all of its strict exact-one field values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentExtractionRow {
    pub fields: Vec<DocumentExtractionValue>,
}

/// A bounded result which never contains a DOM or JavaScript handle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAutomationResult {
    QueryCount { count: u32 },
    TextContent { value: String },
    InnerHtml { value: String },
    Extract { rows: Vec<DocumentExtractionRow> },
    Filled,
    Activated,
    Checked { changed: bool, checked: bool },
    Selected { changed: bool, values: Vec<String> },
    Focused { focused: bool },
    Submitted,
}

#[cfg(test)]
mod tests {
    use postcard::{from_bytes, to_stdvec};
    use servo_base::Epoch;
    use servo_base::id::{TEST_PIPELINE_ID, TEST_SCRIPT_EVENT_LOOP_ID, TEST_WEBVIEW_ID};

    use super::*;
    use crate::document_pending::{PendingActiveTopLevelPipeline, PendingNavigationRevision};

    fn target() -> PendingTargetObservation {
        PendingTargetObservation::new(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(7),
            }),
            PendingNavigationRevision::new(3),
            vec![TEST_PIPELINE_ID],
            vec![TEST_PIPELINE_ID],
            Vec::new(),
        )
        .unwrap()
    }

    fn field(name: &str, selector: &str) -> DocumentExtractionField {
        DocumentExtractionField::new_internal(
            name.to_owned(),
            selector.to_owned(),
            DocumentExtractionRead::TextContent,
        )
    }

    #[test]
    fn request_round_trip_preserves_private_authority_and_plan_order() {
        let operation = DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            vec![field("case", ".case"), field("court", ".court")],
        ));
        let request = DocumentAutomationRequest::new_internal(
            target(),
            RuntimeStateGeneration::new(19),
            operation,
            DocumentAutomationLimits::MVP,
        )
        .unwrap();

        let bytes = to_stdvec(&request).unwrap();
        let decoded: DocumentAutomationRequest = from_bytes(&bytes).unwrap();

        assert_eq!(decoded, request);
        assert_eq!(decoded.expected_generation().get(), 19);
        assert_eq!(
            decoded.selector_grammar(),
            DocumentSelectorGrammar::LocalCompoundV1,
        );
        assert!(decoded.target().contains_pipeline(TEST_PIPELINE_ID));
        let DocumentAutomationOperation::Extract(plan) = decoded.operation() else {
            panic!("expected extraction plan");
        };
        assert_eq!(plan.fields()[0].name(), "case");
        assert_eq!(plan.fields()[1].name(), "court");

        let practical = DocumentAutomationRequest::new_with_selector_grammar_internal(
            target(),
            RuntimeStateGeneration::new(20),
            DocumentAutomationOperation::QueryCount {
                selector: ".row > .case".to_owned(),
            },
            DocumentAutomationLimits::MVP,
            DocumentSelectorGrammar::PracticalV2,
        )
        .unwrap();
        let decoded: DocumentAutomationRequest =
            from_bytes(&to_stdvec(&practical).unwrap()).unwrap();
        assert_eq!(decoded, practical);
        assert_eq!(
            decoded.selector_grammar(),
            DocumentSelectorGrammar::PracticalV2,
        );
    }

    #[test]
    fn execution_authority_requires_the_exact_target_and_generation() {
        let request = DocumentAutomationRequest::new_internal(
            target(),
            RuntimeStateGeneration::new(u64::MAX),
            DocumentAutomationOperation::TextContent {
                selector: "#result".to_owned(),
            },
            DocumentAutomationLimits::MVP,
        )
        .unwrap();

        assert_eq!(
            request.validate_for_execution(request.target(), RuntimeStateGeneration::new(u64::MAX)),
            Ok(()),
        );

        let mut changed_target = request.target().clone();
        changed_target.navigation_revision = PendingNavigationRevision::new(4);
        assert_eq!(
            request.validate_for_execution(&changed_target, RuntimeStateGeneration::new(u64::MAX),),
            Err(DocumentAutomationError::TargetChanged),
        );
        assert_eq!(
            request.validate_for_execution(request.target(), RuntimeStateGeneration::new(0)),
            Err(DocumentAutomationError::StaleStateGeneration {
                expected: RuntimeStateGeneration::new(u64::MAX),
                observed: RuntimeStateGeneration::new(0),
            }),
        );
    }

    #[test]
    fn limits_must_be_positive() {
        assert_eq!(
            DocumentAutomationLimits::new_internal(1, 1, 1, 1, 1, 1, 0),
            Err(DocumentAutomationRequestError::ZeroLimit(
                DocumentAutomationLimitKind::OutputBytes,
            )),
        );
    }

    #[test]
    fn serialized_callers_cannot_raise_any_owner_hard_limit() {
        let mut selector = DocumentAutomationLimits::MVP;
        selector.max_selector_bytes += 1;
        let mut fill = DocumentAutomationLimits::MVP;
        fill.max_fill_value_bytes += 1;
        let mut field_name = DocumentAutomationLimits::MVP;
        field_name.max_field_name_bytes += 1;
        let mut fields = DocumentAutomationLimits::MVP;
        fields.max_extraction_fields += 1;
        let mut matches = DocumentAutomationLimits::MVP;
        matches.max_matches += 1;
        let mut visited = DocumentAutomationLimits::MVP;
        visited.max_dom_nodes_visited += 1;
        let mut output = DocumentAutomationLimits::MVP;
        output.max_output_bytes += 1;

        for (forged, kind, actual, maximum) in [
            (
                selector,
                DocumentAutomationLimitKind::SelectorBytes,
                u64::from(selector.max_selector_bytes),
                u64::from(DocumentAutomationLimits::MVP.max_selector_bytes),
            ),
            (
                fill,
                DocumentAutomationLimitKind::FillValueBytes,
                u64::from(fill.max_fill_value_bytes),
                u64::from(DocumentAutomationLimits::MVP.max_fill_value_bytes),
            ),
            (
                field_name,
                DocumentAutomationLimitKind::FieldNameBytes,
                u64::from(field_name.max_field_name_bytes),
                u64::from(DocumentAutomationLimits::MVP.max_field_name_bytes),
            ),
            (
                fields,
                DocumentAutomationLimitKind::ExtractionFields,
                u64::from(fields.max_extraction_fields),
                u64::from(DocumentAutomationLimits::MVP.max_extraction_fields),
            ),
            (
                matches,
                DocumentAutomationLimitKind::Matches,
                u64::from(matches.max_matches),
                u64::from(DocumentAutomationLimits::MVP.max_matches),
            ),
            (
                visited,
                DocumentAutomationLimitKind::DomNodesVisited,
                u64::from(visited.max_dom_nodes_visited),
                u64::from(DocumentAutomationLimits::MVP.max_dom_nodes_visited),
            ),
            (
                output,
                DocumentAutomationLimitKind::OutputBytes,
                output.max_output_bytes,
                DocumentAutomationLimits::MVP.max_output_bytes,
            ),
        ] {
            let bytes = to_stdvec(&forged).unwrap();
            let decoded: DocumentAutomationLimits = from_bytes(&bytes).unwrap();
            assert_eq!(
                decoded.validate(),
                Err(DocumentAutomationRequestError::LimitExceedsHardMaximum {
                    kind,
                    actual,
                    maximum,
                }),
            );
        }
    }

    #[test]
    fn extraction_plan_is_bounded_and_has_unique_nonempty_names() {
        let limits = DocumentAutomationLimits::new_internal(8, 8, 4, 1, 2, 16, 16).unwrap();
        let too_many = DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            vec![field("a", ".a"), field("b", ".b")],
        ));
        assert_eq!(
            too_many.validate(limits),
            Err(DocumentAutomationRequestError::TooManyExtractionFields {
                actual: 2,
                limit: 1,
            }),
        );

        let limits = DocumentAutomationLimits::new_internal(8, 8, 4, 2, 2, 16, 16).unwrap();
        let duplicate = DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            vec![field("case", ".a"), field("case", ".b")],
        ));
        assert_eq!(
            duplicate.validate(limits),
            Err(
                DocumentAutomationRequestError::DuplicateExtractionFieldName {
                    name: "case".to_owned(),
                },
            ),
        );

        let empty = DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            Vec::new(),
        ));
        assert_eq!(
            empty.validate(limits),
            Err(DocumentAutomationRequestError::EmptyExtractionPlan),
        );
    }

    #[test]
    fn attribute_extraction_requires_a_bounded_valid_attribute_name() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 8, 2, 2, 32, 64).unwrap();
        let valid = DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            vec![DocumentExtractionField::new_attribute_internal(
                "href".to_owned(),
                "a".to_owned(),
                DocumentExtractionRead::ResolvedUrl,
                "href".to_owned(),
            )],
        ));
        assert_eq!(valid.validate(limits), Ok(()));

        let missing_attribute =
            DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
                ".row".to_owned(),
                vec![DocumentExtractionField::new_internal(
                    "href".to_owned(),
                    "a".to_owned(),
                    DocumentExtractionRead::Attribute,
                )],
            ));
        assert_eq!(
            missing_attribute.validate(limits),
            Err(DocumentAutomationRequestError::InvalidExtractionFieldRead { index: 0 }),
        );

        let whitespace =
            DocumentAutomationOperation::Extract(DocumentExtractionPlan::new_internal(
                ".row".to_owned(),
                vec![DocumentExtractionField::new_attribute_internal(
                    "href".to_owned(),
                    "a".to_owned(),
                    DocumentExtractionRead::Attribute,
                    "bad name".to_owned(),
                )],
            ));
        assert_eq!(
            whitespace.validate(limits),
            Err(
                DocumentAutomationRequestError::InvalidExtractionAttributeName {
                    index: 0,
                    actual: 8,
                    limit: 8,
                },
            ),
        );
    }

    #[test]
    fn select_values_are_unique_and_cumulatively_bounded() {
        let limits = DocumentAutomationLimits::new_internal(32, 5, 8, 2, 2, 32, 64).unwrap();
        assert_eq!(
            DocumentAutomationOperation::Select {
                selector: "select".to_owned(),
                values: vec!["a".to_owned(), "a".to_owned()],
            }
            .validate(limits),
            Err(DocumentAutomationRequestError::DuplicateSelectValue {
                value: "a".to_owned(),
            }),
        );
        assert_eq!(
            DocumentAutomationOperation::Select {
                selector: "select".to_owned(),
                values: vec!["abc".to_owned(), "def".to_owned()],
            }
            .validate(limits),
            Err(DocumentAutomationRequestError::SelectValuesTooLong {
                actual: 6,
                limit: 5,
            }),
        );
    }

    #[test]
    fn selector_and_fill_value_are_bounded_before_execution() {
        let limits = DocumentAutomationLimits::new_internal(3, 4, 4, 1, 1, 8, 8).unwrap();
        assert_eq!(
            DocumentAutomationOperation::TextContent {
                selector: "abcd".to_owned(),
            }
            .validate(limits),
            Err(DocumentAutomationRequestError::SelectorTooLong {
                actual: 4,
                limit: 3,
            }),
        );
        assert_eq!(
            DocumentAutomationOperation::Fill {
                selector: "#x".to_owned(),
                value: "12345".to_owned(),
            }
            .validate(limits),
            Err(DocumentAutomationRequestError::FillValueTooLong {
                actual: 5,
                limit: 4,
            }),
        );
    }

    #[test]
    fn mutation_classification_is_conservative_for_native_events() {
        assert!(
            DocumentAutomationOperation::Fill {
                selector: "#field".to_owned(),
                value: String::new(),
            }
            .may_mutate_document()
        );
        assert!(
            DocumentAutomationOperation::Activate {
                selector: "#save".to_owned(),
            }
            .may_mutate_document()
        );
        for operation in [
            DocumentAutomationOperation::Check {
                selector: "#check".to_owned(),
            },
            DocumentAutomationOperation::Uncheck {
                selector: "#check".to_owned(),
            },
            DocumentAutomationOperation::Select {
                selector: "select".to_owned(),
                values: vec!["one".to_owned()],
            },
            DocumentAutomationOperation::Focus {
                selector: "#field".to_owned(),
            },
            DocumentAutomationOperation::Submit {
                selector: "form".to_owned(),
            },
        ] {
            assert!(operation.may_mutate_document());
        }
        assert!(
            !DocumentAutomationOperation::TextContent {
                selector: "#result".to_owned(),
            }
            .may_mutate_document()
        );
    }
}
