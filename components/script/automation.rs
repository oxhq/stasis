/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Native, bounded document automation which runs on the owning Script thread.
//!
//! Routing must supply a rooted, current document after revalidating the private target and state
//! generation in [`DocumentAutomationRequest`]. This module deliberately does not evaluate
//! caller-supplied JavaScript, expose DOM handles, perform pointer hit testing, or settle the event
//! loop. Native fill and activation dispatch DOM events, so page event handlers can run as part of
//! those explicitly mutating operations.

use std::io::{self, Write};

use embedder_traits::document_automation::{
    DocumentAutomationError, DocumentAutomationLimits, DocumentAutomationOperation,
    DocumentAutomationOperationKind, DocumentAutomationRequest, DocumentAutomationResult,
    DocumentExtractionRead, DocumentExtractionRow, DocumentExtractionValue,
};
use html5ever::serialize::{
    HtmlSerializer, Serialize as _, SerializeOpts as HtmlSerializeOpts,
    TraversalScope as HtmlTraversalScope,
};
use html5ever::{LocalName, QualName};
use js::context::JSContext;
use layout_api::with_layout_state;
use selectors::Element as _;
use selectors::matching::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches,
    matches_selector_list,
};
use selectors::parser::{Component, SelectorList};
use style::attr::AttrValue;
use style::dom::{TDocument, TNode};
use style::selector_parser::{SelectorImpl, SelectorParser};
use style::stylesheets::UrlExtraData;
use xml5ever::serialize::{TraversalScope as XmlTraversalScope, XmlSerializer};

use crate::dom::bindings::codegen::Bindings::HTMLElementBinding::HTMLElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLInputElementBinding::HTMLInputElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTemplateElementBinding::HTMLTemplateElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTextAreaElementBinding::HTMLTextAreaElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot, LayoutDom, ToLayout, UnrootedDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::documenttype::DocumentType;
use crate::dom::element::Element;
use crate::dom::event::Event;
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::form_controls::htmlinputelement::HTMLInputElement;
use crate::dom::html::form_controls::htmltextareaelement::HTMLTextAreaElement;
use crate::dom::html::form_controls::input_type::InputType;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::htmltemplateelement::HTMLTemplateElement;
use crate::dom::inputevent::InputEvent;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::processinginstruction::ProcessingInstruction;
use crate::dom::servoparser::html::HtmlSerialize;
use crate::dom::servoparser::serialize_html_fragment;
use crate::dom::text::Text;
use crate::layout_dom::{ServoDangerousStyleElement, ServoDangerousStyleNode};

/// Execute one request after the owner has revalidated its target and generation and rooted the
/// corresponding active document.
///
/// This function validates request bounds again because same-build deserialization can bypass the
/// checked constructors. It intentionally does not perform authority validation itself.
pub(crate) fn execute_prevalidated_document_automation(
    cx: &mut JSContext,
    document: &Document,
    request: &DocumentAutomationRequest,
) -> Result<DocumentAutomationResult, DocumentAutomationError> {
    request
        .validate()
        .map_err(DocumentAutomationError::InvalidRequest)?;
    let mut dom = ServoAutomationDom { cx, document };
    execute_operation(&mut dom, request.operation(), request.limits())
}

trait AutomationDom {
    type Element: Clone;

    fn query_document(
        &mut self,
        selector: &str,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure>;

    fn query_descendants(
        &mut self,
        root: &Self::Element,
        selector: &str,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure>;

    fn text_content(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<String, DocumentAutomationError>;

    fn inner_html(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<String, DocumentAutomationError>;

    fn fill(&mut self, element: &Self::Element, value: &str) -> Result<(), FillFailure>;

    fn activate(&mut self, element: &Self::Element) -> Result<(), ActivationFailure>;
}

struct ServoAutomationDom<'a> {
    cx: &'a mut JSContext,
    document: &'a Document,
}

impl ServoAutomationDom<'_> {
    #[allow(unsafe_code)]
    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    fn query_node(
        cx: &mut JSContext,
        root: &Node,
        selector: &str,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<DomRoot<Element>>, QueryFailure> {
        let document_url = root.owner_document().url().get_arc();
        let traced_node = UnrootedDom::from_dom(Dom::from_ref(root), cx.no_gc());
        let matching_elements = with_layout_state(|| {
            let layout_node: LayoutDom<'_, Node> = unsafe { traced_node.to_layout() };
            let root = ServoDangerousStyleNode::from(layout_node);
            let parsed_selector = parse_local_selector(selector, &UrlExtraData(document_url))?;

            let mut selector_caches = SelectorCaches::default();
            let mut context = MatchingContext::new(
                MatchingMode::Normal,
                None,
                &mut selector_caches,
                root.owner_doc().quirks_mode(),
                NeedsSelectorFlags::No,
                MatchingForInvalidation::No,
            );
            let root_element = root.as_element();
            context.scope_element = root_element.map(|element| element.opaque());
            context.current_host = root_element
                .and_then(|element| element.containing_shadow_host().map(|host| host.opaque()));

            // Charge the scope root even though querySelectorAll-style matching only considers
            // descendants. This gives every selector evaluation a non-zero cumulative cost and
            // bounds extraction plans which query many empty roots.
            work.visit_node()
                .map_err(QueryFailure::DomTraversalLimitExceeded)?;
            let mut matches = Vec::with_capacity(stop_after_matches as usize);
            for node in root.dom_descendants() {
                work.visit_node()
                    .map_err(QueryFailure::DomTraversalLimitExceeded)?;
                let Some(element) = node.as_element() else {
                    continue;
                };
                // Pre-charge the maximum number of selector-list arm/component evaluations for
                // this candidate. Local selectors cannot walk the tree, and this independent
                // counter prevents a large selector list from multiplying a bounded node walk.
                work.evaluate_selector(parsed_selector.evaluation_units)
                    .map_err(QueryFailure::SelectorEvaluationLimitExceeded)?;
                if matches_selector_list(&parsed_selector.list, &element, &mut context) {
                    matches.push(element);
                    if matches.len() >= stop_after_matches as usize {
                        break;
                    }
                }
            }
            Ok(matches)
        })?;

        Ok(matching_elements
            .into_iter()
            .map(ServoDangerousStyleElement::rooted)
            .collect())
    }
}

/// Parse the CSS subset whose match decision is local to one candidate element.
///
/// Combinators and pseudo-classes are deliberately rejected in 0.1 because selector matching for
/// them can traverse ancestors, siblings, or descendant subtrees behind the automation work
/// counter. Type, id, class, namespace, universal, and attribute components are local.
struct ParsedLocalSelector {
    list: SelectorList<SelectorImpl>,
    evaluation_units: u32,
}

fn parse_local_selector(
    selector: &str,
    url_data: &UrlExtraData,
) -> Result<ParsedLocalSelector, QueryFailure> {
    let selector_list = SelectorParser::parse_author_origin_no_namespace(selector, url_data)
        .map_err(|_| QueryFailure::InvalidSelector)?;
    let mut evaluation_units = 0u32;
    let is_local = selector_list.slice().iter().all(|selector| {
        evaluation_units = evaluation_units.saturating_add(1);
        selector.iter_raw_match_order().all(|component| {
            evaluation_units = evaluation_units.saturating_add(1);
            matches!(
                component,
                Component::LocalName(_) |
                    Component::ID(_) |
                    Component::Class(_) |
                    Component::AttributeInNoNamespaceExists { .. } |
                    Component::AttributeInNoNamespace { .. } |
                    Component::AttributeOther(_) |
                    Component::ExplicitUniversalType |
                    Component::ExplicitAnyNamespace |
                    Component::ExplicitNoNamespace |
                    Component::DefaultNamespace(_) |
                    Component::Namespace(_, _)
            )
        })
    });
    if !is_local {
        return Err(QueryFailure::UnsupportedSelector);
    }
    Ok(ParsedLocalSelector {
        list: selector_list,
        evaluation_units,
    })
}

impl AutomationDom for ServoAutomationDom<'_> {
    type Element = DomRoot<Element>;

    fn query_document(
        &mut self,
        selector: &str,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure> {
        Self::query_node(
            self.cx,
            self.document.upcast::<Node>(),
            selector,
            stop_after_matches,
            work,
        )
    }

    fn query_descendants(
        &mut self,
        root: &Self::Element,
        selector: &str,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure> {
        Self::query_node(
            self.cx,
            root.upcast::<Node>(),
            selector,
            stop_after_matches,
            work,
        )
    }

    fn text_content(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<String, DocumentAutomationError> {
        let mut value = String::new();
        for node in element
            .upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
        {
            work.visit_node().map_err(map_work_failure)?;
            if let Some(text) = node.downcast::<Text>() {
                let data = text.upcast::<CharacterData>().data();
                output.append(&mut value, &data)?;
            }
        }
        Ok(value)
    }

    fn inner_html(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<String, DocumentAutomationError> {
        let node = element.upcast::<Node>();
        let is_html_document = self.document.is_html_document();
        preflight_inner_html(self.cx, element, work, output, is_html_document)?;
        let qname = QualName::new(
            element.prefix().clone(),
            element.namespace().clone(),
            element.local_name().clone(),
        );
        let mut writer = BoundedOutputWriter::new(output);

        let serialization = if is_html_document {
            let traversal_scope = HtmlTraversalScope::ChildrenOnly(Some(qname));
            let mut serializer = HtmlSerializer::new(
                &mut writer,
                HtmlSerializeOpts {
                    traversal_scope: traversal_scope.clone(),
                    ..Default::default()
                },
            );
            serialize_html_fragment(
                self.cx,
                node,
                &mut serializer,
                traversal_scope,
                false,
                Vec::new(),
            )
        } else {
            let traversal_scope = XmlTraversalScope::ChildrenOnly(Some(qname));
            let mut serializer = XmlSerializer::new(&mut writer);
            HtmlSerialize::new(node).serialize(&mut serializer, traversal_scope)
        };

        writer.finish(serialization, DocumentAutomationOperationKind::InnerHtml)
    }

    fn fill(&mut self, element: &Self::Element, value: &str) -> Result<(), FillFailure> {
        if let Some(input) = element.downcast::<HTMLInputElement>() {
            {
                let input_type = input.input_type();
                if !input_type_supports_fill(&input_type) {
                    return Err(FillFailure::Unsupported);
                }
            }
            if !input.is_mutable() {
                return Err(FillFailure::Immutable);
            }
            input
                .SetValue(self.cx, DOMString::from(value))
                .map_err(|_| FillFailure::DomOperation)?;
        } else if let Some(textarea) = element.downcast::<HTMLTextAreaElement>() {
            if !textarea.is_mutable() {
                return Err(FillFailure::Immutable);
            }
            textarea.SetValue(self.cx, DOMString::from(value));
        } else {
            return Err(FillFailure::Unsupported);
        }

        // `fill` is one semantic value replacement followed by one synthetic replacement
        // InputEvent. `data` is the complete replacement (including Some("") for an empty
        // value), `inputType` is `insertReplacementText`, and the event bubbles and is composed
        // but is not cancelable. It deliberately does not synthesize keyboard or change events.
        let window = element.upcast::<Node>().owner_window();
        let event = InputEvent::new(
            self.cx,
            &window,
            None,
            atom!("input"),
            true,
            false,
            Some(&window),
            0,
            Some(DOMString::from(value)),
            false,
            DOMString::from("insertReplacementText"),
        );
        let event = event.upcast::<Event>();
        event.set_composed(true);
        event.fire(self.cx, element.upcast::<EventTarget>());
        Ok(())
    }

    fn activate(&mut self, element: &Self::Element) -> Result<(), ActivationFailure> {
        let html_element = element
            .downcast::<HTMLElement>()
            .ok_or(ActivationFailure::Unsupported)?;
        if element.disabled_state() {
            return Err(ActivationFailure::Disabled);
        }
        html_element.Click(self.cx);
        Ok(())
    }
}

/// Prove a hard upper bound on Servo's temporary serialization materialization before invoking
/// the existing HTML/XML serializers.
///
/// Servo's serializer batches direct children and clones an element's attributes before it calls
/// the output writer. This streaming preflight visits nodes without batching siblings, charges
/// every attribute entry to the cumulative work budget, and bounds all names and values which the
/// serializer may clone. The output writer still enforces the exact encoded byte limit.
fn preflight_inner_html(
    cx: &mut JSContext,
    element: &Element,
    work: &mut WorkBudget,
    output: &OutputBudget,
    is_html_document: bool,
) -> Result<(), DocumentAutomationError> {
    let mut allocation = output.fork();
    let mut encoded = output.fork();

    // The context qualified name is cloned before fragment serialization, even though the
    // context element itself is not included in the result.
    allocation.check(element.local_name().as_ref())?;
    allocation.check(element.namespace().as_ref())?;
    if let Some(prefix) = element.prefix().as_ref() {
        allocation.check(prefix.as_ref())?;
    }

    let container = if let Some(template) = element.downcast::<HTMLTemplateElement>() {
        DomRoot::upcast::<Node>(template.Content(cx))
    } else {
        DomRoot::from_ref(element.upcast::<Node>())
    };
    let mut template_contents = Vec::new();
    preflight_serialized_children(
        cx,
        &container,
        work,
        &mut allocation,
        &mut encoded,
        &mut template_contents,
        is_html_document,
    )?;

    while let Some(content) = template_contents.pop() {
        preflight_serialized_children(
            cx,
            &content,
            work,
            &mut allocation,
            &mut encoded,
            &mut template_contents,
            is_html_document,
        )?;
    }
    Ok(())
}

fn preflight_serialized_children(
    cx: &mut JSContext,
    container: &Node,
    work: &mut WorkBudget,
    allocation: &mut OutputBudget,
    encoded: &mut OutputBudget,
    template_contents: &mut Vec<DomRoot<Node>>,
    is_html_document: bool,
) -> Result<(), DocumentAutomationError> {
    for child in container.children() {
        let mut traversal = child.traverse_preorder(ShadowIncluding::No);
        while traversal.peek().is_some() {
            let skip_children = traversal
                .peek()
                .and_then(|node| node.downcast::<Element>())
                .is_some_and(|element| element.is_void() || element.is::<HTMLTemplateElement>());
            let node = if skip_children {
                traversal.next_skipping_children()
            } else {
                traversal.next()
            }
            .expect("a peeked tree iterator has a current node");

            preflight_serialized_node(&node, work, allocation, encoded, is_html_document)?;
            if let Some(template) = node.downcast::<HTMLTemplateElement>() {
                template_contents.push(DomRoot::upcast::<Node>(template.Content(cx)));
            }
        }
    }
    Ok(())
}

fn preflight_serialized_node(
    node: &Node,
    work: &mut WorkBudget,
    allocation: &mut OutputBudget,
    encoded: &mut OutputBudget,
    is_html_document: bool,
) -> Result<(), DocumentAutomationError> {
    work.visit_node().map_err(map_work_failure)?;

    if let Some(element) = node.downcast::<Element>() {
        let local_name = element.local_name().as_ref();
        let namespace = element.namespace().as_ref();
        allocation.check(local_name)?;
        allocation.check(namespace)?;
        let element_overhead = if is_html_document {
            5u64.saturating_add((local_name.len() as u64).saturating_mul(2))
        } else {
            14u64
                .saturating_add((local_name.len() as u64).saturating_mul(2))
                .saturating_add(namespace.len() as u64)
        };
        encoded.reserve_bytes(element_overhead)?;

        if !element.has_attribute(&LocalName::from("is")) &&
            let Some(is_value) = element.get_is()
        {
            work.visit_node().map_err(map_work_failure)?;
            allocation.check("is")?;
            allocation.check(is_value.as_ref())?;
            encoded.reserve_bytes(
                8u64.saturating_add(escaped_output_upper_bound(is_value.as_ref())),
            )?;
        }

        let attributes = element.attrs().borrow();
        for attribute in attributes.iter() {
            work.visit_node().map_err(map_work_failure)?;
            allocation.check(attribute.local_name().as_ref())?;
            allocation.check(attribute.namespace().as_ref())?;
            let value = attribute.value();
            let attribute_overhead = if is_html_document {
                22u64.saturating_add(attribute.local_name().len() as u64)
            } else {
                15u64
                    .saturating_add(attribute.local_name().len() as u64)
                    .saturating_add(attribute.namespace().len() as u64)
            };
            encoded.reserve_bytes(attribute_overhead)?;
            preflight_attribute_value(
                attribute.local_name().as_ref(),
                &value,
                allocation,
                encoded,
            )?;
        }
    }

    if let Some(data) = node.downcast::<CharacterData>() {
        let data = data.data();
        allocation.check(&data)?;
        encoded.reserve_bytes(16u64.saturating_add(escaped_output_upper_bound(&data)))?;
    }
    if let Some(instruction) = node.downcast::<ProcessingInstruction>() {
        allocation.reserve_len(instruction.target().len().saturating_mul(2))?;
        encoded.reserve_bytes(
            8u64.saturating_add((instruction.target().len() as u64).saturating_mul(2)),
        )?;
    }
    if let Some(doctype) = node.downcast::<DocumentType>() {
        allocation.reserve_len(doctype.name().len().saturating_mul(2))?;
        encoded
            .reserve_bytes(16u64.saturating_add((doctype.name().len() as u64).saturating_mul(2)))?;
    }
    Ok(())
}

/// Bound one serialized attribute value without forcing Stylo's lazy `AttrValue` variants to
/// materialize an unbounded `String` before the limit is known.
fn preflight_attribute_value(
    attribute_name: &str,
    value: &AttrValue,
    allocation: &mut OutputBudget,
    encoded: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    match value {
        AttrValue::String(value) |
        AttrValue::LengthPercentage(value, _) |
        AttrValue::Color(value, _) |
        AttrValue::Dimension(value, _) |
        AttrValue::ResolvedUrl(value, _) |
        AttrValue::ShadowParts(value, _) => {
            preflight_materialized_attribute_value(value, allocation, encoded)
        },
        AttrValue::Atom(value) => {
            preflight_materialized_attribute_value(value, allocation, encoded)
        },
        AttrValue::TokenList(serialization, tokens) => {
            if let Some(serialization) = serialization.get() {
                return preflight_materialized_attribute_value(serialization, allocation, encoded);
            }
            for (index, token) in tokens.iter().enumerate() {
                if index != 0 {
                    preflight_materialized_attribute_value(" ", allocation, encoded)?;
                }
                preflight_materialized_attribute_value(token, allocation, encoded)?;
            }
            Ok(())
        },
        AttrValue::UInt(serialization, value) => {
            if let Some(serialization) = serialization.get() {
                preflight_materialized_attribute_value(serialization, allocation, encoded)
            } else {
                // Formatting a primitive has a small type-defined upper bound, so producing this
                // temporary cannot bypass the document budget.
                let serialization = value.to_string();
                preflight_materialized_attribute_value(&serialization, allocation, encoded)
            }
        },
        AttrValue::Int(serialization, value) => {
            if let Some(serialization) = serialization.get() {
                preflight_materialized_attribute_value(serialization, allocation, encoded)
            } else {
                let serialization = value.to_string();
                preflight_materialized_attribute_value(&serialization, allocation, encoded)
            }
        },
        AttrValue::Double(serialization, value) => {
            if let Some(serialization) = serialization.get() {
                preflight_materialized_attribute_value(serialization, allocation, encoded)
            } else {
                let serialization = value.to_string();
                preflight_materialized_attribute_value(&serialization, allocation, encoded)
            }
        },
        AttrValue::Declaration {
            block,
            lock,
            serialization,
        } => {
            if let Some(serialization) = serialization.get() {
                return preflight_materialized_attribute_value(serialization, allocation, encoded);
            }

            // Stylo's declaration-block serializer currently requires `&mut String` rather than
            // a generic bounded writer. Materializing it here would recreate the allocation
            // bypass this preflight exists to prevent, so 0.1 rejects the uncommon lazy form
            // explicitly. A future generic Stylo writer hook can remove this safe feature cut.
            let _ = (block, lock);
            Err(
                DocumentAutomationError::UnsupportedLazyAttributeSerialization {
                    attribute: attribute_name.to_owned(),
                },
            )
        },
    }
}

fn preflight_materialized_attribute_value(
    value: &str,
    allocation: &mut OutputBudget,
    encoded: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    allocation.reserve_len(value.len())?;
    encoded.reserve_bytes(escaped_output_upper_bound(value))
}

fn escaped_output_upper_bound(value: &str) -> u64 {
    value.chars().fold(0u64, |bytes, character| {
        let encoded = match character {
            '&' => 5,
            '\u{00A0}' | '"' | '\'' => 6,
            '<' | '>' => 4,
            character => character.len_utf8() as u64,
        };
        bytes.saturating_add(encoded)
    })
}

fn input_type_supports_fill(input_type: &InputType) -> bool {
    matches!(
        input_type,
        InputType::Text(_) |
            InputType::Search(_) |
            InputType::Url(_) |
            InputType::Tel(_) |
            InputType::Email(_) |
            InputType::Password(_)
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FillFailure {
    Unsupported,
    Immutable,
    DomOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationFailure {
    Unsupported,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryFailure {
    InvalidSelector,
    UnsupportedSelector,
    DomTraversalLimitExceeded(WorkFailure),
    SelectorEvaluationLimitExceeded(WorkFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkFailure {
    observed: u64,
    limit: u32,
}

struct WorkBudget {
    visited: u64,
    selector_evaluations: u64,
    limit: u32,
}

impl WorkBudget {
    fn new(limits: DocumentAutomationLimits) -> Self {
        Self {
            visited: 0,
            selector_evaluations: 0,
            limit: limits.max_dom_nodes_visited(),
        }
    }

    fn visit_node(&mut self) -> Result<(), WorkFailure> {
        let observed = self.visited.saturating_add(1);
        if observed > u64::from(self.limit) {
            return Err(WorkFailure {
                observed,
                limit: self.limit,
            });
        }
        self.visited = observed;
        Ok(())
    }

    fn evaluate_selector(&mut self, units: u32) -> Result<(), WorkFailure> {
        let observed = self
            .selector_evaluations
            .checked_add(u64::from(units))
            .unwrap_or(u64::MAX);
        if observed > u64::from(self.limit) {
            return Err(WorkFailure {
                observed,
                limit: self.limit,
            });
        }
        self.selector_evaluations = observed;
        Ok(())
    }
}

fn execute_operation<D: AutomationDom>(
    dom: &mut D,
    operation: &DocumentAutomationOperation,
    limits: DocumentAutomationLimits,
) -> Result<DocumentAutomationResult, DocumentAutomationError> {
    operation
        .validate(limits)
        .map_err(DocumentAutomationError::InvalidRequest)?;

    match operation {
        DocumentAutomationOperation::QueryCount { selector } => {
            let mut work = WorkBudget::new(limits);
            let elements = query_document_bounded(dom, selector, limits, &mut work)?;
            let count = u32::try_from(elements.len()).expect("match limit is represented by u32");
            Ok(DocumentAutomationResult::QueryCount { count })
        },
        DocumentAutomationOperation::TextContent { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, &mut work)?;
            let mut output = OutputBudget::new(limits);
            let value = dom.text_content(&element, &mut work, &mut output)?;
            Ok(DocumentAutomationResult::TextContent { value })
        },
        DocumentAutomationOperation::InnerHtml { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, &mut work)?;
            let mut output = OutputBudget::new(limits);
            let value = dom.inner_html(&element, &mut work, &mut output)?;
            Ok(DocumentAutomationResult::InnerHtml { value })
        },
        DocumentAutomationOperation::Extract(plan) => {
            let mut work = WorkBudget::new(limits);
            let roots = query_document_bounded(dom, plan.root_selector(), limits, &mut work)?;
            let mut budget = OutputBudget::new(limits);
            let mut rows = Vec::with_capacity(roots.len());

            for (row_index, root) in roots.iter().enumerate() {
                let row = u32::try_from(row_index).expect("match limit is represented by u32");
                let mut values = Vec::with_capacity(plan.fields().len());
                for field in plan.fields() {
                    let matches = query_descendants_exact(dom, root, field.selector(), &mut work)?;
                    let element = match matches.len() {
                        0 => {
                            return Err(DocumentAutomationError::ExtractionFieldNotFound {
                                row,
                                field: field.name().to_owned(),
                                selector: field.selector().to_owned(),
                            });
                        },
                        1 => matches.into_iter().next().unwrap(),
                        count => {
                            return Err(DocumentAutomationError::ExtractionFieldAmbiguous {
                                row,
                                field: field.name().to_owned(),
                                selector: field.selector().to_owned(),
                                matches: count as u64,
                            });
                        },
                    };

                    budget.check(field.name())?;
                    let value = match field.read() {
                        DocumentExtractionRead::TextContent => {
                            dom.text_content(&element, &mut work, &mut budget)?
                        },
                        DocumentExtractionRead::InnerHtml => {
                            dom.inner_html(&element, &mut work, &mut budget)?
                        },
                    };
                    values.push(DocumentExtractionValue {
                        name: field.name().to_owned(),
                        value,
                    });
                }
                rows.push(DocumentExtractionRow { fields: values });
            }

            Ok(DocumentAutomationResult::Extract { rows })
        },
        DocumentAutomationOperation::Fill { selector, value } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, &mut work)?;
            dom.fill(&element, value).map_err(|failure| match failure {
                FillFailure::Unsupported => DocumentAutomationError::UnsupportedFillElement {
                    selector: selector.to_owned(),
                },
                FillFailure::Immutable => DocumentAutomationError::ImmutableFillElement {
                    selector: selector.to_owned(),
                },
                FillFailure::DomOperation => DocumentAutomationError::DomOperationFailed {
                    operation: DocumentAutomationOperationKind::Fill,
                },
            })?;
            Ok(DocumentAutomationResult::Filled)
        },
        DocumentAutomationOperation::Activate { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, &mut work)?;
            dom.activate(&element).map_err(|failure| match failure {
                ActivationFailure::Unsupported => {
                    DocumentAutomationError::UnsupportedActivationElement {
                        selector: selector.to_owned(),
                    }
                },
                ActivationFailure::Disabled => DocumentAutomationError::DisabledActivationElement {
                    selector: selector.to_owned(),
                },
            })?;
            Ok(DocumentAutomationResult::Activated)
        },
    }
}

fn query_document_bounded<D: AutomationDom>(
    dom: &mut D,
    selector: &str,
    limits: DocumentAutomationLimits,
    work: &mut WorkBudget,
) -> Result<Vec<D::Element>, DocumentAutomationError> {
    let elements = dom
        .query_document(selector, limits.max_matches().saturating_add(1), work)
        .map_err(|failure| map_query_failure(selector, failure))?;
    enforce_match_limit(selector, elements.len(), limits)?;
    Ok(elements)
}

fn query_descendants_exact<D: AutomationDom>(
    dom: &mut D,
    root: &D::Element,
    selector: &str,
    work: &mut WorkBudget,
) -> Result<Vec<D::Element>, DocumentAutomationError> {
    dom.query_descendants(root, selector, 2, work)
        .map_err(|failure| map_query_failure(selector, failure))
}

fn query_document_exact<D: AutomationDom>(
    dom: &mut D,
    selector: &str,
    work: &mut WorkBudget,
) -> Result<D::Element, DocumentAutomationError> {
    let elements = dom
        .query_document(selector, 2, work)
        .map_err(|failure| map_query_failure(selector, failure))?;
    match elements.len() {
        0 => Err(DocumentAutomationError::ElementNotFound {
            selector: selector.to_owned(),
        }),
        1 => Ok(elements.into_iter().next().unwrap()),
        matches => Err(DocumentAutomationError::SelectorAmbiguous {
            selector: selector.to_owned(),
            matches: matches as u64,
        }),
    }
}

fn map_query_failure(selector: &str, failure: QueryFailure) -> DocumentAutomationError {
    match failure {
        QueryFailure::InvalidSelector => DocumentAutomationError::InvalidSelector {
            selector: selector.to_owned(),
        },
        QueryFailure::UnsupportedSelector => DocumentAutomationError::UnsupportedSelector {
            selector: selector.to_owned(),
        },
        QueryFailure::DomTraversalLimitExceeded(failure) => map_work_failure(failure),
        QueryFailure::SelectorEvaluationLimitExceeded(failure) => {
            DocumentAutomationError::SelectorEvaluationLimitExceeded {
                observed: failure.observed,
                limit: failure.limit,
            }
        },
    }
}

fn map_work_failure(failure: WorkFailure) -> DocumentAutomationError {
    DocumentAutomationError::DomTraversalLimitExceeded {
        observed: failure.observed,
        limit: failure.limit,
    }
}

fn enforce_match_limit(
    selector: &str,
    observed: usize,
    limits: DocumentAutomationLimits,
) -> Result<(), DocumentAutomationError> {
    if observed as u64 > u64::from(limits.max_matches()) {
        return Err(DocumentAutomationError::MatchLimitExceeded {
            selector: selector.to_owned(),
            observed: observed as u64,
            limit: limits.max_matches(),
        });
    }
    Ok(())
}

struct OutputBudget {
    used: u64,
    limit: u64,
}

impl OutputBudget {
    fn new(limits: DocumentAutomationLimits) -> Self {
        Self {
            used: 0,
            limit: limits.max_output_bytes(),
        }
    }

    fn reserve_len(&mut self, len: usize) -> Result<(), DocumentAutomationError> {
        let additional = u64::try_from(len).unwrap_or(u64::MAX);
        self.reserve_bytes(additional)
    }

    fn reserve_bytes(&mut self, additional: u64) -> Result<(), DocumentAutomationError> {
        let attempted = self.used.checked_add(additional).unwrap_or(u64::MAX);
        if attempted > self.limit {
            return Err(DocumentAutomationError::OutputLimitExceeded {
                attempted,
                limit: self.limit,
            });
        }
        self.used = attempted;
        Ok(())
    }

    fn check(&mut self, value: &str) -> Result<(), DocumentAutomationError> {
        self.reserve_len(value.len())
    }

    fn fork(&self) -> Self {
        Self {
            used: self.used,
            limit: self.limit,
        }
    }

    fn append(&mut self, target: &mut String, value: &str) -> Result<(), DocumentAutomationError> {
        self.reserve_len(value.len())?;
        target.push_str(value);
        Ok(())
    }

    #[cfg(test)]
    fn copy(&mut self, value: &str) -> Result<String, DocumentAutomationError> {
        self.reserve_len(value.len())?;
        Ok(value.to_owned())
    }
}

struct BoundedOutputWriter<'a> {
    bytes: Vec<u8>,
    output: &'a mut OutputBudget,
    limit_error: Option<DocumentAutomationError>,
}

impl<'a> BoundedOutputWriter<'a> {
    fn new(output: &'a mut OutputBudget) -> Self {
        Self {
            bytes: Vec::new(),
            output,
            limit_error: None,
        }
    }

    fn finish(
        self,
        serialization: io::Result<()>,
        operation: DocumentAutomationOperationKind,
    ) -> Result<String, DocumentAutomationError> {
        if let Some(error) = self.limit_error {
            return Err(error);
        }
        serialization.map_err(|_| DocumentAutomationError::DomOperationFailed { operation })?;
        String::from_utf8(self.bytes)
            .map_err(|_| DocumentAutomationError::DomOperationFailed { operation })
    }
}

impl Write for BoundedOutputWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.limit_error.is_some() {
            return Err(io::Error::other("automation output limit exceeded"));
        }
        if let Err(error) = self.output.reserve_len(bytes.len()) {
            self.limit_error = Some(error);
            return Err(io::Error::other("automation output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use servo_arc::Arc;
    use url::Url;

    use super::*;

    #[derive(Clone)]
    struct FakeElement {
        text: String,
        html: String,
        fill: FakeFill,
        activation: FakeActivation,
    }

    #[derive(Clone, Copy)]
    enum FakeFill {
        Supported,
        Unsupported,
        Immutable,
    }

    #[derive(Clone, Copy)]
    enum FakeActivation {
        Supported,
        Unsupported,
        Disabled,
    }

    struct FakeDom {
        document_matches: Vec<FakeElement>,
        descendant_matches: Vec<FakeElement>,
        fill_calls: usize,
        activate_calls: usize,
        mutations: usize,
        input_events: usize,
        filled_value: Option<String>,
    }

    impl FakeDom {
        fn with_document_matches(document_matches: Vec<FakeElement>) -> Self {
            Self {
                document_matches,
                descendant_matches: Vec::new(),
                fill_calls: 0,
                activate_calls: 0,
                mutations: 0,
                input_events: 0,
                filled_value: None,
            }
        }
    }

    impl AutomationDom for FakeDom {
        type Element = FakeElement;

        fn query_document(
            &mut self,
            selector: &str,
            stop_after_matches: u32,
            work: &mut WorkBudget,
        ) -> Result<Vec<Self::Element>, QueryFailure> {
            if selector == "invalid[" {
                return Err(QueryFailure::InvalidSelector);
            }
            work.visit_node()
                .map_err(QueryFailure::DomTraversalLimitExceeded)?;
            let count = self.document_matches.len().min(stop_after_matches as usize);
            for _ in 0..count {
                work.visit_node()
                    .map_err(QueryFailure::DomTraversalLimitExceeded)?;
            }
            Ok(self.document_matches[..count].to_vec())
        }

        fn query_descendants(
            &mut self,
            _root: &Self::Element,
            selector: &str,
            stop_after_matches: u32,
            work: &mut WorkBudget,
        ) -> Result<Vec<Self::Element>, QueryFailure> {
            let matches = match selector {
                "invalid[" => return Err(QueryFailure::InvalidSelector),
                ".missing" => Vec::new(),
                ".ambiguous" => vec![element("a"), element("b")],
                _ => self.descendant_matches.clone(),
            };
            work.visit_node()
                .map_err(QueryFailure::DomTraversalLimitExceeded)?;
            let count = matches.len().min(stop_after_matches as usize);
            for _ in 0..count {
                work.visit_node()
                    .map_err(QueryFailure::DomTraversalLimitExceeded)?;
            }
            Ok(matches[..count].to_vec())
        }

        fn text_content(
            &mut self,
            element: &Self::Element,
            work: &mut WorkBudget,
            output: &mut OutputBudget,
        ) -> Result<String, DocumentAutomationError> {
            work.visit_node().map_err(map_work_failure)?;
            output.copy(&element.text)
        }

        fn inner_html(
            &mut self,
            element: &Self::Element,
            work: &mut WorkBudget,
            output: &mut OutputBudget,
        ) -> Result<String, DocumentAutomationError> {
            work.visit_node().map_err(map_work_failure)?;
            output.copy(&element.html)
        }

        fn fill(&mut self, element: &Self::Element, value: &str) -> Result<(), FillFailure> {
            self.fill_calls += 1;
            match element.fill {
                FakeFill::Supported => {
                    self.mutations += 1;
                    self.input_events += 1;
                    self.filled_value = Some(value.to_owned());
                    Ok(())
                },
                FakeFill::Unsupported => Err(FillFailure::Unsupported),
                FakeFill::Immutable => Err(FillFailure::Immutable),
            }
        }

        fn activate(&mut self, element: &Self::Element) -> Result<(), ActivationFailure> {
            self.activate_calls += 1;
            match element.activation {
                FakeActivation::Supported => {
                    self.mutations += 1;
                    Ok(())
                },
                FakeActivation::Unsupported => Err(ActivationFailure::Unsupported),
                FakeActivation::Disabled => Err(ActivationFailure::Disabled),
            }
        }
    }

    fn element(text: &str) -> FakeElement {
        FakeElement {
            text: text.to_owned(),
            html: format!("<b>{text}</b>"),
            fill: FakeFill::Supported,
            activation: FakeActivation::Supported,
        }
    }

    fn execute(
        dom: &mut FakeDom,
        operation: DocumentAutomationOperation,
    ) -> Result<DocumentAutomationResult, DocumentAutomationError> {
        execute_operation(dom, &operation, DocumentAutomationLimits::MVP)
    }

    #[test]
    fn invalid_zero_and_ambiguous_selectors_do_not_reach_actions() {
        let mut invalid = FakeDom::with_document_matches(vec![element("one")]);
        assert_eq!(
            execute(
                &mut invalid,
                DocumentAutomationOperation::Fill {
                    selector: "invalid[".to_owned(),
                    value: "value".to_owned(),
                },
            ),
            Err(DocumentAutomationError::InvalidSelector {
                selector: "invalid[".to_owned(),
            }),
        );
        assert_eq!((invalid.fill_calls, invalid.mutations), (0, 0));

        let mut zero = FakeDom::with_document_matches(Vec::new());
        assert_eq!(
            execute(
                &mut zero,
                DocumentAutomationOperation::Fill {
                    selector: "#field".to_owned(),
                    value: "value".to_owned(),
                },
            ),
            Err(DocumentAutomationError::ElementNotFound {
                selector: "#field".to_owned(),
            }),
        );
        assert_eq!((zero.fill_calls, zero.mutations), (0, 0));

        let mut ambiguous = FakeDom::with_document_matches(vec![element("a"), element("b")]);
        assert_eq!(
            execute(
                &mut ambiguous,
                DocumentAutomationOperation::Activate {
                    selector: ".button".to_owned(),
                },
            ),
            Err(DocumentAutomationError::SelectorAmbiguous {
                selector: ".button".to_owned(),
                matches: 2,
            }),
        );
        assert_eq!((ambiguous.activate_calls, ambiguous.mutations), (0, 0));
    }

    #[test]
    fn selector_subset_rejects_hidden_tree_traversal() {
        let url_data = UrlExtraData(Arc::new(Url::parse("http://fixture.local/").unwrap()));
        for selector in [
            "#save",
            ".result",
            "article.case[data-id='7']",
            "input[type=email], textarea",
        ] {
            assert!(
                parse_local_selector(selector, &url_data).is_ok(),
                "{selector}"
            );
        }

        for selector in [
            ".row .case",
            ".row > .case",
            ".row + .case",
            ":has(.case)",
            ":nth-child(2)",
            ":not(.disabled)",
        ] {
            assert!(
                matches!(
                    parse_local_selector(selector, &url_data),
                    Err(QueryFailure::UnsupportedSelector | QueryFailure::InvalidSelector),
                ),
                "{selector}"
            );
        }
        assert!(matches!(
            parse_local_selector("invalid[", &url_data),
            Err(QueryFailure::InvalidSelector),
        ));
    }

    #[test]
    fn selector_list_complexity_is_precharged_per_candidate() {
        let url_data = UrlExtraData(Arc::new(Url::parse("http://fixture.local/").unwrap()));
        let selector = parse_local_selector(".a, .b", &url_data).unwrap();
        assert!(selector.evaluation_units >= 4);

        let limit = selector.evaluation_units - 1;
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 4, 2, limit, 32).unwrap();
        let mut work = WorkBudget::new(limits);
        assert_eq!(
            work.evaluate_selector(selector.evaluation_units),
            Err(WorkFailure {
                observed: u64::from(selector.evaluation_units),
                limit,
            }),
        );
    }

    #[test]
    fn text_content_is_raw_and_does_not_apply_visibility_filtering() {
        let mut dom = FakeDom::with_document_matches(vec![element("hidden secret")]);
        assert_eq!(
            execute(
                &mut dom,
                DocumentAutomationOperation::TextContent {
                    selector: "[hidden]".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::TextContent {
                value: "hidden secret".to_owned(),
            }),
        );
        assert_eq!(dom.mutations, 0);
    }

    #[test]
    fn count_html_and_extract_preserve_document_and_plan_order() {
        let mut count = FakeDom::with_document_matches(vec![element("a"), element("b")]);
        assert_eq!(
            execute(
                &mut count,
                DocumentAutomationOperation::QueryCount {
                    selector: ".row".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::QueryCount { count: 2 }),
        );

        let mut html = FakeDom::with_document_matches(vec![element("markup")]);
        assert_eq!(
            execute(
                &mut html,
                DocumentAutomationOperation::InnerHtml {
                    selector: "#content".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::InnerHtml {
                value: "<b>markup</b>".to_owned(),
            }),
        );

        let fields = vec![
            embedder_traits::document_automation::DocumentExtractionField::new_internal(
                "case".to_owned(),
                ".case".to_owned(),
                DocumentExtractionRead::TextContent,
            ),
            embedder_traits::document_automation::DocumentExtractionField::new_internal(
                "markup".to_owned(),
                ".markup".to_owned(),
                DocumentExtractionRead::InnerHtml,
            ),
        ];
        let plan = embedder_traits::document_automation::DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            fields,
        );
        let mut extract = FakeDom::with_document_matches(vec![element("row")]);
        extract.descendant_matches = vec![element("value")];
        assert_eq!(
            execute(&mut extract, DocumentAutomationOperation::Extract(plan)),
            Ok(DocumentAutomationResult::Extract {
                rows: vec![DocumentExtractionRow {
                    fields: vec![
                        DocumentExtractionValue {
                            name: "case".to_owned(),
                            value: "value".to_owned(),
                        },
                        DocumentExtractionValue {
                            name: "markup".to_owned(),
                            value: "<b>value</b>".to_owned(),
                        },
                    ],
                }],
            }),
        );
        assert_eq!(extract.mutations, 0);
    }

    #[test]
    fn fill_replaces_value_and_emits_one_input_event() {
        let mut dom = FakeDom::with_document_matches(vec![element("")]);
        assert_eq!(
            execute(
                &mut dom,
                DocumentAutomationOperation::Fill {
                    selector: "#field".to_owned(),
                    value: "Garay".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::Filled),
        );
        assert_eq!(dom.filled_value.as_deref(), Some("Garay"));
        assert_eq!((dom.fill_calls, dom.input_events, dom.mutations), (1, 1, 1));
    }

    #[test]
    fn activate_invokes_native_activation_once_after_exact_match() {
        let mut dom = FakeDom::with_document_matches(vec![element("button")]);
        assert_eq!(
            execute(
                &mut dom,
                DocumentAutomationOperation::Activate {
                    selector: "#save".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::Activated),
        );
        assert_eq!((dom.activate_calls, dom.mutations), (1, 1));
    }

    #[test]
    fn disabled_activation_is_typed_and_does_not_dispatch() {
        let mut target = element("button");
        target.activation = FakeActivation::Disabled;
        let mut dom = FakeDom::with_document_matches(vec![target]);
        assert_eq!(
            execute(
                &mut dom,
                DocumentAutomationOperation::Activate {
                    selector: "#save".to_owned(),
                },
            ),
            Err(DocumentAutomationError::DisabledActivationElement {
                selector: "#save".to_owned(),
            }),
        );
        assert_eq!(dom.mutations, 0);
    }

    #[test]
    fn unsupported_activation_is_typed_and_does_not_dispatch() {
        let mut target = element("not activatable");
        target.activation = FakeActivation::Unsupported;
        let mut dom = FakeDom::with_document_matches(vec![target]);
        assert_eq!(
            execute(
                &mut dom,
                DocumentAutomationOperation::Activate {
                    selector: "#target".to_owned(),
                },
            ),
            Err(DocumentAutomationError::UnsupportedActivationElement {
                selector: "#target".to_owned(),
            }),
        );
        assert_eq!(dom.mutations, 0);
    }

    #[test]
    fn unsupported_and_immutable_fill_targets_do_not_mutate() {
        for (fill, expected) in [
            (
                FakeFill::Unsupported,
                DocumentAutomationError::UnsupportedFillElement {
                    selector: "#field".to_owned(),
                },
            ),
            (
                FakeFill::Immutable,
                DocumentAutomationError::ImmutableFillElement {
                    selector: "#field".to_owned(),
                },
            ),
        ] {
            let mut target = element("");
            target.fill = fill;
            let mut dom = FakeDom::with_document_matches(vec![target]);
            assert_eq!(
                execute(
                    &mut dom,
                    DocumentAutomationOperation::Fill {
                        selector: "#field".to_owned(),
                        value: "value".to_owned(),
                    },
                ),
                Err(expected),
            );
            assert_eq!((dom.input_events, dom.mutations), (0, 0));
        }
    }

    #[test]
    fn extraction_fields_have_typed_exact_one_failures() {
        for (selector, expected) in [
            (
                ".missing",
                DocumentAutomationError::ExtractionFieldNotFound {
                    row: 0,
                    field: "case".to_owned(),
                    selector: ".missing".to_owned(),
                },
            ),
            (
                ".ambiguous",
                DocumentAutomationError::ExtractionFieldAmbiguous {
                    row: 0,
                    field: "case".to_owned(),
                    selector: ".ambiguous".to_owned(),
                    matches: 2,
                },
            ),
        ] {
            let field = embedder_traits::document_automation::DocumentExtractionField::new_internal(
                "case".to_owned(),
                selector.to_owned(),
                DocumentExtractionRead::TextContent,
            );
            let plan = embedder_traits::document_automation::DocumentExtractionPlan::new_internal(
                ".row".to_owned(),
                vec![field],
            );
            let mut dom = FakeDom::with_document_matches(vec![element("row")]);
            assert_eq!(
                execute(&mut dom, DocumentAutomationOperation::Extract(plan)),
                Err(expected),
            );
            assert_eq!(dom.mutations, 0);
        }
    }

    #[test]
    fn matches_and_output_are_bounded() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 4, 1, 32, 4).unwrap();
        let mut too_many = FakeDom::with_document_matches(vec![element("a"), element("b")]);
        assert_eq!(
            execute_operation(
                &mut too_many,
                &DocumentAutomationOperation::QueryCount {
                    selector: ".row".to_owned(),
                },
                limits,
            ),
            Err(DocumentAutomationError::MatchLimitExceeded {
                selector: ".row".to_owned(),
                observed: 2,
                limit: 1,
            }),
        );

        let mut too_large = FakeDom::with_document_matches(vec![element("12345")]);
        assert_eq!(
            execute_operation(
                &mut too_large,
                &DocumentAutomationOperation::TextContent {
                    selector: "#x".to_owned(),
                },
                limits,
            ),
            Err(DocumentAutomationError::OutputLimitExceeded {
                attempted: 5,
                limit: 4,
            }),
        );
        assert_eq!(too_large.mutations, 0);
    }

    #[test]
    fn dom_traversal_limit_is_cumulative_across_query_and_read() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 4, 2, 2, 32).unwrap();
        let mut dom = FakeDom::with_document_matches(vec![element("value")]);
        assert_eq!(
            execute_operation(
                &mut dom,
                &DocumentAutomationOperation::TextContent {
                    selector: "#x".to_owned(),
                },
                limits,
            ),
            Err(DocumentAutomationError::DomTraversalLimitExceeded {
                observed: 3,
                limit: 2,
            }),
        );
    }

    #[test]
    fn extraction_queries_share_one_cumulative_work_budget() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 2, 2, 5, 32).unwrap();
        let fields = vec![
            embedder_traits::document_automation::DocumentExtractionField::new_internal(
                "a".to_owned(),
                ".value".to_owned(),
                DocumentExtractionRead::TextContent,
            ),
            embedder_traits::document_automation::DocumentExtractionField::new_internal(
                "b".to_owned(),
                ".value".to_owned(),
                DocumentExtractionRead::TextContent,
            ),
        ];
        let plan = embedder_traits::document_automation::DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            fields,
        );
        let mut dom = FakeDom::with_document_matches(vec![element("row")]);
        dom.descendant_matches = vec![element("value")];
        assert_eq!(
            execute_operation(
                &mut dom,
                &DocumentAutomationOperation::Extract(plan),
                limits,
            ),
            Err(DocumentAutomationError::DomTraversalLimitExceeded {
                observed: 6,
                limit: 5,
            }),
        );
    }

    #[test]
    fn bounded_writer_rejects_before_allocating_over_limit() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 4, 2, 32, 4).unwrap();
        let mut output = OutputBudget::new(limits);
        let mut writer = BoundedOutputWriter::new(&mut output);
        writer.write_all(b"1234").unwrap();
        let serialization = writer.write_all(b"5");
        assert!(serialization.is_err());
        assert_eq!(writer.bytes, b"1234");
        assert_eq!(
            writer.finish(serialization, DocumentAutomationOperationKind::InnerHtml,),
            Err(DocumentAutomationError::OutputLimitExceeded {
                attempted: 5,
                limit: 4,
            }),
        );
    }

    #[test]
    fn lazy_token_list_is_bounded_without_materializing_its_serialization() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 4, 2, 32, 8).unwrap();
        let value = AttrValue::TokenList(
            OnceLock::new(),
            vec![style::Atom::from("abcd"), style::Atom::from("efgh")],
        );
        let mut allocation = OutputBudget::new(limits);
        let mut encoded = OutputBudget::new(limits);
        assert_eq!(
            preflight_attribute_value("class", &value, &mut allocation, &mut encoded),
            Err(DocumentAutomationError::OutputLimitExceeded {
                attempted: 9,
                limit: 8,
            }),
        );
        let AttrValue::TokenList(serialization, _) = value else {
            unreachable!();
        };
        assert!(serialization.get().is_none());
    }

    #[test]
    fn fill_accepts_only_text_like_input_types() {
        assert!(input_type_supports_fill(&InputType::Text(
            Default::default()
        )));
        assert!(input_type_supports_fill(&InputType::Email(
            Default::default()
        )));
        assert!(!input_type_supports_fill(&InputType::Checkbox(
            Default::default(),
        )));
        assert!(!input_type_supports_fill(&InputType::Date(
            Default::default()
        )));
    }
}
