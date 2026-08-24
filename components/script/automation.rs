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

use std::collections::HashSet;
use std::io::{self, Write};

use embedder_traits::document_automation::{
    DocumentAutomationError, DocumentAutomationLimits, DocumentAutomationOperation,
    DocumentAutomationOperationKind, DocumentAutomationRequest, DocumentAutomationResult,
    DocumentExtractionRead, DocumentExtractionRow, DocumentExtractionValue,
    DocumentSelectorGrammar,
};
use html5ever::serialize::{
    HtmlSerializer, Serialize as _, SerializeOpts as HtmlSerializeOpts,
    TraversalScope as HtmlTraversalScope,
};
use html5ever::{LocalName, QualName, local_name, ns};
use js::context::JSContext;
use layout_api::with_layout_state;
use selectors::Element as _;
use selectors::attr::AttrSelectorOperator;
use selectors::matching::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches,
    matches_selector_list,
};
use selectors::parser::{Combinator, Component, SelectorList};
use style::attr::AttrValue;
use style::dom::{TDocument, TNode};
use style::selector_parser::{SelectorImpl, SelectorParser};
use style::str::{split_html_space_chars, str_join};
use style::stylesheets::UrlExtraData;
use xml5ever::serialize::{TraversalScope as XmlTraversalScope, XmlSerializer};

use crate::dom::bindings::codegen::Bindings::HTMLButtonElementBinding::HTMLButtonElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLElementBinding::HTMLElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLFormElementBinding::HTMLFormElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLInputElementBinding::HTMLInputElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLLabelElementBinding::HTMLLabelElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLOptionElementBinding::HTMLOptionElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLOrSVGElementBinding::FocusOptions;
use crate::dom::bindings::codegen::Bindings::HTMLSelectElementBinding::HTMLSelectElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTemplateElementBinding::HTMLTemplateElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTextAreaElementBinding::HTMLTextAreaElementMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::{GetRootNodeOptions, NodeMethods};
use crate::dom::bindings::codegen::Bindings::ShadowRootBinding::ShadowRoot_Binding::ShadowRootMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot, LayoutDom, ToLayout, UnrootedDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::characterdata::CharacterData;
use crate::dom::document::Document;
use crate::dom::documenttype::DocumentType;
use crate::dom::element::Element;
use crate::dom::event::{Event, EventBubbles, EventCancelable, EventComposed};
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::form_controls::htmlbuttonelement::HTMLButtonElement;
use crate::dom::html::form_controls::htmlfieldsetelement::HTMLFieldSetElement;
use crate::dom::html::form_controls::htmlformelement::{
    FormControl, FormControlElementHelpers, HTMLFormElement,
};
use crate::dom::html::form_controls::htmlinputelement::HTMLInputElement;
use crate::dom::html::form_controls::htmllabelelement::HTMLLabelElement;
use crate::dom::html::form_controls::htmloptgroupelement::HTMLOptGroupElement;
use crate::dom::html::form_controls::htmloptionelement::HTMLOptionElement;
use crate::dom::html::form_controls::htmloutputelement::HTMLOutputElement;
use crate::dom::html::form_controls::htmlselectelement::HTMLSelectElement;
use crate::dom::html::form_controls::htmltextareaelement::HTMLTextAreaElement;
use crate::dom::html::form_controls::input_type::InputType;
use crate::dom::html::form_controls::input_type::radio_input_type::in_same_group;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::htmlscriptelement::HTMLScriptElement;
use crate::dom::html::htmltemplateelement::HTMLTemplateElement;
use crate::dom::inputevent::InputEvent;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::processinginstruction::ProcessingInstruction;
use crate::dom::servoparser::html::HtmlSerialize;
use crate::dom::servoparser::serialize_html_fragment;
use crate::dom::shadowroot::ShadowRoot;
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
    execute_operation(
        &mut dom,
        request.operation(),
        request.limits(),
        request.selector_grammar(),
    )
}

trait AutomationDom {
    type Element: Clone;

    fn query_document(
        &mut self,
        selector: &str,
        grammar: DocumentSelectorGrammar,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure>;

    fn query_descendants(
        &mut self,
        root: &Self::Element,
        selector: &str,
        grammar: DocumentSelectorGrammar,
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

    fn attribute(
        &mut self,
        element: &Self::Element,
        name: &str,
        resolve_url: bool,
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<Option<String>, DocumentAutomationError>;

    fn fill(
        &mut self,
        element: &Self::Element,
        value: &str,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<(), FillFailure>;

    fn activate(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<(), ActivationFailure>;

    fn set_checked(
        &mut self,
        element: &Self::Element,
        checked: bool,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<CheckedState, CheckFailure>;

    fn select(
        &mut self,
        element: &Self::Element,
        values: &[String],
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<SelectedState, SelectFailure>;

    fn focus(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<bool, FocusFailure>;

    fn submit(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<(), SubmitFailure>;
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
        grammar: DocumentSelectorGrammar,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<DomRoot<Element>>, QueryFailure> {
        let document_url = root.owner_document().url().get_arc();
        let traced_node = UnrootedDom::from_dom(Dom::from_ref(root), cx.no_gc());
        let matching_elements = with_layout_state(|| {
            let layout_node: LayoutDom<'_, Node> = unsafe { traced_node.to_layout() };
            let root = ServoDangerousStyleNode::from(layout_node);
            let parsed_selector =
                parse_bounded_selector(selector, grammar, &UrlExtraData(document_url))?;

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
                // this candidate. Structural selectors also charge their ancestor walk, and this
                // independent counter prevents a selector list from multiplying bounded work.
                let ancestor_facts = selector_ancestor_facts(element, &parsed_selector, work)?;
                work.evaluate_selector(parsed_selector.evaluation_units_for_depth(
                    ancestor_facts.depth,
                    ancestor_facts.attribute_entries,
                    ancestor_facts.attribute_value_bytes,
                ))
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SelectorAncestorFacts {
    depth: u32,
    attribute_entries: u64,
    attribute_value_bytes: u64,
}

fn selector_ancestor_facts(
    mut element: ServoDangerousStyleElement<'_>,
    selector: &ParsedLocalSelector,
    work: &mut WorkBudget,
) -> Result<SelectorAncestorFacts, QueryFailure> {
    let walk_complete_chain = selector
        .arms
        .iter()
        .any(|arm| arm.descendant_combinators != 0);
    let maximum_child_depth = selector
        .arms
        .iter()
        .map(|arm| arm.child_combinators)
        .max()
        .unwrap_or(0);
    let count_attributes = selector
        .arms
        .iter()
        .any(|arm| arm.attribute_components != 0);
    let count_attribute_values = !selector.equality_attribute_names.is_empty();
    let (candidate_attribute_entries, candidate_attribute_value_bytes) =
        if count_attributes || count_attribute_values {
            selector_attribute_facts(element, &selector.equality_attribute_names)?
        } else {
            (0, 0)
        };
    let mut facts = SelectorAncestorFacts {
        depth: 0,
        attribute_entries: candidate_attribute_entries,
        attribute_value_bytes: candidate_attribute_value_bytes,
    };
    while (walk_complete_chain || facts.depth < maximum_child_depth)
        && let Some(parent) = element.parent_element()
    {
        work.visit_node()
            .map_err(QueryFailure::DomTraversalLimitExceeded)?;
        facts.depth = facts.depth.saturating_add(1);
        element = parent;
        if count_attributes || count_attribute_values {
            let (attribute_entries, attribute_value_bytes) =
                selector_attribute_facts(element, &selector.equality_attribute_names)?;
            facts.attribute_entries = facts.attribute_entries.saturating_add(attribute_entries);
            facts.attribute_value_bytes = facts
                .attribute_value_bytes
                .saturating_add(attribute_value_bytes);
        }
    }
    Ok(facts)
}

fn selector_attribute_facts(
    element: ServoDangerousStyleElement<'_>,
    equality_attribute_names: &HashSet<LocalName>,
) -> Result<(u64, u64), QueryFailure> {
    // This helper runs inside `with_layout_state`; rooting the layout element here would violate
    // the Script-thread access assertion. Use the layout-safe attribute accessors and root only
    // the final matched elements after leaving the layout-state closure.
    let mut entries = 0u64;
    let mut value_bytes = 0u64;
    let mut unsupported_lazy_value = false;
    element
        .element
        .each_attr_for_layout(|namespace, name, value| {
            entries = entries.saturating_add(1);
            if namespace != &ns!() || !equality_attribute_names.contains(name) {
                return;
            }
            let Some(bytes) = selector_attribute_value_upper_bound(value) else {
                // Stylo would lazily allocate an unbounded CSS declaration serialization while
                // matching `[style=...]`. Until Stylo accepts a bounded writer, reject this exact
                // DOM state before entering the matcher.
                unsupported_lazy_value = true;
                return;
            };
            value_bytes = value_bytes.saturating_add(bytes);
        });
    if unsupported_lazy_value {
        return Err(QueryFailure::UnsupportedSelector);
    }
    Ok((entries, value_bytes))
}

fn selector_attribute_value_upper_bound(value: &AttrValue) -> Option<u64> {
    let bytes = match value {
        AttrValue::String(value)
        | AttrValue::LengthPercentage(value, _)
        | AttrValue::Color(value, _)
        | AttrValue::Dimension(value, _)
        | AttrValue::ResolvedUrl(value, _)
        | AttrValue::ShadowParts(value, _) => value.len() as u64,
        AttrValue::Atom(value) => value.len() as u64,
        AttrValue::TokenList(serialization, tokens) => serialization
            .get()
            .map(|value| value.len() as u64)
            .unwrap_or_else(|| {
                tokens
                    .iter()
                    .fold(0u64, |bytes, token| {
                        bytes.saturating_add(token.len() as u64)
                    })
                    .saturating_add(tokens.len().saturating_sub(1) as u64)
            }),
        AttrValue::UInt(serialization, _) => serialization
            .get()
            .map(|value| value.len() as u64)
            .unwrap_or(10),
        AttrValue::Int(serialization, _) => serialization
            .get()
            .map(|value| value.len() as u64)
            .unwrap_or(11),
        AttrValue::Double(serialization, _) => serialization
            .get()
            .map(|value| value.len() as u64)
            // Rust's shortest round-trip f64 formatting is well below this fixed bound.
            .unwrap_or(64),
        AttrValue::Declaration { serialization, .. } => {
            serialization.get().map(|value| value.len() as u64)?
        },
    };
    Some(bytes)
}

/// Parse the CSS subset whose hidden matcher traversal has an explicit conservative upper bound.
///
/// Local components remain constant-cost except admitted no-namespace attribute presence
/// selectors, which precharge candidate/ancestor attribute entries. `>` takes one parent step,
/// and every descendant combinator is precharged by the candidate's exact ancestor depth.
/// Attribute-value operators, namespace-generic attributes, sibling combinators, pseudo-classes,
/// pseudo-elements, and nested selector lists remain rejected: admitting any of them without
/// extending the structural and byte-cost model would bypass [`WorkBudget`].
struct ParsedLocalSelector {
    list: SelectorList<SelectorImpl>,
    arms: Vec<SelectorArmCost>,
    equality_attribute_names: HashSet<LocalName>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SelectorArmCost {
    local_units: u64,
    child_combinators: u32,
    descendant_combinators: u32,
    attribute_components: u32,
    equality_components: u32,
}

impl ParsedLocalSelector {
    fn evaluation_units_for_depth(
        &self,
        depth: u32,
        attribute_entries: u64,
        attribute_value_bytes: u64,
    ) -> u64 {
        self.arms.iter().fold(0u64, |total, arm| {
            let structural_choices = u64::from(depth)
                .saturating_add(1)
                .saturating_pow(arm.descendant_combinators);
            let fixed_steps = u64::from(arm.child_combinators).saturating_add(1);
            // Servo stores element attributes in a vector. Even no-namespace existence/equality
            // matching can therefore inspect every entry on each structurally considered element.
            // Charge the complete candidate/ancestor attribute inventory for every admitted
            // attribute component before entering the matcher.
            let local_units = arm
                .local_units
                .saturating_add(
                    attribute_entries.saturating_mul(u64::from(arm.attribute_components)),
                )
                .saturating_add(
                    attribute_value_bytes.saturating_mul(u64::from(arm.equality_components)),
                );
            total.saturating_add(
                local_units
                    .saturating_mul(fixed_steps)
                    .saturating_mul(structural_choices),
            )
        })
    }
}

fn parse_bounded_selector(
    selector: &str,
    grammar: DocumentSelectorGrammar,
    url_data: &UrlExtraData,
) -> Result<ParsedLocalSelector, QueryFailure> {
    let selector_list = SelectorParser::parse_author_origin_no_namespace(selector, url_data)
        .map_err(|_| QueryFailure::InvalidSelector)?;
    let mut arms = Vec::with_capacity(selector_list.slice().len());
    let mut equality_attribute_names = HashSet::new();
    for selector in selector_list.slice() {
        let mut cost = SelectorArmCost {
            local_units: 1,
            child_combinators: 0,
            descendant_combinators: 0,
            attribute_components: 0,
            equality_components: 0,
        };
        for component in selector.iter_raw_match_order() {
            match component {
                Component::LocalName(_)
                | Component::ID(_)
                | Component::Class(_)
                | Component::ExplicitUniversalType
                | Component::ExplicitAnyNamespace
                | Component::ExplicitNoNamespace
                | Component::DefaultNamespace(_)
                | Component::Namespace(_, _) => {
                    cost.local_units = cost.local_units.saturating_add(1);
                },
                Component::AttributeInNoNamespaceExists { .. } => {
                    cost.local_units = cost.local_units.saturating_add(1);
                    cost.attribute_components = cost.attribute_components.saturating_add(1);
                },
                Component::AttributeInNoNamespace {
                    local_name,
                    operator: AttrSelectorOperator::Equal,
                    ..
                } => {
                    cost.local_units = cost.local_units.saturating_add(1);
                    cost.attribute_components = cost.attribute_components.saturating_add(1);
                    cost.equality_components = cost.equality_components.saturating_add(1);
                    equality_attribute_names.insert(local_name.0.clone());
                },
                // Every value operator, including equality, can compare an arbitrarily large,
                // page-controlled attribute value. Namespace-generic selectors can additionally
                // scan multiple matching attributes. Neither is part of the frozen bounded v2
                // grammar until value bytes and attribute candidates are explicitly accounted.
                Component::AttributeInNoNamespace { .. } | Component::AttributeOther(_) => {
                    return Err(QueryFailure::UnsupportedSelector);
                },
                Component::Combinator(Combinator::Child)
                    if grammar == DocumentSelectorGrammar::PracticalV2 =>
                {
                    cost.child_combinators = cost.child_combinators.saturating_add(1);
                },
                Component::Combinator(Combinator::Descendant)
                    if grammar == DocumentSelectorGrammar::PracticalV2 =>
                {
                    cost.descendant_combinators = cost.descendant_combinators.saturating_add(1);
                },
                _ => return Err(QueryFailure::UnsupportedSelector),
            }
        }
        arms.push(cost);
    }
    Ok(ParsedLocalSelector {
        list: selector_list,
        arms,
        equality_attribute_names,
    })
}

impl AutomationDom for ServoAutomationDom<'_> {
    type Element = DomRoot<Element>;

    fn query_document(
        &mut self,
        selector: &str,
        grammar: DocumentSelectorGrammar,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure> {
        Self::query_node(
            self.cx,
            self.document.upcast::<Node>(),
            selector,
            grammar,
            stop_after_matches,
            work,
        )
    }

    fn query_descendants(
        &mut self,
        root: &Self::Element,
        selector: &str,
        grammar: DocumentSelectorGrammar,
        stop_after_matches: u32,
        work: &mut WorkBudget,
    ) -> Result<Vec<Self::Element>, QueryFailure> {
        Self::query_node(
            self.cx,
            root.upcast::<Node>(),
            selector,
            grammar,
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
        bounded_text_content(element, work, output)
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

    fn attribute(
        &mut self,
        element: &Self::Element,
        name: &str,
        resolve_url: bool,
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<Option<String>, DocumentAutomationError> {
        work.visit_node().map_err(map_work_failure)?;
        let mut temporary = output.fork();
        let Some(raw) = bounded_raw_attribute(element, name, work, &mut temporary)? else {
            return Ok(None);
        };
        if !resolve_url {
            output.check(&raw)?;
            return Ok(Some(raw));
        }

        let base = self.document.base_url();
        // URL parsing can percent-encode bytes and join the base path. Prove a conservative
        // allocation bound before asking `url` to materialize the resolved string.
        let upper_bound = (base.as_str().len() as u64)
            .saturating_add(raw.len() as u64)
            .saturating_mul(3);
        let mut resolved_temporary = output.fork();
        resolved_temporary.reserve_bytes(upper_bound)?;
        let Ok(resolved) = base.join(&raw) else {
            return Ok(None);
        };
        let resolved = resolved.to_string();
        output.check(&resolved)?;
        Ok(Some(resolved))
    }

    fn fill(
        &mut self,
        element: &Self::Element,
        value: &str,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<(), FillFailure> {
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
            scratch.check(value).map_err(FillFailure::Automation)?;
            reserve_semantic_action_hidden_work(
                self.document,
                element,
                SemanticActionWork::Fill,
                work,
                scratch,
            )
            .map_err(FillFailure::Automation)?;
            input
                .SetValue(self.cx, DOMString::from(value))
                .map_err(|_| FillFailure::DomOperation)?;
        } else if let Some(textarea) = element.downcast::<HTMLTextAreaElement>() {
            if !textarea.is_mutable() {
                return Err(FillFailure::Immutable);
            }
            scratch.check(value).map_err(FillFailure::Automation)?;
            reserve_semantic_action_hidden_work(
                self.document,
                element,
                SemanticActionWork::Fill,
                work,
                scratch,
            )
            .map_err(FillFailure::Automation)?;
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

    fn activate(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<(), ActivationFailure> {
        let html_element = element
            .downcast::<HTMLElement>()
            .ok_or(ActivationFailure::Unsupported)?;
        if element.disabled_state() {
            return Err(ActivationFailure::Disabled);
        }
        reserve_semantic_action_hidden_work(
            self.document,
            element,
            SemanticActionWork::Activate,
            work,
            scratch,
        )
        .map_err(ActivationFailure::Automation)?;
        html_element.Click(self.cx);
        Ok(())
    }

    fn set_checked(
        &mut self,
        element: &Self::Element,
        checked: bool,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<CheckedState, CheckFailure> {
        let input = element
            .downcast::<HTMLInputElement>()
            .ok_or(CheckFailure::Unsupported)?;
        let input_type = input.input_type();
        let is_checkbox = matches!(*input_type, InputType::Checkbox(_));
        let is_radio = matches!(*input_type, InputType::Radio(_));
        if !is_checkbox && !(checked && is_radio) {
            return Err(CheckFailure::Unsupported);
        }
        drop(input_type);
        if !input.is_mutable() {
            return Err(CheckFailure::Immutable);
        }

        let before = input.Checked();
        if before != checked {
            reserve_semantic_action_hidden_work(
                self.document,
                element,
                if is_radio {
                    SemanticActionWork::Radio
                } else {
                    SemanticActionWork::Checkbox
                },
                work,
                scratch,
            )
            .map_err(CheckFailure::Automation)?;
            element
                .downcast::<HTMLElement>()
                .expect("every HTML input is an HTMLElement")
                .Click(self.cx);
        }
        let observed = input.Checked();
        Ok(CheckedState {
            changed: before != observed,
            checked: observed,
        })
    }

    fn select(
        &mut self,
        element: &Self::Element,
        values: &[String],
        work: &mut WorkBudget,
        output: &mut OutputBudget,
    ) -> Result<SelectedState, SelectFailure> {
        let select = element
            .downcast::<HTMLSelectElement>()
            .ok_or(SelectFailure::Unsupported)?;
        if element.disabled_state() {
            return Err(SelectFailure::Immutable);
        }
        let multiple = select.Multiple();
        if !multiple && values.len() != 1 {
            return Err(SelectFailure::InvalidMultiplicity {
                multiple,
                requested: u32::try_from(values.len()).unwrap_or(u32::MAX),
            });
        }

        let requested: HashSet<&str> = values.iter().map(String::as_str).collect();
        let collected = collect_select_options(select, work).map_err(SelectFailure::Automation)?;
        let mut temporary = output.fork();
        let mut options: Vec<(DomRoot<HTMLOptionElement>, String, bool)> = Vec::new();
        for option in collected.options {
            let value = bounded_option_value(&option, work, &mut temporary)
                .map_err(SelectFailure::Automation)?;
            let disabled = option.upcast::<Element>().disabled_state();
            options.push((option, value, disabled));
        }

        let mut chosen_index = None;
        for requested_value in values {
            let mut saw_disabled = false;
            let mut saw_enabled = false;
            for (index, (_, value, disabled)) in options.iter().enumerate() {
                if value != requested_value {
                    continue;
                }
                if *disabled {
                    saw_disabled = true;
                } else {
                    saw_enabled = true;
                    chosen_index.get_or_insert(index);
                }
            }
            if !saw_enabled {
                return Err(if saw_disabled {
                    SelectFailure::ValueDisabled(requested_value.to_owned())
                } else {
                    SelectFailure::ValueNotFound(requested_value.to_owned())
                });
            }
        }

        let before: Vec<bool> = options
            .iter()
            .map(|(option, _, _)| option.Selected())
            .collect();
        let desired: Vec<bool> = if multiple {
            options
                .iter()
                .map(|(_, value, disabled)| !*disabled && requested.contains(value.as_str()))
                .collect()
        } else {
            let index = chosen_index.expect("a single-select value was prevalidated");
            (0..options.len())
                .map(|candidate| candidate == index)
                .collect()
        };

        let changed = before != desired;
        if !changed {
            // Preserve the option-setter dirtiness semantics without invoking its repeated
            // reset/validity scans. Build and check the complete result before even this internal
            // mutation so every possible non-success remains definitively non-mutating.
            let mut selected = Vec::new();
            for ((_, value, _), selectedness) in options.iter().zip(&desired) {
                if *selectedness {
                    output.check(&value).map_err(SelectFailure::Automation)?;
                    selected.push(value.clone());
                }
            }

            if multiple {
                for (option, _, _) in &options {
                    option.set_dirtiness(true);
                }
            } else {
                let index = chosen_index.expect("a single-select value was prevalidated");
                options[index].0.set_dirtiness(true);
            }
            return Ok(SelectedState {
                changed: false,
                values: selected,
            });
        }

        // The bulk DOM update below performs one validity refresh. Its select validity algorithm
        // can scan the option tree twice (placeholder lookup plus selected-option search), so
        // conservatively reserve both complete scans before crossing the mutation point.
        reserve_select_validity_scans(work, collected.inspected_nodes)
            .map_err(map_work_failure)
            .map_err(SelectFailure::Automation)?;
        reserve_select_hidden_mutation_work(&element.owner_document(), work)
            .map_err(SelectFailure::Automation)?;
        reserve_select_version_bump(select, work)
            .map_err(map_work_failure)
            .map_err(SelectFailure::Automation)?;

        let selected_count = desired.iter().filter(|selected| **selected).count();
        let displayed_text = if selected_count == 1 {
            let index = desired
                .iter()
                .position(|selected| *selected)
                .expect("one desired option was counted");
            let mut display_budget = output.fresh();
            bounded_option_displayed_label(&options[index].0, work, &mut display_budget)
                .map_err(SelectFailure::Automation)?
        } else {
            format!("{selected_count} selected")
        };

        // Required single-select validity can inspect the first option's value while finding its
        // placeholder. Reserve that attribute lookup and possible option-text traversal before
        // selectedness changes; other validity option-list passes were reserved above.
        if let Some((first, _, _)) = options.first() {
            reserve_option_value_lookup(first, work)
                .map_err(map_work_failure)
                .map_err(SelectFailure::Automation)?;
        }

        // A changed semantic select fires one `input` and one `change` event. Charge the exact
        // composed target path for both before selectedness changes, just like every other
        // mutating semantic action; unrelated document size remains irrelevant to this term.
        let mut event_derived = output.fresh();
        let event_path =
            inspect_composed_event_path(element, false, work, output, &mut event_derived)
                .map_err(SelectFailure::Automation)?;
        work.visit_nodes(event_dispatch_units(&event_path, None).saturating_mul(2))
            .map_err(map_work_failure)
            .map_err(SelectFailure::Automation)?;

        if multiple {
            for (index, (option, _, _)) in options.iter().enumerate() {
                option.set_dirtiness(true);
                if before[index] != desired[index] {
                    option.set_selectedness_for_automation(desired[index]);
                }
            }
        } else {
            let chosen = chosen_index.expect("a single-select value was prevalidated");
            options[chosen].0.set_dirtiness(true);
            for (index, (option, _, _)) in options.iter().enumerate() {
                if before[index] != desired[index] {
                    option.set_selectedness_for_automation(desired[index]);
                }
            }
        }
        select.upcast::<Node>().rev_version(self.cx.no_gc());
        select.finish_automation_selection(self.cx, &displayed_text);

        let target = select.upcast::<EventTarget>();
        target.fire_event_with_params(
            self.cx,
            atom!("input"),
            EventBubbles::Bubbles,
            EventCancelable::NotCancelable,
            EventComposed::Composed,
        );
        target.fire_bubbling_event(self.cx, atom!("change"));

        // Event handlers run synchronously and may replace the option tree or alter values. Return
        // the current target state in current DOM order, never the pre-event captured handles.
        let current =
            collect_select_options(select, work).map_err(SelectFailure::PostMutationAutomation)?;
        let mut selected = Vec::new();
        for option in current.options {
            if option.Selected() {
                let value = bounded_option_value(&option, work, output)
                    .map_err(SelectFailure::PostMutationAutomation)?;
                selected.push(value);
            }
        }
        Ok(SelectedState {
            changed,
            values: selected,
        })
    }

    fn focus(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<bool, FocusFailure> {
        let html_element = element
            .downcast::<HTMLElement>()
            .ok_or(FocusFailure::Unsupported)?;
        reserve_semantic_action_hidden_work(
            self.document,
            element,
            SemanticActionWork::Focus,
            work,
            scratch,
        )
        .map_err(FocusFailure::Automation)?;
        let document = element.owner_document();
        let _semantic_focus = document
            .embedder_controls()
            .begin_semantic_automation_focus();
        html_element.Focus(
            self.cx,
            &FocusOptions {
                preventScroll: true,
            },
        );
        Ok(element.focus_state())
    }

    fn submit(
        &mut self,
        element: &Self::Element,
        work: &mut WorkBudget,
        scratch: &mut OutputBudget,
    ) -> Result<(), SubmitFailure> {
        let form = element
            .downcast::<HTMLFormElement>()
            .ok_or(SubmitFailure::Unsupported)?;
        reserve_semantic_action_hidden_work(
            self.document,
            element,
            SemanticActionWork::Submit,
            work,
            scratch,
        )
        .map_err(SubmitFailure::Automation)?;
        form.RequestSubmit(self.cx, None)
            .map_err(|_| SubmitFailure::DomOperation)
    }
}

/// Conservative native-work classes for semantic actions.
///
/// These reservations cover only Servo's synchronous native algorithms against the DOM state
/// observed before the action. Page JavaScript runs under the command wall-time boundary; if an
/// event handler changes the page or exhausts that boundary, the mutating command is fail-stop and
/// indeterminate rather than being reclassified as a definitive preflight rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SemanticActionWork {
    Fill,
    Activate,
    Checkbox,
    Radio,
    Focus,
    Submit,
}

const SEMANTIC_ACTION_FIXED_WORK: u64 = 256;
// The generated multipart boundary is at most 47 bytes. A file entry with empty strings and the
// `text/plain` fallback therefore has 135 bytes of fixed boundary/header/trailer output; string
// entries use less. The final closing boundary is at most 53 bytes. Page-controlled strings and
// file bodies are reserved separately below.
const FORM_SUBMISSION_DERIVED_BYTES_PER_ENTRY: u64 = 135;
const FORM_SUBMISSION_DERIVED_FINAL_BYTES: u64 = 53;

fn base_form_submission_entries(control_count: u64) -> u64 {
    control_count.saturating_mul(2)
}

fn form_submission_entry_excess(actual_entries: u64) -> u64 {
    actual_entries.saturating_sub(1)
}

fn form_submission_repeated_name_bytes(name_bytes: u64, actual_entries: u64) -> u64 {
    name_bytes.saturating_mul(actual_entries.saturating_sub(1))
}

fn reserve_form_submission_entries(
    entries: u64,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    // One native-work unit and one raw-cardinality byte keep empty entries bounded even before
    // their strings/bodies are inspected. The fixed multipart envelope is the maximum across the
    // supported encodings; actual page-controlled source bytes are charged separately.
    work.visit_nodes(entries).map_err(map_work_failure)?;
    scratch.reserve_bytes(entries)?;
    derived.reserve_bytes(entries.saturating_mul(FORM_SUBMISSION_DERIVED_BYTES_PER_ENTRY))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormActionWork {
    Submit,
    Reset,
}

struct EventPathFacts {
    path_nodes: u64,
    ancestor_depth_sum: u64,
    target_depth: u64,
    shadow_retarget_hops: u64,
    activation_target: Option<DomRoot<Element>>,
}

impl EventPathFacts {
    fn viewport() -> Self {
        Self {
            path_nodes: 1,
            ancestor_depth_sum: 1,
            target_depth: 1,
            shadow_retarget_hops: 0,
            activation_target: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ControlValidityFacts {
    form_controls: u64,
    ancestor_nodes: u64,
    fieldset_nodes: u64,
}

impl ControlValidityFacts {
    fn native_units(self) -> u64 {
        self.form_controls
            .saturating_add(self.ancestor_nodes)
            .saturating_add(self.fieldset_nodes)
    }

    fn native_units_after_form_scan(self) -> u64 {
        self.ancestor_nodes.saturating_add(self.fieldset_nodes)
    }
}

fn submit_form_control_scan_units(control_count: u64) -> u64 {
    control_count.saturating_mul(control_count)
}

fn reserve_semantic_action_hidden_work(
    document: &Document,
    target: &Element,
    action: SemanticActionWork,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let mut derived_string_scratch = scratch.fresh();
    preflight_semantic_action_element_state(
        target,
        false,
        work,
        scratch,
        &mut derived_string_scratch,
    )?;

    match action {
        SemanticActionWork::Fill => reserve_fill_action(target, work, scratch)?,
        SemanticActionWork::Activate => reserve_click_action(
            document,
            target,
            0,
            work,
            scratch,
            &mut derived_string_scratch,
        )?,
        SemanticActionWork::Checkbox => reserve_checkbox_action(target, 3, work, scratch)?,
        SemanticActionWork::Radio => reserve_radio_action(target, 3, work, scratch)?,
        SemanticActionWork::Focus => {
            reserve_focus_action(document, target, work, scratch, &mut derived_string_scratch)?
        },
        SemanticActionWork::Submit => {
            let form = target
                .downcast::<HTMLFormElement>()
                .expect("submit preflight target was validated as a form");
            reserve_form_action(
                document,
                form,
                FormActionWork::Submit,
                work,
                scratch,
                &mut derived_string_scratch,
            )?;
        },
    }
    Ok(())
}

fn preflight_semantic_action_element_state(
    element: &Element,
    include_submission_state: bool,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let raw_attribute_bytes = preflight_semantic_action_element_attributes(element, work, scratch)?;
    // URL/form encoders and CRLF/header escaping can expand one source byte. Six dominates the
    // expansion used by URL percent encoding and multipart header escaping.
    derived.reserve_bytes(raw_attribute_bytes.saturating_mul(6))?;

    if let Some(input) = element.downcast::<HTMLInputElement>() {
        let value_bytes = input
            .automation_text_value_bytes()
            .saturating_add(input.automation_custom_validity_bytes());
        scratch.reserve_bytes(value_bytes)?;
        derived.reserve_bytes(value_bytes.saturating_mul(6))?;
        if include_submission_state {
            let file_entries = input.automation_selected_file_entry_count();
            // The form-wide 2*C base already reserves one possible ordinary entry for this
            // control. Reserve the file-list fan-out before walking/cloning its entries.
            reserve_form_submission_entries(
                form_submission_entry_excess(file_entries),
                work,
                scratch,
                derived,
            )?;
            let (file_string_bytes, file_body_bytes) = input.automation_selected_file_bounds();
            scratch.reserve_bytes(file_string_bytes)?;
            scratch.reserve_bytes(file_body_bytes)?;
            derived.reserve_bytes(file_string_bytes.saturating_mul(6))?;
            derived.reserve_bytes(file_body_bytes)?;
        }
    }
    if let Some(textarea) = element.downcast::<HTMLTextAreaElement>() {
        let value_bytes = textarea
            .automation_text_value_bytes()
            .saturating_add(textarea.automation_custom_validity_bytes());
        scratch.reserve_bytes(value_bytes)?;
        derived.reserve_bytes(value_bytes.saturating_mul(6))?;
    }
    if let Some(select) = element.downcast::<HTMLSelectElement>() {
        let value_bytes = select.automation_custom_validity_bytes();
        scratch.reserve_bytes(value_bytes)?;
        derived.reserve_bytes(value_bytes.saturating_mul(6))?;
    }
    if let Some(button) = element.downcast::<HTMLButtonElement>() {
        let value_bytes = button.automation_custom_validity_bytes();
        scratch.reserve_bytes(value_bytes)?;
        derived.reserve_bytes(value_bytes.saturating_mul(6))?;
    }
    if include_submission_state && let Some(internals) = element.get_element_internals() {
        let entries = internals.automation_submission_entry_count();
        // As for file controls, reserve cardinality fan-out before the hidden FormData values are
        // rooted or cloned. One possible entry is already included in the form-wide 2*C base.
        reserve_form_submission_entries(
            form_submission_entry_excess(entries),
            work,
            scratch,
            derived,
        )?;
        let (string_bytes, file_body_bytes) = internals.automation_submission_bounds();
        scratch.reserve_bytes(string_bytes)?;
        scratch.reserve_bytes(file_body_bytes)?;
        derived.reserve_bytes(string_bytes.saturating_mul(6))?;
        derived.reserve_bytes(file_body_bytes)?;
    }
    Ok(())
}

fn reserve_fill_action(
    target: &Element,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let validity = inspect_control_validity(target, work)?;
    let path = inspect_composed_event_path(target, false, work, scratch, &mut scratch.fresh())?;
    let units = validity
        .native_units()
        .saturating_mul(4)
        .saturating_add(event_dispatch_units(&path, None))
        .saturating_add(SEMANTIC_ACTION_FIXED_WORK);
    work.visit_nodes(units).map_err(map_work_failure)
}

fn reserve_checkbox_action(
    target: &Element,
    event_count: u64,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let validity = inspect_control_validity(target, work)?;
    let mut derived = scratch.fresh();
    let path = inspect_composed_event_path(target, false, work, scratch, &mut derived)?;
    let units = validity
        .native_units()
        .saturating_mul(4)
        .saturating_add(event_dispatch_units(&path, None).saturating_mul(event_count))
        .saturating_add(SEMANTIC_ACTION_FIXED_WORK);
    work.visit_nodes(units).map_err(map_work_failure)
}

fn reserve_radio_action(
    target: &Element,
    event_count: u64,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let input = target
        .downcast::<HTMLInputElement>()
        .expect("radio preflight target was validated as an input");
    let root = input
        .upcast::<Node>()
        .GetRootNode(&GetRootNodeOptions::empty());
    let owner = input.form_owner();
    let group_name = input.radio_group_name();
    let mut root_nodes = 0u64;
    let mut group = Vec::new();
    for node in root.traverse_preorder(ShadowIncluding::No) {
        work.visit_node().map_err(map_work_failure)?;
        root_nodes = root_nodes.saturating_add(1);
        if let Some(candidate) = node.downcast::<HTMLInputElement>()
            && (candidate == input
                || in_same_group(
                    candidate,
                    owner.as_deref(),
                    group_name.as_ref(),
                    Some(&root),
                ))
        {
            group.push(DomRoot::from_ref(candidate));
        }
    }

    let mut validity_units = 0u64;
    for member in &group {
        validity_units = validity_units.saturating_add(
            inspect_control_validity(member.upcast::<Element>(), work)?.native_units(),
        );
    }
    let mut derived = scratch.fresh();
    let path = inspect_composed_event_path(target, false, work, scratch, &mut derived)?;
    let group_count = u64::try_from(group.len()).unwrap_or(u64::MAX);
    // A click can run the radio group once for pre-activation, once for an existing checked
    // member, and again during canceled activation. Six complete cycles conservatively covers
    // those paths without charging unrelated document trees.
    let units = radio_group_native_units(root_nodes, group_count, validity_units)
        .saturating_add(event_dispatch_units(&path, None).saturating_mul(event_count))
        .saturating_add(SEMANTIC_ACTION_FIXED_WORK);
    work.visit_nodes(units).map_err(map_work_failure)
}

fn radio_group_native_units(root_nodes: u64, group_count: u64, validity_units: u64) -> u64 {
    root_nodes
        .saturating_add(root_nodes.saturating_mul(group_count))
        .saturating_add(validity_units)
        .saturating_mul(6)
}

fn reserve_click_action(
    document: &Document,
    target: &Element,
    label_depth: u8,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let path = inspect_composed_event_path(target, true, work, scratch, derived)?;
    work.visit_nodes(event_dispatch_units(&path, None))
        .map_err(map_work_failure)?;
    let Some(activation_target) = path.activation_target else {
        return Ok(());
    };
    preflight_semantic_action_element_state(&activation_target, false, work, scratch, derived)?;

    if let Some(input) = activation_target.downcast::<HTMLInputElement>() {
        return match *input.input_type() {
            InputType::Checkbox(_) => reserve_checkbox_action(&activation_target, 2, work, scratch),
            InputType::Radio(_) => reserve_radio_action(&activation_target, 2, work, scratch),
            InputType::Submit(_) | InputType::Image(_) => {
                if let Some(form) = input.form_owner() {
                    reserve_form_action(
                        document,
                        &form,
                        FormActionWork::Submit,
                        work,
                        scratch,
                        derived,
                    )?;
                }
                Ok(())
            },
            InputType::Reset(_) => {
                if let Some(form) = input.form_owner() {
                    reserve_form_action(
                        document,
                        &form,
                        FormActionWork::Reset,
                        work,
                        scratch,
                        derived,
                    )?;
                }
                Ok(())
            },
            _ => Ok(()),
        };
    }

    if let Some(button) = activation_target.downcast::<HTMLButtonElement>() {
        let button_type = button.Type();
        let form_action = if button.is_submit_button() {
            Some(FormActionWork::Submit)
        } else if button_type.str().eq_ignore_ascii_case("reset") {
            Some(FormActionWork::Reset)
        } else {
            None
        };
        if let Some(action) = form_action
            && let Some(form) = button.form_owner()
        {
            reserve_form_action(document, &form, action, work, scratch, derived)?;
        }
        return Ok(());
    }

    if let Some(label) = activation_target.downcast::<HTMLLabelElement>() {
        // Label control resolution is either its own descendant tree or the label's complete
        // light-tree root for a `for` lookup. Charge that exact relevant tree before calling the
        // read-only resolver, then classify the nested synthetic click.
        if label_depth >= 2 {
            return Ok(());
        }
        let label_element = label.upcast::<Element>();
        let root = if label_element.has_attribute(&local_name!("for")) {
            label
                .upcast::<Node>()
                .GetRootNode(&GetRootNodeOptions::empty())
        } else {
            DomRoot::from_ref(label.upcast::<Node>())
        };
        preflight_semantic_subtree(&root, ShadowIncluding::No, true, work, scratch, derived)?;
        if let Some(control) = label.GetControl() {
            reserve_click_action(
                document,
                control.upcast::<Element>(),
                label_depth.saturating_add(1),
                work,
                scratch,
                derived,
            )?;
        }
        return Ok(());
    }

    if activation_target.local_name() == &local_name!("summary")
        && let Some(parent) = activation_target.upcast::<Node>().GetParentNode()
    {
        let mut children = 0u64;
        for _ in parent.children() {
            work.visit_node().map_err(map_work_failure)?;
            children = children.saturating_add(1);
        }
        work.visit_nodes(children.saturating_mul(2))
            .map_err(map_work_failure)?;
    }
    Ok(())
}

fn reserve_focus_action(
    document: &Document,
    target: &Element,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let new_path = inspect_composed_event_path(target, false, work, scratch, derived)?;
    let old_path = if let Some(old) = document.focus_handler().focused_area().element() {
        inspect_composed_event_path(old, false, work, scratch, derived)?
    } else {
        EventPathFacts::viewport()
    };
    let transition_units = event_dispatch_units(&old_path, Some(&new_path))
        .saturating_mul(2)
        .saturating_add(event_dispatch_units(&new_path, Some(&old_path)).saturating_mul(2));
    work.visit_nodes(transition_units.saturating_add(SEMANTIC_ACTION_FIXED_WORK))
        .map_err(map_work_failure)?;

    if let Some(shadow_root) = target.shadow_root()
        && shadow_root.DelegatesFocus()
    {
        // Autofocus discovery and ordinary delegate discovery inspect only this shadow tree. The
        // ancestor sum covers inherited-contenteditable checks and the deepest possible delegate
        // event path without making unrelated light-DOM size relevant.
        let root = shadow_root.upcast::<Node>();
        let (nodes, attributes, ancestor_steps, ancestor_attributes) =
            preflight_focus_delegate_tree(root, work, scratch, derived)?;
        let units = nodes
            .saturating_add(attributes)
            .saturating_add(ancestor_steps)
            .saturating_add(ancestor_attributes)
            .saturating_mul(8);
        work.visit_nodes(units).map_err(map_work_failure)?;
    }

    if target
        .downcast::<HTMLElement>()
        .is_some_and(HTMLElement::is_editing_host)
    {
        let (nodes, attributes) = preflight_semantic_subtree(
            target.upcast::<Node>(),
            ShadowIncluding::Yes,
            true,
            work,
            scratch,
            derived,
        )?;
        work.visit_nodes(nodes.saturating_add(attributes).saturating_mul(8))
            .map_err(map_work_failure)?;
    }
    Ok(())
}

fn reserve_form_action(
    _document: &Document,
    form: &HTMLFormElement,
    action: FormActionWork,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    if action == FormActionWork::Submit {
        derived.reserve_bytes(FORM_SUBMISSION_DERIVED_FINAL_BYTES)?;
    }
    preflight_semantic_action_element_state(
        form.upcast::<Element>(),
        true,
        work,
        scratch,
        derived,
    )?;
    let control_count = u64::try_from(form.automation_control_count()).unwrap_or(u64::MAX);
    // Bound the rooted snapshot before allocating it.
    work.visit_nodes(control_count).map_err(map_work_failure)?;
    if action == FormActionWork::Submit {
        // `static_validation` refreshes validity for every owned control, and each refresh scans
        // the same owned-control list. This exact C*C term is known without reading control
        // content, so reject a shallow oversized form before the independent entry-output bound.
        work.visit_nodes(submit_form_control_scan_units(control_count))
            .map_err(map_work_failure)?;
    }
    // Every owned control can contribute one ordinary datum plus one optional `dirname` datum.
    // Reserve that complete conservative base before inspecting control content. File lists,
    // multi-selects, and custom-element FormData reserve only their excess over one below, before
    // any hidden entry values are iterated.
    if action == FormActionWork::Submit {
        reserve_form_submission_entries(
            base_form_submission_entries(control_count),
            work,
            scratch,
            derived,
        )?;
    }
    let controls = form.automation_controls();
    let form_path =
        inspect_composed_event_path(form.upcast::<Element>(), false, work, scratch, derived)?;
    let mut native_units = event_dispatch_units(&form_path, None).saturating_mul(match action {
        FormActionWork::Submit => 2, // submit + formdata
        FormActionWork::Reset => 1,
    });

    for control in controls {
        preflight_semantic_action_element_state(&control, true, work, scratch, derived)?;

        // Static constraint validation can dispatch `invalid` at every owned control. Dataset
        // construction also checks each control's ancestor path for datalist containment. The
        // validity refresh itself scans the owning form and nearest fieldset for every control;
        // named radios additionally scan their actual light-tree root while validating the group.
        if action == FormActionWork::Submit {
            let control_path =
                inspect_composed_event_path(&control, false, work, scratch, derived)?;
            native_units = native_units.saturating_add(event_dispatch_units(&control_path, None));
            native_units = native_units.saturating_add(
                inspect_control_validity(&control, work)?.native_units_after_form_scan(),
            );
            if let Some(input) = control.downcast::<HTMLInputElement>()
                && matches!(*input.input_type(), InputType::Radio(_))
            {
                reserve_radio_action(&control, 0, work, scratch)?;
            }
        }
        let mut ancestor_nodes = 0u64;
        for _ in control.upcast::<Node>().ancestors() {
            work.visit_node().map_err(map_work_failure)?;
            ancestor_nodes = ancestor_nodes.saturating_add(1);
        }
        native_units = native_units.saturating_add(ancestor_nodes.saturating_mul(2));

        if let Some(select) = control.downcast::<HTMLSelectElement>() {
            let collected = collect_select_options(select, work)?;
            let option_count = u64::try_from(collected.options.len()).unwrap_or(u64::MAX);
            if action == FormActionWork::Submit {
                // Count selected entries in one explicitly charged pass, then reserve their
                // zero-byte cardinality and repeated control name before reading option values.
                work.visit_nodes(option_count).map_err(map_work_failure)?;
                let selected_option_entries = u64::try_from(
                    collected
                        .options
                        .iter()
                        .filter(|option| option.Selected())
                        .count(),
                )
                .unwrap_or(u64::MAX);
                reserve_form_submission_entries(
                    form_submission_entry_excess(selected_option_entries),
                    work,
                    scratch,
                    derived,
                )?;
                let repeated_name_bytes = form_submission_repeated_name_bytes(
                    select.Name().len() as u64,
                    selected_option_entries,
                );
                scratch.reserve_bytes(repeated_name_bytes)?;
                derived.reserve_bytes(repeated_name_bytes.saturating_mul(6))?;
                native_units = native_units.saturating_add(
                    collected
                        .inspected_nodes
                        .saturating_mul(SELECT_VALIDITY_OPTION_TREE_SCANS),
                );
            }
            for option in collected.options {
                if action == FormActionWork::Reset || option.Selected() {
                    let value = bounded_option_value(&option, work, scratch)?;
                    derived.reserve_bytes((value.len() as u64).saturating_mul(6))?;
                }
                if action == FormActionWork::Reset {
                    let mut option_ancestors = 0u64;
                    for _ in option.upcast::<Node>().ancestors() {
                        work.visit_node().map_err(map_work_failure)?;
                        option_ancestors = option_ancestors.saturating_add(1);
                    }
                    native_units = native_units.saturating_add(option_ancestors);
                }
            }
            native_units = native_units.saturating_add(option_count.saturating_mul(3));
        }

        if action == FormActionWork::Reset {
            if let Some(input) = control.downcast::<HTMLInputElement>() {
                if matches!(*input.input_type(), InputType::Radio(_)) {
                    reserve_radio_action(&control, 0, work, scratch)?;
                } else {
                    native_units = native_units.saturating_add(
                        inspect_control_validity(&control, work)?
                            .native_units()
                            .saturating_mul(2),
                    );
                }
            } else if control.is::<HTMLTextAreaElement>() {
                let (nodes, _) = preflight_semantic_subtree(
                    control.upcast::<Node>(),
                    ShadowIncluding::No,
                    false,
                    work,
                    scratch,
                    derived,
                )?;
                native_units = native_units.saturating_add(nodes).saturating_add(
                    inspect_control_validity(&control, work)?
                        .native_units()
                        .saturating_mul(2),
                );
            } else if let Some(output) = control.downcast::<HTMLOutputElement>() {
                if let Some(bytes) = output.automation_default_value_override_bytes() {
                    scratch.reserve_bytes(bytes)?;
                } else {
                    let (nodes, _) = preflight_semantic_subtree(
                        control.upcast::<Node>(),
                        ShadowIncluding::No,
                        false,
                        work,
                        scratch,
                        derived,
                    )?;
                    native_units = native_units.saturating_add(nodes.saturating_mul(2));
                }
            }
        }
    }

    native_units = native_units
        .saturating_add(control_count.saturating_mul(4))
        .saturating_add(SEMANTIC_ACTION_FIXED_WORK);
    work.visit_nodes(native_units).map_err(map_work_failure)?;

    Ok(())
}

fn inspect_control_validity(
    control: &Element,
    work: &mut WorkBudget,
) -> Result<ControlValidityFacts, DocumentAutomationError> {
    let form_controls = control
        .as_maybe_form_control()
        .and_then(|control| control.form_owner())
        .map(|form| u64::try_from(form.automation_control_count()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let mut ancestor_nodes = 0u64;
    let mut nearest_fieldset = None;
    for ancestor in control.upcast::<Node>().ancestors() {
        work.visit_node().map_err(map_work_failure)?;
        ancestor_nodes = ancestor_nodes.saturating_add(1);
        if nearest_fieldset.is_none()
            && let Some(fieldset) = ancestor.downcast::<HTMLFieldSetElement>()
        {
            nearest_fieldset = Some(DomRoot::from_ref(fieldset));
        }
    }
    let mut fieldset_nodes = 0u64;
    if let Some(fieldset) = nearest_fieldset {
        for _ in fieldset
            .upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
        {
            work.visit_node().map_err(map_work_failure)?;
            fieldset_nodes = fieldset_nodes.saturating_add(1);
        }
    }
    Ok(ControlValidityFacts {
        form_controls,
        ancestor_nodes,
        fieldset_nodes,
    })
}

fn inspect_composed_event_path(
    target: &Element,
    find_activation_target: bool,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<EventPathFacts, DocumentAutomationError> {
    let mut current = Some(DomRoot::from_ref(target.upcast::<Node>()));
    let mut path_nodes = 0u64;
    let mut ancestor_depth_sum = 0u64;
    let mut target_depth = 0u64;
    let mut shadow_retarget_hops = 0u64;
    let mut activation_target = None;
    while let Some(node) = current {
        work.visit_node().map_err(map_work_failure)?;
        path_nodes = path_nodes.saturating_add(1);
        let mut depth = 0u64;
        for _ in node.inclusive_ancestors(ShadowIncluding::Yes) {
            work.visit_node().map_err(map_work_failure)?;
            depth = depth.saturating_add(1);
        }
        if path_nodes == 1 {
            target_depth = depth;
        }
        ancestor_depth_sum = ancestor_depth_sum.saturating_add(depth);

        if node.is::<ShadowRoot>() {
            shadow_retarget_hops = shadow_retarget_hops.saturating_add(1);
        }
        if let Some(element) = node.downcast::<Element>() {
            preflight_event_path_element(element, work, scratch, derived)?;
            if find_activation_target
                && activation_target.is_none()
                && element.as_maybe_activatable().is_some()
            {
                activation_target = Some(DomRoot::from_ref(element));
            }
        }

        current = if let Some(shadow_root) = node.downcast::<ShadowRoot>() {
            Some(DomRoot::upcast(shadow_root.Host()))
        } else if let Some(slot) = node.assigned_slot() {
            Some(DomRoot::upcast(slot))
        } else {
            node.GetParentNode().map(DomRoot::upcast)
        };
    }
    Ok(EventPathFacts {
        path_nodes,
        ancestor_depth_sum,
        target_depth,
        shadow_retarget_hops,
        activation_target,
    })
}

fn preflight_event_path_element(
    element: &Element,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(), DocumentAutomationError> {
    let attributes = element.attrs().borrow();
    work.visit_nodes(u64::try_from(attributes.len()).unwrap_or(u64::MAX))
        .map_err(map_work_failure)?;
    for attribute in attributes.iter() {
        if attribute.namespace() == &ns!()
            && attribute.local_name() == &local_name!("contenteditable")
        {
            let value = attribute.value();
            let mut encoded = scratch.fresh();
            preflight_attribute_value(
                attribute.local_name().as_ref(),
                &value,
                scratch,
                &mut encoded,
            )?;
            let bytes = selector_attribute_value_upper_bound(&value).unwrap_or(u64::MAX);
            derived.reserve_bytes(bytes.saturating_mul(6))?;
        }
    }
    Ok(())
}

fn event_dispatch_units(target: &EventPathFacts, related: Option<&EventPathFacts>) -> u64 {
    let related_hops = related.map_or(0, |facts| facts.shadow_retarget_hops);
    target
        .ancestor_depth_sum
        .saturating_mul(related_hops.saturating_add(1))
        .saturating_add(target.target_depth.saturating_mul(related_hops))
        .saturating_add(target.path_nodes.saturating_mul(4))
        .saturating_add(32)
}

fn preflight_focus_delegate_tree(
    root: &Node,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(u64, u64, u64, u64), DocumentAutomationError> {
    let mut nodes = 0u64;
    let mut attributes = 0u64;
    let mut ancestor_steps = 0u64;
    let mut ancestor_attributes = 0u64;
    let mut local_string_bytes = 0u64;
    for node in root.traverse_preorder(ShadowIncluding::No) {
        work.visit_node().map_err(map_work_failure)?;
        nodes = nodes.saturating_add(1);
        if let Some(element) = node.downcast::<Element>() {
            let count = u64::try_from(element.attrs().borrow().len()).unwrap_or(u64::MAX);
            attributes = attributes.saturating_add(count);
            local_string_bytes = local_string_bytes.saturating_add(
                preflight_semantic_action_element_attributes(element, work, scratch)?,
            );
        }
        if let Some(data) = node.downcast::<CharacterData>() {
            let bytes = u64::try_from(data.data().len()).unwrap_or(u64::MAX);
            scratch.reserve_bytes(bytes)?;
            local_string_bytes = local_string_bytes.saturating_add(bytes);
        }
        for ancestor in node.inclusive_ancestors(ShadowIncluding::No) {
            work.visit_node().map_err(map_work_failure)?;
            ancestor_steps = ancestor_steps.saturating_add(1);
            if let Some(element) = ancestor.downcast::<Element>() {
                let count = u64::try_from(element.attrs().borrow().len()).unwrap_or(u64::MAX);
                work.visit_nodes(count).map_err(map_work_failure)?;
                ancestor_attributes = ancestor_attributes.saturating_add(count);
            }
            if &*ancestor == root {
                break;
            }
        }
    }
    derived.reserve_bytes(local_string_bytes.saturating_mul(6))?;
    Ok((nodes, attributes, ancestor_steps, ancestor_attributes))
}

fn preflight_semantic_subtree(
    root: &Node,
    shadow_including: ShadowIncluding,
    include_attributes: bool,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
    derived: &mut OutputBudget,
) -> Result<(u64, u64), DocumentAutomationError> {
    let mut nodes = 0u64;
    let mut attributes = 0u64;
    let mut raw_bytes = 0u64;
    for node in root.traverse_preorder(shadow_including) {
        work.visit_node().map_err(map_work_failure)?;
        nodes = nodes.saturating_add(1);
        if let Some(data) = node.downcast::<CharacterData>() {
            scratch.reserve_len(data.data().len())?;
        }
        if include_attributes && let Some(element) = node.downcast::<Element>() {
            attributes = attributes
                .saturating_add(u64::try_from(element.attrs().borrow().len()).unwrap_or(u64::MAX));
            raw_bytes = raw_bytes.saturating_add(preflight_semantic_action_element_attributes(
                element, work, scratch,
            )?);
        }
    }
    derived.reserve_bytes(raw_bytes.saturating_mul(6))?;
    Ok((nodes, attributes))
}

fn preflight_semantic_action_element_attributes(
    element: &Element,
    work: &mut WorkBudget,
    scratch: &mut OutputBudget,
) -> Result<u64, DocumentAutomationError> {
    let attributes = element.attrs().borrow();
    let mut raw_value_bytes = 0u64;
    for attribute in attributes.iter() {
        work.visit_node().map_err(map_work_failure)?;
        let value = attribute.value();
        let mut encoded = OutputBudget {
            used: 0,
            limit: u64::MAX,
        };
        preflight_attribute_value(
            attribute.local_name().as_ref(),
            &value,
            scratch,
            &mut encoded,
        )?;
        raw_value_bytes = raw_value_bytes
            .saturating_add(selector_attribute_value_upper_bound(&value).unwrap_or(u64::MAX));
    }
    Ok(raw_value_bytes)
}

struct CollectedSelectOptions {
    options: Vec<DomRoot<HTMLOptionElement>>,
    inspected_nodes: u64,
}

const SELECT_VALIDITY_OPTION_TREE_SCANS: u64 = 2;
const SELECT_VALIDITY_FULL_DOCUMENT_PASSES: u64 = 3;
const SELECT_SHADOW_UPDATE_FIXED_WORK: u64 = 128;

fn reserve_select_validity_scans(
    work: &mut WorkBudget,
    inspected_nodes: u64,
) -> Result<(), WorkFailure> {
    work.visit_nodes(inspected_nodes.saturating_mul(SELECT_VALIDITY_OPTION_TREE_SCANS))
}

/// Precharge page-sized work hidden behind the select validity-state refresh before selectedness
/// changes. `ValidityState::update_pseudo_classes` can scan the complete owning form, walk all
/// ancestors to find a fieldset, and scan that fieldset's complete subtree. Each is bounded by one
/// complete light-DOM document pass. The UA select shadow update has a fixed implementation-owned
/// tree; its creation, mutation-observer walk, and version-ancestor walks consume a separate fixed
/// allowance rather than page-controlled work.
fn reserve_select_hidden_mutation_work(
    document: &Document,
    work: &mut WorkBudget,
) -> Result<(), DocumentAutomationError> {
    let mut document_nodes = 0u64;
    for _ in document
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
    {
        work.visit_node().map_err(map_work_failure)?;
        document_nodes = document_nodes.saturating_add(1);
    }
    reserve_select_hidden_mutation_units(work, document_nodes).map_err(map_work_failure)
}

fn reserve_select_hidden_mutation_units(
    work: &mut WorkBudget,
    document_nodes: u64,
) -> Result<(), WorkFailure> {
    work.visit_nodes(document_nodes.saturating_mul(SELECT_VALIDITY_FULL_DOCUMENT_PASSES))?;
    work.visit_nodes(SELECT_SHADOW_UPDATE_FIXED_WORK)
}

fn reserve_select_version_bump(
    select: &HTMLSelectElement,
    work: &mut WorkBudget,
) -> Result<(), WorkFailure> {
    for _ in select
        .upcast::<Node>()
        .inclusive_ancestors(ShadowIncluding::No)
    {
        work.visit_node()?;
    }
    // `Node::rev_version` also writes the owner document explicitly. Count it even when the
    // connected ancestor traversal already included that document; conservative overcharging is
    // preferable to a disconnected-control escape hatch.
    work.visit_node()
}

/// Collect the HTML option list while charging every node the specification algorithm inspects,
/// including non-option direct children and non-option optgroup children.
fn collect_select_options(
    select: &HTMLSelectElement,
    work: &mut WorkBudget,
) -> Result<CollectedSelectOptions, DocumentAutomationError> {
    let mut options = Vec::new();
    let mut inspected_nodes = 0u64;
    for child in select.upcast::<Node>().children() {
        work.visit_node().map_err(map_work_failure)?;
        inspected_nodes = inspected_nodes.saturating_add(1);
        if let Some(option) = child.downcast::<HTMLOptionElement>() {
            options.push(DomRoot::from_ref(option));
            continue;
        }
        let Some(optgroup) = child.downcast::<HTMLOptGroupElement>() else {
            continue;
        };
        for child in optgroup.upcast::<Node>().children() {
            work.visit_node().map_err(map_work_failure)?;
            inspected_nodes = inspected_nodes.saturating_add(1);
            if let Some(option) = child.downcast::<HTMLOptionElement>() {
                options.push(DomRoot::from_ref(option));
            }
        }
    }
    Ok(CollectedSelectOptions {
        options,
        inspected_nodes,
    })
}

fn bounded_option_value(
    option: &HTMLOptionElement,
    work: &mut WorkBudget,
    output: &mut OutputBudget,
) -> Result<String, DocumentAutomationError> {
    if let Some(value) = bounded_raw_attribute(option.upcast::<Element>(), "value", work, output)? {
        return Ok(value);
    }
    bounded_option_text(option, work, output)
}

fn bounded_option_displayed_label(
    option: &HTMLOptionElement,
    work: &mut WorkBudget,
    output: &mut OutputBudget,
) -> Result<String, DocumentAutomationError> {
    if let Some(label) = bounded_raw_attribute(option.upcast::<Element>(), "label", work, output)?
        && !label.is_empty()
    {
        return Ok(label);
    }
    bounded_option_text(option, work, output)
}

/// Materialize the option text algorithm directly so its DOM traversal and both the intermediate
/// raw text and collapsed result are bounded instead of calling the otherwise-unaccounted IDL
/// getter after a best-effort preflight.
fn bounded_option_text(
    option: &HTMLOptionElement,
    work: &mut WorkBudget,
    output: &mut OutputBudget,
) -> Result<String, DocumentAutomationError> {
    let mut content = String::new();
    let mut allocation = output.fresh();
    let mut traversal = option
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No);
    while traversal.peek().is_some() {
        let skip_children = traversal
            .peek()
            .and_then(|node| node.downcast::<Element>())
            .is_some_and(|element| {
                element.is::<HTMLScriptElement>()
                    || (*element.namespace() == ns!(svg)
                        && element.local_name() == &local_name!("script"))
            });
        let node = if skip_children {
            traversal.next_skipping_children()
        } else {
            traversal.next()
        }
        .expect("a peeked option-text traversal has a current node");
        work.visit_node().map_err(map_work_failure)?;
        if let Some(text) = node.downcast::<Text>() {
            let data = text.upcast::<CharacterData>().data();
            allocation.append(&mut content, &data)?;
        }
    }
    let value = str_join(split_html_space_chars(&content), " ");
    output.check(&value)?;
    Ok(value)
}

fn reserve_option_value_lookup(
    option: &HTMLOptionElement,
    work: &mut WorkBudget,
) -> Result<(), WorkFailure> {
    let element = option.upcast::<Element>();
    let attribute_entries = u64::try_from(element.attrs().borrow().len()).unwrap_or(u64::MAX);
    work.visit_nodes(attribute_entries)?;
    if element.has_attribute(&local_name!("value")) {
        return Ok(());
    }
    for _ in option
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
    {
        work.visit_node()?;
    }
    Ok(())
}

fn bounded_raw_attribute(
    element: &Element,
    name: &str,
    work: &mut WorkBudget,
    temporary: &mut OutputBudget,
) -> Result<Option<String>, DocumentAutomationError> {
    let attribute_entries = u64::try_from(element.attrs().borrow().len()).unwrap_or(u64::MAX);
    work.visit_nodes(attribute_entries)
        .map_err(map_work_failure)?;
    let name = if element.owner_document().is_html_document() {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    };
    let name = LocalName::from(name);
    element
        .with_attribute(&ns!(), &name, |attribute| {
            let value = attribute.value();
            // This operation returns the raw value, not HTML serialization. Keep the lazy-value
            // materialization audit while bounding only bytes which can actually be allocated.
            let mut encoded = OutputBudget {
                used: 0,
                limit: u64::MAX,
            };
            preflight_attribute_value(name.as_ref(), &value, temporary, &mut encoded)?;
            Ok((&**value).to_owned())
        })
        .transpose()
}

fn bounded_text_content(
    element: &Element,
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

        if !element.has_attribute(&LocalName::from("is"))
            && let Some(is_value) = element.get_is()
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
        AttrValue::String(value)
        | AttrValue::LengthPercentage(value, _)
        | AttrValue::Color(value, _)
        | AttrValue::Dimension(value, _)
        | AttrValue::ResolvedUrl(value, _)
        | AttrValue::ShadowParts(value, _) => {
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
        InputType::Text(_)
            | InputType::Search(_)
            | InputType::Url(_)
            | InputType::Tel(_)
            | InputType::Email(_)
            | InputType::Password(_)
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FillFailure {
    Unsupported,
    Immutable,
    DomOperation,
    Automation(DocumentAutomationError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActivationFailure {
    Unsupported,
    Disabled,
    Automation(DocumentAutomationError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckedState {
    changed: bool,
    checked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CheckFailure {
    Unsupported,
    Immutable,
    Automation(DocumentAutomationError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectedState {
    changed: bool,
    values: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SelectFailure {
    Unsupported,
    Immutable,
    InvalidMultiplicity { multiple: bool, requested: u32 },
    ValueNotFound(String),
    ValueDisabled(String),
    Automation(DocumentAutomationError),
    PostMutationAutomation(DocumentAutomationError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FocusFailure {
    Unsupported,
    Automation(DocumentAutomationError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SubmitFailure {
    Unsupported,
    DomOperation,
    Automation(DocumentAutomationError),
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
        self.visit_nodes(1)
    }

    fn visit_nodes(&mut self, count: u64) -> Result<(), WorkFailure> {
        let observed = self.visited.saturating_add(count);
        if observed > u64::from(self.limit) {
            return Err(WorkFailure {
                observed,
                limit: self.limit,
            });
        }
        self.visited = observed;
        Ok(())
    }

    fn evaluate_selector(&mut self, units: u64) -> Result<(), WorkFailure> {
        let observed = self
            .selector_evaluations
            .checked_add(units)
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
    grammar: DocumentSelectorGrammar,
) -> Result<DocumentAutomationResult, DocumentAutomationError> {
    operation
        .validate(limits)
        .map_err(DocumentAutomationError::InvalidRequest)?;

    match operation {
        DocumentAutomationOperation::QueryCount { selector } => {
            let mut work = WorkBudget::new(limits);
            let elements = query_document_bounded(dom, selector, grammar, limits, &mut work)?;
            let count = u32::try_from(elements.len()).expect("match limit is represented by u32");
            Ok(DocumentAutomationResult::QueryCount { count })
        },
        DocumentAutomationOperation::TextContent { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut output = OutputBudget::new(limits);
            let value = dom.text_content(&element, &mut work, &mut output)?;
            Ok(DocumentAutomationResult::TextContent { value })
        },
        DocumentAutomationOperation::InnerHtml { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut output = OutputBudget::new(limits);
            let value = dom.inner_html(&element, &mut work, &mut output)?;
            Ok(DocumentAutomationResult::InnerHtml { value })
        },
        DocumentAutomationOperation::Extract(plan) => {
            let mut work = WorkBudget::new(limits);
            let roots =
                query_document_bounded(dom, plan.root_selector(), grammar, limits, &mut work)?;
            let mut budget = OutputBudget::new(limits);
            let mut rows = Vec::with_capacity(roots.len());

            for (row_index, root) in roots.iter().enumerate() {
                let row = u32::try_from(row_index).expect("match limit is represented by u32");
                let mut values = Vec::with_capacity(plan.fields().len());
                for field in plan.fields() {
                    let element = if grammar == DocumentSelectorGrammar::PracticalV2
                        && field.selector().is_empty()
                    {
                        // An empty v2 field selector deliberately names the current row root.
                        // Charge it independently so a wide plan cannot repeatedly read roots
                        // without consuming the same cumulative traversal budget as a query.
                        work.visit_node().map_err(map_work_failure)?;
                        root.clone()
                    } else {
                        let matches = query_descendants_exact(
                            dom,
                            root,
                            field.selector(),
                            grammar,
                            &mut work,
                        )?;
                        match matches.len() {
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
                        }
                    };

                    budget.check(field.name())?;
                    let value = match field.read() {
                        DocumentExtractionRead::TextContent => {
                            Some(dom.text_content(&element, &mut work, &mut budget)?)
                        },
                        DocumentExtractionRead::InnerHtml => {
                            Some(dom.inner_html(&element, &mut work, &mut budget)?)
                        },
                        DocumentExtractionRead::Attribute => dom.attribute(
                            &element,
                            field.attribute().expect("validated attribute field"),
                            false,
                            &mut work,
                            &mut budget,
                        )?,
                        DocumentExtractionRead::ResolvedUrl => dom.attribute(
                            &element,
                            field.attribute().expect("validated attribute field"),
                            true,
                            &mut work,
                            &mut budget,
                        )?,
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
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut scratch = OutputBudget::new(limits);
            dom.fill(&element, value, &mut work, &mut scratch).map_err(
                |failure| match failure {
                    FillFailure::Unsupported => DocumentAutomationError::UnsupportedFillElement {
                        selector: selector.to_owned(),
                    },
                    FillFailure::Immutable => DocumentAutomationError::ImmutableFillElement {
                        selector: selector.to_owned(),
                    },
                    FillFailure::DomOperation => DocumentAutomationError::DomOperationFailed {
                        operation: DocumentAutomationOperationKind::Fill,
                    },
                    FillFailure::Automation(error) => error,
                },
            )?;
            Ok(DocumentAutomationResult::Filled)
        },
        DocumentAutomationOperation::Activate { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut scratch = OutputBudget::new(limits);
            dom.activate(&element, &mut work, &mut scratch)
                .map_err(|failure| match failure {
                    ActivationFailure::Unsupported => {
                        DocumentAutomationError::UnsupportedActivationElement {
                            selector: selector.to_owned(),
                        }
                    },
                    ActivationFailure::Disabled => {
                        DocumentAutomationError::DisabledActivationElement {
                            selector: selector.to_owned(),
                        }
                    },
                    ActivationFailure::Automation(error) => error,
                })?;
            Ok(DocumentAutomationResult::Activated)
        },
        DocumentAutomationOperation::Check { selector }
        | DocumentAutomationOperation::Uncheck { selector } => {
            let checked = matches!(operation, &DocumentAutomationOperation::Check { .. });
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut scratch = OutputBudget::new(limits);
            let state = dom
                .set_checked(&element, checked, &mut work, &mut scratch)
                .map_err(|failure| match (checked, failure) {
                    (_, CheckFailure::Automation(error)) => error,
                    (true, CheckFailure::Unsupported) => {
                        DocumentAutomationError::UnsupportedCheckElement {
                            selector: selector.to_owned(),
                        }
                    },
                    (true, CheckFailure::Immutable) => {
                        DocumentAutomationError::ImmutableCheckElement {
                            selector: selector.to_owned(),
                        }
                    },
                    (false, CheckFailure::Unsupported) => {
                        DocumentAutomationError::UnsupportedUncheckElement {
                            selector: selector.to_owned(),
                        }
                    },
                    (false, CheckFailure::Immutable) => {
                        DocumentAutomationError::ImmutableUncheckElement {
                            selector: selector.to_owned(),
                        }
                    },
                })?;
            Ok(DocumentAutomationResult::Checked {
                changed: state.changed,
                checked: state.checked,
            })
        },
        DocumentAutomationOperation::Select { selector, values } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut output = OutputBudget::new(limits);
            let state = dom
                .select(&element, values, &mut work, &mut output)
                .map_err(|failure| match failure {
                    SelectFailure::Unsupported => {
                        DocumentAutomationError::UnsupportedSelectElement {
                            selector: selector.to_owned(),
                        }
                    },
                    SelectFailure::Immutable => DocumentAutomationError::ImmutableSelectElement {
                        selector: selector.to_owned(),
                    },
                    SelectFailure::InvalidMultiplicity {
                        multiple,
                        requested,
                    } => DocumentAutomationError::InvalidSelectMultiplicity {
                        selector: selector.to_owned(),
                        multiple,
                        requested,
                    },
                    SelectFailure::ValueNotFound(value) => {
                        DocumentAutomationError::SelectValueNotFound {
                            selector: selector.to_owned(),
                            value,
                        }
                    },
                    SelectFailure::ValueDisabled(value) => {
                        DocumentAutomationError::SelectValueDisabled {
                            selector: selector.to_owned(),
                            value,
                        }
                    },
                    SelectFailure::Automation(error) => error,
                    SelectFailure::PostMutationAutomation(error) => {
                        let _ = error;
                        DocumentAutomationError::DomOperationFailed {
                            operation: DocumentAutomationOperationKind::Select,
                        }
                    },
                })?;
            Ok(DocumentAutomationResult::Selected {
                changed: state.changed,
                values: state.values,
            })
        },
        DocumentAutomationOperation::Focus { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut scratch = OutputBudget::new(limits);
            let focused = dom
                .focus(&element, &mut work, &mut scratch)
                .map_err(|failure| match failure {
                    FocusFailure::Unsupported => DocumentAutomationError::UnsupportedFocusElement {
                        selector: selector.to_owned(),
                    },
                    FocusFailure::Automation(error) => error,
                })?;
            Ok(DocumentAutomationResult::Focused { focused })
        },
        DocumentAutomationOperation::Submit { selector } => {
            let mut work = WorkBudget::new(limits);
            let element = query_document_exact(dom, selector, grammar, &mut work)?;
            let mut scratch = OutputBudget::new(limits);
            dom.submit(&element, &mut work, &mut scratch)
                .map_err(|failure| match failure {
                    SubmitFailure::Unsupported => {
                        DocumentAutomationError::UnsupportedSubmitElement {
                            selector: selector.to_owned(),
                        }
                    },
                    SubmitFailure::DomOperation => DocumentAutomationError::DomOperationFailed {
                        operation: DocumentAutomationOperationKind::Submit,
                    },
                    SubmitFailure::Automation(error) => error,
                })?;
            Ok(DocumentAutomationResult::Submitted)
        },
    }
}

fn query_document_bounded<D: AutomationDom>(
    dom: &mut D,
    selector: &str,
    grammar: DocumentSelectorGrammar,
    limits: DocumentAutomationLimits,
    work: &mut WorkBudget,
) -> Result<Vec<D::Element>, DocumentAutomationError> {
    let elements = dom
        .query_document(
            selector,
            grammar,
            limits.max_matches().saturating_add(1),
            work,
        )
        .map_err(|failure| map_query_failure(selector, failure))?;
    enforce_match_limit(selector, elements.len(), limits)?;
    Ok(elements)
}

fn query_descendants_exact<D: AutomationDom>(
    dom: &mut D,
    root: &D::Element,
    selector: &str,
    grammar: DocumentSelectorGrammar,
    work: &mut WorkBudget,
) -> Result<Vec<D::Element>, DocumentAutomationError> {
    dom.query_descendants(root, selector, grammar, 2, work)
        .map_err(|failure| map_query_failure(selector, failure))
}

fn query_document_exact<D: AutomationDom>(
    dom: &mut D,
    selector: &str,
    grammar: DocumentSelectorGrammar,
    work: &mut WorkBudget,
) -> Result<D::Element, DocumentAutomationError> {
    let elements = dom
        .query_document(selector, grammar, 2, work)
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

    fn fresh(&self) -> Self {
        Self {
            used: 0,
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
        attribute: Option<String>,
        fill: FakeFill,
        activation: FakeActivation,
        checked: bool,
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
        selected_values: Vec<String>,
        focused: bool,
        submitted: bool,
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
                selected_values: Vec::new(),
                focused: false,
                submitted: false,
            }
        }
    }

    impl AutomationDom for FakeDom {
        type Element = FakeElement;

        fn query_document(
            &mut self,
            selector: &str,
            _grammar: DocumentSelectorGrammar,
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
            grammar: DocumentSelectorGrammar,
            stop_after_matches: u32,
            work: &mut WorkBudget,
        ) -> Result<Vec<Self::Element>, QueryFailure> {
            let matches = match selector {
                "" if grammar == DocumentSelectorGrammar::LocalCompoundV1 => {
                    return Err(QueryFailure::InvalidSelector);
                },
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

        fn attribute(
            &mut self,
            element: &Self::Element,
            _name: &str,
            _resolve_url: bool,
            work: &mut WorkBudget,
            output: &mut OutputBudget,
        ) -> Result<Option<String>, DocumentAutomationError> {
            work.visit_node().map_err(map_work_failure)?;
            element
                .attribute
                .as_deref()
                .map(|value| output.copy(value))
                .transpose()
        }

        fn fill(
            &mut self,
            element: &Self::Element,
            value: &str,
            _work: &mut WorkBudget,
            _scratch: &mut OutputBudget,
        ) -> Result<(), FillFailure> {
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

        fn activate(
            &mut self,
            element: &Self::Element,
            _work: &mut WorkBudget,
            _scratch: &mut OutputBudget,
        ) -> Result<(), ActivationFailure> {
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

        fn set_checked(
            &mut self,
            element: &Self::Element,
            checked: bool,
            _work: &mut WorkBudget,
            _scratch: &mut OutputBudget,
        ) -> Result<CheckedState, CheckFailure> {
            self.mutations += usize::from(element.checked != checked);
            Ok(CheckedState {
                changed: element.checked != checked,
                checked,
            })
        }

        fn select(
            &mut self,
            _element: &Self::Element,
            values: &[String],
            _work: &mut WorkBudget,
            output: &mut OutputBudget,
        ) -> Result<SelectedState, SelectFailure> {
            let changed = self.selected_values != values;
            for value in values {
                output.check(value).map_err(SelectFailure::Automation)?;
            }
            self.selected_values = values.to_vec();
            self.mutations += usize::from(changed);
            Ok(SelectedState {
                changed,
                values: values.to_vec(),
            })
        }

        fn focus(
            &mut self,
            _element: &Self::Element,
            _work: &mut WorkBudget,
            _scratch: &mut OutputBudget,
        ) -> Result<bool, FocusFailure> {
            let changed = !self.focused;
            self.focused = true;
            self.mutations += usize::from(changed);
            Ok(true)
        }

        fn submit(
            &mut self,
            _element: &Self::Element,
            _work: &mut WorkBudget,
            _scratch: &mut OutputBudget,
        ) -> Result<(), SubmitFailure> {
            self.submitted = true;
            self.mutations += 1;
            Ok(())
        }
    }

    fn element(text: &str) -> FakeElement {
        FakeElement {
            text: text.to_owned(),
            html: format!("<b>{text}</b>"),
            attribute: Some(format!("/{text}")),
            fill: FakeFill::Supported,
            activation: FakeActivation::Supported,
            checked: false,
        }
    }

    fn execute(
        dom: &mut FakeDom,
        operation: DocumentAutomationOperation,
    ) -> Result<DocumentAutomationResult, DocumentAutomationError> {
        execute_operation(
            dom,
            &operation,
            DocumentAutomationLimits::MVP,
            DocumentSelectorGrammar::PracticalV2,
        )
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
    fn selector_subset_admits_only_precharged_ancestor_traversal() {
        let url_data = UrlExtraData(Arc::new(Url::parse("http://fixture.local/").unwrap()));
        for selector in [
            "#save",
            ".result",
            "article.case[data-id]",
            "article.case[data-id='7']",
            "input[type], textarea",
            ".row .case",
            ".row > .case",
        ] {
            assert!(
                parse_bounded_selector(selector, DocumentSelectorGrammar::PracticalV2, &url_data)
                    .is_ok(),
                "{selector}"
            );
        }

        for selector in [
            ".row + .case",
            ".row ~ .case",
            "[data-id~='7']",
            "[data-id|='7']",
            "[data-id^='7']",
            "[data-id*='7']",
            "[data-id$='7']",
            "[*|data-id='7']",
            ":has(.case)",
            ":nth-child(2)",
            ":not(.disabled)",
        ] {
            assert!(
                matches!(
                    parse_bounded_selector(
                        selector,
                        DocumentSelectorGrammar::PracticalV2,
                        &url_data,
                    ),
                    Err(QueryFailure::UnsupportedSelector | QueryFailure::InvalidSelector),
                ),
                "{selector}"
            );
        }
        assert!(matches!(
            parse_bounded_selector("invalid[", DocumentSelectorGrammar::PracticalV2, &url_data,),
            Err(QueryFailure::InvalidSelector),
        ));
        for selector in [".row .case", ".row > .case"] {
            assert_eq!(
                parse_bounded_selector(
                    selector,
                    DocumentSelectorGrammar::LocalCompoundV1,
                    &url_data,
                )
                .map(|_| ()),
                Err(QueryFailure::UnsupportedSelector),
                "legacy grammar admitted {selector}",
            );
        }
    }

    #[test]
    fn selector_list_complexity_is_precharged_per_candidate() {
        let url_data = UrlExtraData(Arc::new(Url::parse("http://fixture.local/").unwrap()));
        let selector =
            parse_bounded_selector(".a, .b", DocumentSelectorGrammar::PracticalV2, &url_data)
                .unwrap();
        let units = selector.evaluation_units_for_depth(0, 0, 0);
        assert!(units >= 4);

        let limit = u32::try_from(units - 1).unwrap();
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 4, 2, limit, 32).unwrap();
        let mut work = WorkBudget::new(limits);
        assert_eq!(
            work.evaluate_selector(units),
            Err(WorkFailure {
                observed: units,
                limit,
            }),
        );
    }

    #[test]
    fn descendant_selector_cost_is_bounded_by_candidate_depth() {
        let url_data = UrlExtraData(Arc::new(Url::parse("http://fixture.local/").unwrap()));
        let local =
            parse_bounded_selector(".a", DocumentSelectorGrammar::PracticalV2, &url_data).unwrap();
        let descendant =
            parse_bounded_selector(".a .b .c", DocumentSelectorGrammar::PracticalV2, &url_data)
                .unwrap();
        let child =
            parse_bounded_selector(".a > .b", DocumentSelectorGrammar::PracticalV2, &url_data)
                .unwrap();

        assert!(
            descendant.evaluation_units_for_depth(8, 0, 0)
                > local.evaluation_units_for_depth(8, 0, 0)
        );
        assert!(
            descendant.evaluation_units_for_depth(8, 0, 0)
                > child.evaluation_units_for_depth(8, 0, 0)
        );
        assert_eq!(
            descendant.evaluation_units_for_depth(u32::MAX, 0, 0),
            u64::MAX,
        );
    }

    #[test]
    fn admitted_attribute_selectors_precharge_attribute_inventory() {
        let url_data = UrlExtraData(Arc::new(Url::parse("http://fixture.local/").unwrap()));
        let exists =
            parse_bounded_selector("[data-id]", DocumentSelectorGrammar::PracticalV2, &url_data)
                .unwrap();
        let equal = parse_bounded_selector(
            "[data-id='7']",
            DocumentSelectorGrammar::PracticalV2,
            &url_data,
        )
        .unwrap();

        assert!(exists.evaluation_units_for_depth(0, 100_000, 0) > 100_000);
        assert!(equal.evaluation_units_for_depth(0, 100_000, 100_000) > 200_000);
    }

    #[test]
    fn equality_selector_lazy_value_bounds_do_not_materialize() {
        let tokens = AttrValue::TokenList(
            OnceLock::new(),
            vec![style::Atom::from("alpha"), style::Atom::from("beta")],
        );
        let uint = AttrValue::UInt(OnceLock::new(), u32::MAX);
        let integer = AttrValue::Int(OnceLock::new(), i32::MIN);
        let double = AttrValue::Double(OnceLock::new(), f64::MAX);

        assert_eq!(selector_attribute_value_upper_bound(&tokens), Some(10));
        assert_eq!(selector_attribute_value_upper_bound(&uint), Some(10));
        assert_eq!(selector_attribute_value_upper_bound(&integer), Some(11));
        assert_eq!(selector_attribute_value_upper_bound(&double), Some(64));
    }

    #[test]
    fn non_option_select_children_are_reserved_for_every_validity_scan() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 8, 2, 2, 9, 64).unwrap();
        let mut work = WorkBudget::new(limits);
        work.visit_nodes(2).unwrap();

        assert_eq!(
            reserve_select_validity_scans(&mut work, 4),
            Err(WorkFailure {
                observed: 10,
                limit: 9,
            }),
        );
    }

    #[test]
    fn select_hidden_form_fieldset_and_shadow_work_is_reserved_before_mutation() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 8, 2, 2, 52, 64).unwrap();
        let mut work = WorkBudget::new(limits);

        // The document preflight itself consumes 10 units, then three complete passes are
        // reserved for form controls, fieldset discovery, and the fieldset subtree. The fixed UA
        // shadow allowance is intentionally larger than this synthetic budget, proving the
        // reservation fails before a caller can cross the mutation point.
        work.visit_nodes(10).unwrap();
        assert_eq!(
            reserve_select_hidden_mutation_units(&mut work, 10),
            Err(WorkFailure {
                observed: 168,
                limit: 52,
            }),
        );
    }

    #[test]
    fn composed_event_reservation_uses_only_relevant_path_facts() {
        let target = EventPathFacts {
            path_nodes: 5,
            ancestor_depth_sum: 15,
            target_depth: 5,
            shadow_retarget_hops: 0,
            activation_target: None,
        };
        let slotted_related = EventPathFacts {
            path_nodes: 7,
            ancestor_depth_sum: 24,
            target_depth: 6,
            shadow_retarget_hops: 2,
            activation_target: None,
        };

        assert_eq!(event_dispatch_units(&target, None), 67);
        assert_eq!(event_dispatch_units(&target, Some(&slotted_related)), 107);
    }

    #[test]
    fn radio_reservation_scales_with_actual_group_not_unrelated_document_squared() {
        let limits = DocumentAutomationLimits::new_internal(
            32,
            32,
            8,
            2,
            2,
            DocumentAutomationLimits::MVP.max_dom_nodes_visited(),
            64,
        )
        .unwrap();
        let root_nodes = 10_000;

        // One radio in a 10k-node root remains practical because the unrelated root is never
        // squared. A sixteen-member relevant group reserves the real repeated root scans and is
        // rejected before Click/SetChecked.
        assert!(radio_group_native_units(root_nodes, 1, 64) < 200_000);
        let mut work = WorkBudget::new(limits);
        work.visit_nodes(root_nodes).unwrap();
        let hidden = radio_group_native_units(root_nodes, 16, 1_024);
        assert_eq!(
            work.visit_nodes(hidden),
            Err(WorkFailure {
                observed: root_nodes.saturating_add(hidden),
                limit: DocumentAutomationLimits::MVP.max_dom_nodes_visited(),
            }),
        );
    }

    #[test]
    fn shallow_owned_submit_controls_exceed_the_frozen_work_cap() {
        let limits = DocumentAutomationLimits::new_internal(
            32,
            32,
            8,
            2,
            2,
            DocumentAutomationLimits::MVP.max_dom_nodes_visited(),
            64,
        )
        .unwrap();
        let owned_controls = 1_025u64;
        let hidden = submit_form_control_scan_units(owned_controls);
        assert_eq!(hidden, 1_050_625);

        let mut work = WorkBudget::new(limits);
        assert_eq!(
            work.visit_nodes(hidden),
            Err(WorkFailure {
                observed: hidden,
                limit: DocumentAutomationLimits::MVP.max_dom_nodes_visited(),
            }),
        );
    }

    #[test]
    fn changed_select_reserves_both_exact_event_paths() {
        let target = EventPathFacts {
            path_nodes: 5,
            ancestor_depth_sum: 15,
            target_depth: 5,
            shadow_retarget_hops: 0,
            activation_target: None,
        };

        assert_eq!(event_dispatch_units(&target, None), 67);
        assert_eq!(event_dispatch_units(&target, None).saturating_mul(2), 134,);
    }

    #[test]
    fn empty_submission_entries_are_bounded_by_fixed_derived_output() {
        let limits = DocumentAutomationLimits::new_internal(
            32,
            32,
            8,
            2,
            2,
            DocumentAutomationLimits::MVP.max_dom_nodes_visited(),
            128 * 1024,
        )
        .unwrap();
        // 400 ordinary controls conservatively cover one datum plus one optional `dirname`
        // datum each. A 172-entry multiple select adds 171 entries beyond its already-counted
        // base datum, for 971 zero-byte encoded entries in total.
        let entries =
            base_form_submission_entries(400).saturating_add(form_submission_entry_excess(172));
        assert_eq!(entries, 971);
        assert_eq!(
            base_form_submission_entries(1)
                .saturating_add(form_submission_entry_excess(4))
                .saturating_add(form_submission_entry_excess(5)),
            9,
            "file and ElementInternals excess entries are not double-counted",
        );
        let mut work = WorkBudget::new(limits);
        let mut scratch = OutputBudget::new(limits);
        let mut derived = OutputBudget::new(limits);

        reserve_form_submission_entries(
            base_form_submission_entries(400),
            &mut work,
            &mut scratch,
            &mut derived,
        )
        .unwrap();

        assert_eq!(
            reserve_form_submission_entries(
                form_submission_entry_excess(172),
                &mut work,
                &mut scratch,
                &mut derived,
            ),
            Err(DocumentAutomationError::OutputLimitExceeded {
                attempted: 131_085,
                limit: 131_072,
            }),
        );
    }

    #[test]
    fn repeated_fanout_control_names_share_the_cumulative_raw_budget() {
        let limits = DocumentAutomationLimits::new_internal(
            32,
            32,
            8,
            2,
            2,
            DocumentAutomationLimits::MVP.max_dom_nodes_visited(),
            128 * 1024,
        )
        .unwrap();
        let mut scratch = OutputBudget::new(limits);
        scratch.reserve_bytes(70_000).unwrap();
        let repeated = form_submission_repeated_name_bytes(70_000, 2);
        assert_eq!(repeated, 70_000);
        assert_eq!(
            scratch.reserve_bytes(repeated),
            Err(DocumentAutomationError::OutputLimitExceeded {
                attempted: 140_000,
                limit: 131_072,
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
                            value: Some("value".to_owned()),
                        },
                        DocumentExtractionValue {
                            name: "markup".to_owned(),
                            value: Some("<b>value</b>".to_owned()),
                        },
                    ],
                }],
            }),
        );
        assert_eq!(extract.mutations, 0);
    }

    #[test]
    fn extraction_preserves_nullable_attribute_results() {
        let fields = vec![
            embedder_traits::document_automation::DocumentExtractionField::new_attribute_internal(
                "raw".to_owned(),
                "a".to_owned(),
                DocumentExtractionRead::Attribute,
                "href".to_owned(),
            ),
            embedder_traits::document_automation::DocumentExtractionField::new_attribute_internal(
                "resolved".to_owned(),
                "a".to_owned(),
                DocumentExtractionRead::ResolvedUrl,
                "href".to_owned(),
            ),
        ];
        let plan = embedder_traits::document_automation::DocumentExtractionPlan::new_internal(
            ".row".to_owned(),
            fields,
        );
        let mut missing = element("missing");
        missing.attribute = None;
        let mut dom = FakeDom::with_document_matches(vec![element("row")]);
        dom.descendant_matches = vec![missing];

        assert_eq!(
            execute(&mut dom, DocumentAutomationOperation::Extract(plan)),
            Ok(DocumentAutomationResult::Extract {
                rows: vec![DocumentExtractionRow {
                    fields: vec![
                        DocumentExtractionValue {
                            name: "raw".to_owned(),
                            value: None,
                        },
                        DocumentExtractionValue {
                            name: "resolved".to_owned(),
                            value: None,
                        },
                    ],
                }],
            }),
        );
    }

    #[test]
    fn practical_v2_empty_extraction_selector_reads_the_row_root() {
        let fields = vec![
            embedder_traits::document_automation::DocumentExtractionField::new_attribute_internal(
                "raw".to_owned(),
                String::new(),
                DocumentExtractionRead::Attribute,
                "href".to_owned(),
            ),
            embedder_traits::document_automation::DocumentExtractionField::new_attribute_internal(
                "resolved".to_owned(),
                String::new(),
                DocumentExtractionRead::ResolvedUrl,
                "href".to_owned(),
            ),
        ];
        let plan = embedder_traits::document_automation::DocumentExtractionPlan::new_internal(
            "a[href]".to_owned(),
            fields,
        );
        let present = FakeElement {
            attribute: Some("https://example.test/next".to_owned()),
            ..element("next")
        };
        let missing = FakeElement {
            attribute: None,
            ..element("missing")
        };
        let mut dom = FakeDom::with_document_matches(vec![present, missing]);

        assert_eq!(
            execute(&mut dom, DocumentAutomationOperation::Extract(plan.clone()),),
            Ok(DocumentAutomationResult::Extract {
                rows: vec![
                    DocumentExtractionRow {
                        fields: vec![
                            DocumentExtractionValue {
                                name: "raw".to_owned(),
                                value: Some("https://example.test/next".to_owned()),
                            },
                            DocumentExtractionValue {
                                name: "resolved".to_owned(),
                                value: Some("https://example.test/next".to_owned()),
                            },
                        ],
                    },
                    DocumentExtractionRow {
                        fields: vec![
                            DocumentExtractionValue {
                                name: "raw".to_owned(),
                                value: None,
                            },
                            DocumentExtractionValue {
                                name: "resolved".to_owned(),
                                value: None,
                            },
                        ],
                    },
                ],
            }),
        );

        let mut legacy = FakeDom::with_document_matches(vec![element("next")]);
        assert_eq!(
            execute_operation(
                &mut legacy,
                &DocumentAutomationOperation::Extract(plan),
                DocumentAutomationLimits::MVP,
                DocumentSelectorGrammar::LocalCompoundV1,
            ),
            Err(DocumentAutomationError::InvalidSelector {
                selector: String::new(),
            }),
        );
    }

    #[test]
    fn practical_v2_row_root_reads_consume_the_cumulative_work_budget() {
        let limits = DocumentAutomationLimits::new_internal(32, 32, 32, 2, 2, 5, 256).unwrap();
        let fields = vec![
            embedder_traits::document_automation::DocumentExtractionField::new_attribute_internal(
                "raw".to_owned(),
                String::new(),
                DocumentExtractionRead::Attribute,
                "href".to_owned(),
            ),
            embedder_traits::document_automation::DocumentExtractionField::new_attribute_internal(
                "resolved".to_owned(),
                String::new(),
                DocumentExtractionRead::ResolvedUrl,
                "href".to_owned(),
            ),
        ];
        let plan = embedder_traits::document_automation::DocumentExtractionPlan::new_internal(
            "a[href]".to_owned(),
            fields,
        );
        let mut dom = FakeDom::with_document_matches(vec![element("next")]);

        assert_eq!(
            execute_operation(
                &mut dom,
                &DocumentAutomationOperation::Extract(plan),
                limits,
                DocumentSelectorGrammar::PracticalV2,
            ),
            Err(DocumentAutomationError::DomTraversalLimitExceeded {
                observed: 6,
                limit: 5,
            }),
        );
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
    fn semantic_form_operations_report_observed_state() {
        let mut checked = FakeDom::with_document_matches(vec![element("checkbox")]);
        assert_eq!(
            execute(
                &mut checked,
                DocumentAutomationOperation::Check {
                    selector: "#accept".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::Checked {
                changed: true,
                checked: true,
            }),
        );

        let mut already_unchecked = FakeDom::with_document_matches(vec![element("checkbox")]);
        assert_eq!(
            execute(
                &mut already_unchecked,
                DocumentAutomationOperation::Uncheck {
                    selector: "#accept".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::Checked {
                changed: false,
                checked: false,
            }),
        );

        let mut selected = FakeDom::with_document_matches(vec![element("select")]);
        assert_eq!(
            execute(
                &mut selected,
                DocumentAutomationOperation::Select {
                    selector: "#country".to_owned(),
                    values: vec!["mx".to_owned(), "us".to_owned()],
                },
            ),
            Ok(DocumentAutomationResult::Selected {
                changed: true,
                values: vec!["mx".to_owned(), "us".to_owned()],
            }),
        );

        let mut focused = FakeDom::with_document_matches(vec![element("input")]);
        assert_eq!(
            execute(
                &mut focused,
                DocumentAutomationOperation::Focus {
                    selector: "#email".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::Focused { focused: true }),
        );

        let mut submitted = FakeDom::with_document_matches(vec![element("form")]);
        assert_eq!(
            execute(
                &mut submitted,
                DocumentAutomationOperation::Submit {
                    selector: "form".to_owned(),
                },
            ),
            Ok(DocumentAutomationResult::Submitted),
        );
        assert!(submitted.submitted);
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
                DocumentSelectorGrammar::PracticalV2,
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
                DocumentSelectorGrammar::PracticalV2,
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
                DocumentSelectorGrammar::PracticalV2,
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
                DocumentSelectorGrammar::PracticalV2,
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
