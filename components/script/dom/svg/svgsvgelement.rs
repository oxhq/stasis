/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::{Parser, ParserInput};
use data_url::DataUrl;
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::SVGElementData;
use net_traits::image_cache::PendingImageId;
use net_traits::request::InternalRequest;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::parser::ParserContext;
use style::stylesheets::Origin;
use style::values::specified::LengthPercentage;
use style_traits::ParsingMode;
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::event_loop::script_thread::ScriptThread;

// This is the same explicit URL-byte boundary as the controlled-v2 direct HTMLImageElement
// slice. Inline SVG serialization is independently admitted and must not silently widen it.
const CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT: usize = 65_536;

fn is_bounded_data_svg_url(url: &ServoUrl) -> bool {
    let serialized_url = url.as_str();
    if serialized_url.len() > CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT {
        return false;
    }

    let Ok(data_url) = DataUrl::process(serialized_url) else {
        return false;
    };
    let mime_type = data_url.mime_type();
    mime_type.type_ == "image" && mime_type.subtype == "svg+xml"
}

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    #[no_trace]
    controlled_v2_cached_vector_id: Cell<Option<PendingImageId>>,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            controlled_v2_cached_vector_id: Cell::new(None),
            cached_serialized_data_url: Default::default(),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        }

        self.process_use_elements(cx, &cloned_node);

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    /// Return the exact internally serialized URL owned by this inline SVG under the bounded
    /// controlled-v2 slice. This never accepts an arbitrary data URL supplied by layout.
    pub(crate) fn controlled_v2_cached_serialized_data_url(&self) -> Option<ServoUrl> {
        let document = self.owner_document();
        let window = document.window();
        if !document.is_active() ||
            !ScriptThread::current_controlled_top_level_target_matches(window)
        {
            return None;
        }

        let cached = self.cached_serialized_data_url.borrow();
        let Some(Ok(url)) = &*cached else {
            return None;
        };
        is_bounded_data_svg_url(url).then(|| url.clone())
    }

    /// Prove that one layout decode request is the exact internal URL cached for this SVG.
    pub(crate) fn admits_controlled_v2_serialized_data_url(
        &self,
        candidate: &ServoUrl,
        is_internal_request: InternalRequest,
    ) -> bool {
        is_internal_request == InternalRequest::Yes &&
            self.controlled_v2_cached_serialized_data_url()
                .is_some_and(|cached| cached == *candidate)
    }

    /// Bind the Window's exact retained cache identity to this serialized SVG generation.
    pub(crate) fn record_controlled_v2_cached_vector_id(&self, id: PendingImageId) {
        if let Some(previous) = self.controlled_v2_cached_vector_id.replace(Some(id)) &&
            previous != id
        {
            self.owner_window()
                .release_controlled_v2_cached_vector_identity(previous, self.upcast::<Node>());
        }
    }

    /// Release this element's exact retained identity only when the generation still matches.
    pub(crate) fn release_controlled_v2_cached_vector_id(&self, id: PendingImageId) {
        if self.controlled_v2_cached_vector_id.get() != Some(id) {
            return;
        }
        self.controlled_v2_cached_vector_id.set(None);
        self.owner_window()
            .release_controlled_v2_cached_vector_identity(id, self.upcast::<Node>());
    }

    fn process_use_elements(&self, cx: &mut JSContext, root_node: &Node) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use")
            {
                self.process_single_use_element(cx, element, root_node)
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
    ) {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let Some(id_string) = href.str().strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    fn invalidate_cached_serialized_subtree_and_rasterization_result(&self, no_gc: &NoGC) {
        let owner_window = self.owner_window();
        if let Some(id) = self.controlled_v2_cached_vector_id.take() {
            owner_window.release_controlled_v2_cached_vector_identity(id, self.upcast::<Node>());
        }
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser_input = &mut ParserInput::new(value);
                let parser = &mut Parser::new(parser_input);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}

#[cfg(test)]
mod controlled_v2_inline_svg_tests {
    use super::*;

    #[test]
    fn bounded_data_svg_gate_rejects_other_schemes_mime_types_and_oversize_urls() {
        let svg = ServoUrl::parse("data:image/svg+xml;base64,PHN2Zy8+").unwrap();
        assert!(is_bounded_data_svg_url(&svg));

        let png = ServoUrl::parse("data:image/png;base64,AA==").unwrap();
        let http = ServoUrl::parse("https://example.test/image.svg").unwrap();
        assert!(!is_bounded_data_svg_url(&png));
        assert!(!is_bounded_data_svg_url(&http));

        let oversize = ServoUrl::parse(&format!(
            "data:image/svg+xml,{}",
            "a".repeat(CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT)
        ))
        .unwrap();
        assert!(oversize.as_str().len() > CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT);
        assert!(!is_bounded_data_svg_url(&oversize));
    }
}
