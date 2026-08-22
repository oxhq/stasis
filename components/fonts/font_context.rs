/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::default::Default;
use std::hash::{Hash, Hasher};
use std::iter;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use app_units::Au;
use content_security_policy::Violation;
use fonts_traits::{
    CSSFontFaceDescriptors, FontDescriptor, FontFaceRuleWithOrigin, FontIdentifier, FontTemplate,
    FontTemplateRef, FontTemplateRefMethods, StylesheetWebFontLoadFinishedCallback,
    WebFontLoadEvent, WebFontSetDifference,
};
use log::{debug, trace};
use malloc_size_of::MallocSizeOf;
use malloc_size_of_derive::MallocSizeOf;
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::policy_container::PolicyContainer;
use net_traits::request::{
    CredentialsMode, Destination, Referrer, RequestBuilder, RequestClient, RequestMode,
    ServiceWorkersMode,
};
use net_traits::{
    CoreResourceThread, FetchResponseMsg, ResourceFetchTiming, ResourceThreads, fetch_async,
};
use paint_api::CrossProcessPaintApi;
use parking_lot::{Mutex, RwLock};
use rustc_hash::{FxHashMap, FxHashSet};
use servo_arc::Arc as ServoArc;
use servo_base::id::{PainterId, WebViewId};
use servo_config::pref;
use servo_url::ServoUrl;
use style::Atom;
use style::computed_values::font_variant_caps::T as FontVariantCaps;
use style::font_face::{
    FontFaceSourceFormat, FontFaceSourceFormatKeyword, Source, SourceList, UrlSource,
};
use style::properties::generated::font_face::Descriptors as FontFaceRuleDescriptors;
use style::properties::style_structs::Font as FontStyleStruct;
use style::shared_lock::StylesheetGuards;
use style::stylesheets::LockedFontFaceRule;
use style::stylist::Stylist;
use style::values::computed::FontVariantAlternates;
use style::values::computed::font::{FamilyName, FontFamilyNameSyntax, SingleFontFamily};
use style::values::specified::font::VariantAlternates;
use timers::{DocumentClock, DocumentTimeSurface};
use url::Url;
use uuid::Uuid;
use webrender_api::{FontInstanceFlags, FontInstanceKey, FontKey, FontVariation};

use crate::font::{Font, FontFamilyDescriptor, FontGroup, FontRef, FontSearchScope};
use crate::font_feature_values::{
    AlternateKindRequiringResolution, FontFeatureValue, FontFeatureValueMap,
    ResolvedFontVariantAlternates,
};
use crate::font_store::{CrossThreadFontStore, FontStore};
use crate::platform::font::PlatformFont;
use crate::{FontData, LowercaseFontFamilyName, PlatformFontMethods, SystemFontServiceProxy};

static SMALL_CAPS_SCALE_FACTOR: f32 = 0.8; // Matches FireFox (see gfxFont.h)

#[derive(Eq, Hash, MallocSizeOf, PartialEq)]
pub(crate) struct FontParameters {
    pub(crate) font_key: FontKey,
    pub(crate) pt_size: Au,
    pub(crate) variations: Vec<FontVariation>,
    pub(crate) flags: FontInstanceFlags,
}

pub type FontGroupRef = Arc<FontGroup>;

/// The FontContext represents the per-thread/thread state necessary for
/// working with fonts. It is the public API used by the layout and
/// paint code. It talks directly to the system font service where
/// required.
#[derive(MallocSizeOf)]
pub struct FontContext {
    #[conditional_malloc_size_of]
    system_font_service_proxy: Arc<SystemFontServiceProxy>,

    resource_threads: Mutex<CoreResourceThread>,

    /// A sender that can send messages and receive replies from `Paint`.
    paint_api: Mutex<CrossProcessPaintApi>,

    /// The actual instances of fonts ie a [`FontTemplate`] combined with a size and
    /// other font properties, along with the font data and a platform font instance.
    fonts: RwLock<HashMap<FontCacheKey, Option<FontRef>>>,

    /// A caching map between the specification of a font in CSS style and
    /// resolved [`FontGroup`] which contains information about all fonts that
    /// can be selected with that style.
    #[conditional_malloc_size_of]
    resolved_font_groups: RwLock<HashMap<FontGroupCacheKey, FontGroupRef>>,

    web_fonts: CrossThreadFontStore,

    /// A collection of WebRender [`FontKey`]s generated for the web fonts that this
    /// [`FontContext`] controls.
    webrender_font_keys: RwLock<HashMap<FontIdentifier, FontKey>>,

    /// A collection of WebRender [`FontInstanceKey`]s generated for the web fonts that
    /// this [`FontContext`] controls.
    webrender_font_instance_keys: RwLock<HashMap<FontParameters, FontInstanceKey>>,

    /// The data for each web font [`FontIdentifier`]. This data might be used by more than one
    /// [`FontTemplate`] as each identifier refers to a URL.
    font_data: RwLock<HashMap<FontIdentifier, FontData>>,

    have_removed_web_fonts: AtomicBool,

    /// Maps from a URL to all the `@font-face` rules that are currently waiting for the load to
    /// finish.
    currently_downloading_fonts: Mutex<HashMap<ServoUrl, Vec<WebFontDownloadState>>>,

    /// The set of `@font-face` rules that are currently present in the CSS cascade. This is not necessarily
    /// equivalent to the rules that actually apply to the page, because rules that are invalid or not
    /// yet downloaded are also included.
    known_font_face_rules: Mutex<KnownFontFaceRules>,

    /// A lazily-computed map of feature names from `@font-feature-value` rules.
    font_feature_value_map: RwLock<Option<FontFeatureValueMap>>,

    /// The number of fonts that are currently loading.
    number_of_loading_web_fonts: AtomicUsize,
}

/// A callback that will be invoked on the Fetch thread if a web font download
/// results in CSP violations. This handler will be cloned each time a new
/// web font download is initiated.
pub trait CspViolationHandler: Send + std::fmt::Debug + MallocSizeOf {
    fn process_violations(&self, violations: Vec<Violation>);
    fn clone(&self) -> Box<dyn CspViolationHandler>;
}

/// A callback that will be invoked on the Fetch thread when a web font
/// download succeeds, providing timing information about the request.
pub trait NetworkTimingHandler: Send + std::fmt::Debug + MallocSizeOf {
    fn submit_timing(&self, url: ServoUrl, response: ResourceFetchTiming);
    fn clone(&self) -> Box<dyn NetworkTimingHandler>;
}

/// Document-specific data required to fetch a web font.
#[derive(MallocSizeOf)]
pub struct WebFontDocumentContext {
    pub policy_container: PolicyContainer,
    pub request_client: RequestClient,
    pub document_url: ServoUrl,
    pub csp_handler: Box<dyn CspViolationHandler>,
    pub network_timing_handler: Box<dyn NetworkTimingHandler>,
    pub document_clock: DocumentClock,
}

impl WebFontDocumentContext {
    fn require_remote_font_io(&self) -> Result<(), timers::DocumentClockError> {
        self.document_clock
            .require_surface(DocumentTimeSurface::ResourceThreadIo)
    }
}

impl std::fmt::Debug for WebFontDocumentContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebFontDocumentContext")
            .field("policy_container", &self.policy_container)
            .field("request_client", &self.request_client)
            .field("document_url", &self.document_url)
            .field("csp_handler", &self.csp_handler)
            .field("network_timing_handler", &self.network_timing_handler)
            .field("document_clock_id", &self.document_clock.id())
            .finish()
    }
}

impl Clone for WebFontDocumentContext {
    fn clone(&self) -> WebFontDocumentContext {
        Self {
            policy_container: self.policy_container.clone(),
            request_client: self.request_client.clone(),
            document_url: self.document_url.clone(),
            csp_handler: self.csp_handler.clone(),
            network_timing_handler: self.network_timing_handler.clone(),
            document_clock: self.document_clock.clone(),
        }
    }
}

impl FontContext {
    pub fn new(
        system_font_service_proxy: Arc<SystemFontServiceProxy>,
        paint_api: CrossProcessPaintApi,
        resource_threads: ResourceThreads,
    ) -> Self {
        Self {
            system_font_service_proxy,
            resource_threads: Mutex::new(resource_threads.core_thread),
            paint_api: Mutex::new(paint_api),
            fonts: Default::default(),
            resolved_font_groups: Default::default(),
            web_fonts: Default::default(),
            webrender_font_keys: RwLock::default(),
            webrender_font_instance_keys: RwLock::default(),
            have_removed_web_fonts: AtomicBool::new(false),
            font_data: RwLock::default(),
            currently_downloading_fonts: Default::default(),
            known_font_face_rules: Default::default(),
            font_feature_value_map: Default::default(),
            number_of_loading_web_fonts: Default::default(),
        }
    }

    pub fn web_fonts_still_loading(&self) -> usize {
        self.number_of_loading_web_fonts.load(Ordering::SeqCst)
    }

    fn get_font_data(&self, identifier: &FontIdentifier) -> Option<FontData> {
        match identifier {
            FontIdentifier::Web(_) | FontIdentifier::ArrayBuffer(_) => {
                self.font_data.read().get(identifier).cloned()
            },
            FontIdentifier::Local(_) => None,
        }
    }

    /// Returns a `FontGroup` representing fonts which can be used for layout, given the `style`.
    /// Font groups are cached, so subsequent calls with the same `style` will return a reference
    /// to an existing `FontGroup`.
    pub fn font_group(&self, style: ServoArc<FontStyleStruct>) -> FontGroupRef {
        let font_size = style.font_size.computed_size().into();
        self.font_group_with_size(style, font_size)
    }

    /// Like [`Self::font_group`], but overriding the size found in the [`FontStyleStruct`] with the given size
    /// in pixels.
    pub fn font_group_with_size(
        &self,
        style: ServoArc<FontStyleStruct>,
        size: Au,
    ) -> Arc<FontGroup> {
        let cache_key = FontGroupCacheKey { size, style };
        if let Some(font_group) = self.resolved_font_groups.read().get(&cache_key) {
            return font_group.clone();
        }

        let mut descriptor = FontDescriptor::from(&*cache_key.style);
        descriptor.pt_size = size;

        let font_group = Arc::new(FontGroup::new(&cache_key.style, descriptor));
        self.resolved_font_groups
            .write()
            .insert(cache_key, font_group.clone());
        font_group
    }

    /// Returns a font matching the parameters. Fonts are cached, so repeated calls will return a
    /// reference to the same underlying `Font`.
    pub fn font(
        &self,
        font_template: FontTemplateRef,
        font_descriptor: &FontDescriptor,
    ) -> Option<FontRef> {
        self.get_font_maybe_synthesizing_small_caps(
            font_template,
            font_descriptor,
            true, /* synthesize_small_caps */
        )
    }

    fn get_font_maybe_synthesizing_small_caps(
        &self,
        font_template: FontTemplateRef,
        font_descriptor: &FontDescriptor,
        synthesize_small_caps: bool,
    ) -> Option<FontRef> {
        // TODO: (Bug #3463): Currently we only support fake small-caps
        // painting. We should also support true small-caps (where the
        // font supports it) in the future.
        let synthesized_small_caps_font =
            if font_descriptor.variant == FontVariantCaps::SmallCaps && synthesize_small_caps {
                let mut small_caps_descriptor = font_descriptor.clone();
                small_caps_descriptor.pt_size =
                    font_descriptor.pt_size.scale_by(SMALL_CAPS_SCALE_FACTOR);
                self.get_font_maybe_synthesizing_small_caps(
                    font_template.clone(),
                    &small_caps_descriptor,
                    false, /* synthesize_small_caps */
                )
            } else {
                None
            };

        let cache_key = FontCacheKey {
            font_identifier: font_template.identifier().to_owned(),
            font_descriptor: font_descriptor.clone(),
        };

        if let Some(font) = self.fonts.read().get(&cache_key).cloned() {
            return font;
        }

        debug!(
            "FontContext::font cache miss for font_template={:?} font_descriptor={:?}",
            font_template, font_descriptor
        );

        // Check one more time whether the font is cached or not. There's a potential race
        // condition, where between the time we took the read lock above and now, another thread
        // added the font to the cache. This check makes sense, because loading a font has memory
        // implications and is much slower than checking the map again.
        let mut fonts = self.fonts.write();
        if let Some(font) = fonts.get(&cache_key).cloned() {
            return font;
        }

        // TODO: Inserting `None` into the cache here is a bit bogus. Instead we should somehow
        // mark this template as invalid so it isn't tried again.
        let font = self
            .create_font(
                font_template,
                font_descriptor.to_owned(),
                synthesized_small_caps_font,
            )
            .ok();
        fonts.insert(cache_key, font.clone());
        font
    }

    fn matching_web_font_templates(
        &self,
        descriptor_to_match: &FontDescriptor,
        family_descriptor: &FontFamilyDescriptor,
    ) -> Option<Vec<FontTemplateRef>> {
        if family_descriptor.scope != FontSearchScope::Any {
            return None;
        }

        // Do not look for generic fonts in our list of web fonts.
        let SingleFontFamily::FamilyName(ref family_name) = family_descriptor.family else {
            return None;
        };

        self.web_fonts
            .read()
            .families
            .get(&family_name.name.clone().into())
            .map(|templates| templates.find_for_descriptor(Some(descriptor_to_match)))
    }

    /// Try to find matching templates in this [`FontContext`], first looking in the list of web fonts and
    /// falling back to asking the [`super::SystemFontService`] for a matching system font.
    pub fn matching_templates(
        &self,
        descriptor_to_match: &FontDescriptor,
        family_descriptor: &FontFamilyDescriptor,
    ) -> Vec<FontTemplateRef> {
        self.matching_web_font_templates(descriptor_to_match, family_descriptor)
            .unwrap_or_else(|| {
                self.system_font_service_proxy.find_matching_font_templates(
                    Some(descriptor_to_match),
                    &family_descriptor.family,
                )
            })
    }

    /// Create a `Font` for use in layout calculations, from a `FontTemplateData` returned by the
    /// cache thread and a `FontDescriptor` which contains the styling parameters.
    #[servo_tracing::instrument(skip_all)]
    fn create_font(
        &self,
        font_template: FontTemplateRef,
        font_descriptor: FontDescriptor,
        synthesized_small_caps: Option<FontRef>,
    ) -> Result<FontRef, &'static str> {
        Ok(FontRef(Arc::new(Font::new(
            font_template.clone(),
            font_descriptor,
            self.get_font_data(&font_template.identifier()),
            synthesized_small_caps,
        )?)))
    }

    pub(crate) fn create_font_instance_key(
        &self,
        font: &Font,
        painter_id: PainterId,
    ) -> FontInstanceKey {
        let font_template_identifier = font.template.identifier();
        match &*font_template_identifier {
            FontIdentifier::Local(_) => self.system_font_service_proxy.get_system_font_instance(
                font.template.identifier().to_owned(),
                font.descriptor.pt_size,
                font.webrender_font_instance_flags(),
                font.variations().to_owned(),
                painter_id,
            ),
            FontIdentifier::Web(_) | FontIdentifier::ArrayBuffer(_) => self
                .create_web_font_instance(
                    font.template.clone(),
                    font.descriptor.pt_size,
                    font.webrender_font_instance_flags(),
                    font.variations().to_owned(),
                    painter_id,
                ),
        }
    }

    fn create_web_font_instance(
        &self,
        font_template: FontTemplateRef,
        pt_size: Au,
        flags: FontInstanceFlags,
        variations: Vec<FontVariation>,
        painter_id: PainterId,
    ) -> FontInstanceKey {
        let identifier = font_template.identifier();
        let font_data = self
            .get_font_data(&identifier)
            .expect("Web font should have associated font data");
        let font_key = *self
            .webrender_font_keys
            .write()
            .entry(identifier.clone())
            .or_insert_with(|| {
                let font_key = self.system_font_service_proxy.generate_font_key(painter_id);
                self.paint_api.lock().add_font(
                    font_key,
                    font_data.as_ipc_shared_memory(),
                    identifier.index(),
                );
                font_key
            });

        let entry_key = FontParameters {
            font_key,
            pt_size,
            variations: variations.clone(),
            flags,
        };
        *self
            .webrender_font_instance_keys
            .write()
            .entry(entry_key)
            .or_insert_with(|| {
                let font_instance_key = self
                    .system_font_service_proxy
                    .generate_font_instance_key(painter_id);
                self.paint_api.lock().add_font_instance(
                    font_instance_key,
                    font_key,
                    pt_size.to_f32_px(),
                    flags,
                    variations,
                );
                font_instance_key
            })
    }

    fn invalidate_font_groups_after_web_font_load(&self) {
        self.resolved_font_groups.write().clear();
    }

    pub fn is_supported_web_font_source(source: &&Source) -> bool {
        let url_source = match &source {
            Source::Url(url_source) => url_source,
            Source::Local(_) => return true,
        };
        let format_hint = match url_source.format_hint {
            Some(ref format_hint) => format_hint,
            None => return true,
        };

        if matches!(
            format_hint,
            FontFaceSourceFormat::Keyword(
                FontFaceSourceFormatKeyword::Truetype |
                    FontFaceSourceFormatKeyword::Opentype |
                    FontFaceSourceFormatKeyword::Woff |
                    FontFaceSourceFormatKeyword::Woff2
            )
        ) {
            return true;
        }

        if let FontFaceSourceFormat::String(string) = format_hint {
            if string == "truetype" || string == "opentype" || string == "woff" || string == "woff2"
            {
                return true;
            }

            return pref!(layout_variable_fonts_enabled) &&
                (string == "truetype-variations" ||
                    string == "opentype-variations" ||
                    string == "woff-variations" ||
                    string == "woff2-variations");
        }

        false
    }

    fn is_local_or_unknown_url_font(
        &self,
        family_name: &LowercaseFontFamilyName,
        source: &Source,
    ) -> bool {
        match source {
            Source::Url(url) => !url
                .url
                .url()
                .cloned()
                .map(ServoUrl::from)
                .map(FontIdentifier::Web)
                .filter(|font_identifier| self.font_data.read().contains_key(font_identifier))
                .is_some_and(|font_identifier| {
                    self.web_fonts
                        .read()
                        .families
                        .get(family_name)
                        .is_some_and(|templates| {
                            templates
                                .templates
                                .iter()
                                .any(|template| template.borrow().identifier == font_identifier)
                        })
                }),
            Source::Local(_) => true,
        }
    }

    /// Adds the provided new web font request to the list of pending downloads.
    ///
    /// Returns a boolean indicating whether a new download should be started. If there is
    /// already a pending request for the same URL then there is no need to start a new one.
    pub(crate) fn handle_web_font_request_started(
        &self,
        url: ServoUrl,
        state: WebFontDownloadState,
    ) -> bool {
        let mut downloading_fonts = self.currently_downloading_fonts.lock();
        let entry = downloading_fonts.entry(url);

        // If there is no request for that URL yet then we need to start a new one.
        let needs_new_fetch_request = matches!(entry, Entry::Vacant(_));

        entry.or_default().push(state);

        needs_new_fetch_request
    }

    /// Handle a web font load finishing, adding the new font to the [`FontStore`]. If the web font
    /// load was canceled (for instance, if the stylesheet was removed), then do nothing and return
    /// false.
    ///
    /// All download states waiting for this entry to load will have their promise fulfilled.
    pub(crate) fn handle_web_font_request_succeeded(
        &self,
        font_data: FontData,
        url: ServoUrl,
    ) -> bool {
        let Some(download_states) = self.currently_downloading_fonts.lock().remove(&url) else {
            // No one is waiting for this web font to load ):
            return false;
        };
        debug_assert!(
            !download_states.is_empty(),
            "Should have removed this entry"
        );

        let identifier = FontIdentifier::Web(url);
        let Ok(handle) = PlatformFont::new_from_data(identifier.clone(), &font_data, None, false)
        else {
            for download_state in download_states {
                let font_context = download_state.font_context.clone();
                font_context.process_next_web_font_source(download_state);
            }
            return false;
        };

        self.font_data.write().insert(identifier.clone(), font_data);
        let descriptor = handle.descriptor();
        for download_state in download_states {
            let mut descriptor = descriptor.clone();
            descriptor.override_values_with_css_font_template_descriptors(
                &download_state.css_font_face_descriptors,
            );

            let new_template = FontTemplate::new(
                identifier.clone(),
                descriptor,
                download_state.initiator.font_face_rule().cloned(),
            );

            download_state.handle_web_font_load_success(new_template);
        }

        true
    }

    /// Decrement the count of font loads blocking the `document.fonts.ready` promise by one.
    pub fn decrement_count_of_loading_fonts_by_one(&self) {
        self.number_of_loading_web_fonts
            .fetch_sub(1, Ordering::SeqCst);
    }

    /// Returns true iff a `@font-face` rule is part of the active set.
    ///
    /// A font face rule might be removed from this set if its stylesheet is removed for example.
    pub(crate) fn is_font_face_rule_active(
        &self,
        target_rule: &ServoArc<LockedFontFaceRule>,
    ) -> bool {
        self.known_font_face_rules.lock().contains_rule(target_rule)
    }
}

fn add_stylesheet_web_font_template_if_active(
    known_font_face_rules: &Mutex<KnownFontFaceRules>,
    web_fonts: &CrossThreadFontStore,
    target_rule: &ServoArc<LockedFontFaceRule>,
    family_name: LowercaseFontFamilyName,
    new_template: FontTemplate,
) -> bool {
    let known_font_face_rules = known_font_face_rules.lock();
    if !known_font_face_rules.contains_rule(target_rule) {
        return false;
    }

    web_fonts
        .write()
        .add_new_template(family_name, new_template);
    true
}

/// Tracks the progress of loading a single `@font-face` rule by trying all specified
/// sources in order.
#[derive(MallocSizeOf)]
pub(crate) struct WebFontDownloadState {
    webview_id: Option<WebViewId>,
    css_font_face_descriptors: CSSFontFaceDescriptors,
    remaining_sources: Vec<Source>,
    local_fonts: FxHashMap<Atom, Option<FontTemplateRef>>,
    #[conditional_malloc_size_of]
    pub(crate) font_context: Arc<FontContext>,
    initiator: WebFontLoadInitiator,
    document_context: WebFontDocumentContext,
}

impl WebFontDownloadState {
    fn new(
        webview_id: Option<WebViewId>,
        font_context: Arc<FontContext>,
        css_font_face_descriptors: CSSFontFaceDescriptors,
        initiator: WebFontLoadInitiator,
        sources: Vec<Source>,
        local_fonts: FxHashMap<Atom, Option<FontTemplateRef>>,
        document_context: WebFontDocumentContext,
    ) -> WebFontDownloadState {
        WebFontDownloadState {
            webview_id,
            css_font_face_descriptors,
            remaining_sources: sources,
            local_fonts,
            font_context,
            initiator,
            document_context,
        }
    }

    pub(crate) fn handle_web_font_load_success(self, new_template: FontTemplate) {
        let family_name = self.css_font_face_descriptors.family_name.clone();
        match self.initiator {
            WebFontLoadInitiator::Stylesheet(initiator) => {
                if !add_stylesheet_web_font_template_if_active(
                    &self.font_context.known_font_face_rules,
                    &self.font_context.web_fonts,
                    &initiator.created_by,
                    family_name,
                    new_template,
                ) {
                    // This font load was cancelled.
                    if self
                        .font_context
                        .number_of_loading_web_fonts
                        .fetch_sub(1, Ordering::SeqCst) ==
                        1
                    {
                        // This was the last loading font - we must inform the script thread that the load
                        // has finished because this an opportunity to resolve document.fonts.ready.
                        (initiator.callback)(WebFontLoadEvent::UnblockedFontReadyPromise);
                    }
                    return;
                }

                self.font_context
                    .invalidate_font_groups_after_web_font_load();

                // Note: We intentionally do not call decrement_count_of_loading_fonts_by_one here.
                // That is handled in the callback, which avoids document.fonts.ready being resolved
                // prematurely.
                (initiator.callback)(WebFontLoadEvent::LoadedSuccessfully);
            },
            WebFontLoadInitiator::Script(callback) => {
                self.font_context.decrement_count_of_loading_fonts_by_one();
                callback(family_name, Some(new_template));
            },
        }
    }

    /// Called when we've tried all available sources and none were usable.
    pub(crate) fn handle_web_font_load_failure(self) {
        let family_name = self.css_font_face_descriptors.family_name.clone();
        match self.initiator {
            WebFontLoadInitiator::Stylesheet(initiator) => {
                if self
                    .font_context
                    .number_of_loading_web_fonts
                    .fetch_sub(1, Ordering::SeqCst) ==
                    1
                {
                    // This was the last loading font - we must inform the script thread that the load
                    // has finished because this an opportunity to resolve document.fonts.ready.
                    (initiator.callback)(WebFontLoadEvent::UnblockedFontReadyPromise);
                }
            },
            WebFontLoadInitiator::Script(callback) => {
                self.font_context.decrement_count_of_loading_fonts_by_one();
                callback(family_name, None);
            },
        }
    }
}

pub trait FontContextWebFontMethods {
    fn rebuild_font_face_set(
        &self,
        webview_id: WebViewId,
        stylist: &Stylist,
        guards: &StylesheetGuards<'_>,
        callback: StylesheetWebFontLoadFinishedCallback,
        document_context: &WebFontDocumentContext,
    ) -> WebFontSetDifference;
    fn load_single_font_face_rule(
        &self,
        webview_id: WebViewId,
        locked_font_face_rule: &FontFaceRuleWithOrigin,
        guards: &StylesheetGuards<'_>,
        callback: StylesheetWebFontLoadFinishedCallback,
        document_context: &WebFontDocumentContext,
    );
    fn load_web_font_for_script(
        &self,
        webview_id: Option<WebViewId>,
        sources: SourceList,
        descriptors: CSSFontFaceDescriptors,
        finished_callback: ScriptWebFontLoadFinishedCallback,
        document_context: &WebFontDocumentContext,
    );
    fn handle_web_font_request_failed(&self, url: ServoUrl);
}

impl FontContextWebFontMethods for Arc<FontContext> {
    fn load_single_font_face_rule(
        &self,
        webview_id: WebViewId,
        locked_font_face_rule: &FontFaceRuleWithOrigin,
        guards: &StylesheetGuards<'_>,
        callback: StylesheetWebFontLoadFinishedCallback,
        document_context: &WebFontDocumentContext,
    ) {
        let font_face_rule = locked_font_face_rule.read_with(guards);
        let Some(ref sources) = font_face_rule.descriptors.src else {
            return;
        };

        let css_font_face_descriptors = font_face_rule.into();

        let initiator = FontFaceRuleInitiator {
            created_by: locked_font_face_rule.rule.clone(),
            font_face_rule: font_face_rule.descriptors.clone(),
            callback: callback.clone(),
        };

        self.start_loading_one_web_font(
            Some(webview_id),
            sources,
            css_font_face_descriptors,
            WebFontLoadInitiator::Stylesheet(Box::new(initiator)),
            document_context,
        );
    }
    fn rebuild_font_face_set(
        &self,
        webview_id: WebViewId,
        stylist: &Stylist,
        guards: &StylesheetGuards<'_>,
        callback: StylesheetWebFontLoadFinishedCallback,
        document_context: &WebFontDocumentContext,
    ) -> WebFontSetDifference {
        let difference = self
            .known_font_face_rules
            .lock()
            .diff_old_and_new_font_face_rules(stylist, guards);

        for added_rule in &difference.added_font_faces {
            self.load_single_font_face_rule(
                webview_id,
                added_rule,
                guards,
                callback.clone(),
                document_context,
            );
        }
        for removed_rule in &difference.removed_font_faces {
            let removed_rule = removed_rule.read_with(guards);
            self.remove_single_font_face_rule(
                &removed_rule.descriptors,
                &mut self.web_fonts.write(),
            );
        }

        if !difference.removed_font_faces.is_empty() {
            // We modified the list of available fonts, so invalidate resolved font groups.
            self.resolved_font_groups.write().clear();

            // Ensure that we clean up any WebRender resources on the next display list update.
            self.have_removed_web_fonts.store(true, Ordering::Relaxed);
        }

        difference
    }

    fn load_web_font_for_script(
        &self,
        webview_id: Option<WebViewId>,
        sources: SourceList,
        descriptors: CSSFontFaceDescriptors,
        finished_callback: ScriptWebFontLoadFinishedCallback,
        document_context: &WebFontDocumentContext,
    ) {
        let completion_handler = WebFontLoadInitiator::Script(finished_callback);
        self.start_loading_one_web_font(
            webview_id,
            &sources,
            descriptors,
            completion_handler,
            document_context,
        );
    }

    /// Called when a single URL for a `@font-face` failed to load.
    fn handle_web_font_request_failed(&self, url: ServoUrl) {
        let Some(subscribers) = self.currently_downloading_fonts.lock().remove(&url) else {
            return;
        };

        for subscriber in subscribers {
            // See if the font load was cancelled in the meantime
            if let WebFontLoadInitiator::Stylesheet(stylesheet_initiator) = &subscriber.initiator &&
                !self.is_font_face_rule_active(&stylesheet_initiator.created_by)
            {
                // This font load was cancelled.
                if self
                    .number_of_loading_web_fonts
                    .fetch_sub(1, Ordering::SeqCst) ==
                    1
                {
                    // This was the last loading font - we must inform the script thread that the load
                    // has finished because this an opportunity to resolve document.fonts.ready.
                    (stylesheet_initiator.callback)(WebFontLoadEvent::UnblockedFontReadyPromise);
                }
                continue;
            }

            self.process_next_web_font_source(subscriber);
        }
    }
}

impl FontContext {
    pub fn collect_unused_webrender_resources(
        &self,
        all: bool,
    ) -> (Vec<FontKey>, Vec<FontInstanceKey>) {
        if all {
            let mut webrender_font_keys = self.webrender_font_keys.write();
            let mut webrender_font_instance_keys = self.webrender_font_instance_keys.write();
            self.have_removed_web_fonts.store(false, Ordering::Relaxed);
            return (
                webrender_font_keys.drain().map(|(_, key)| key).collect(),
                webrender_font_instance_keys
                    .drain()
                    .map(|(_, key)| key)
                    .collect(),
            );
        }

        if !self.have_removed_web_fonts.load(Ordering::Relaxed) {
            return (Vec::new(), Vec::new());
        }

        // Lock everything to prevent adding new fonts while we are cleaning up the old ones.
        let web_fonts = self.web_fonts.write();
        let mut font_data = self.font_data.write();
        let _fonts = self.fonts.write();
        let _font_groups = self.resolved_font_groups.write();
        let mut webrender_font_keys = self.webrender_font_keys.write();
        let mut webrender_font_instance_keys = self.webrender_font_instance_keys.write();

        let mut unused_identifiers: HashSet<FontIdentifier> =
            webrender_font_keys.keys().cloned().collect();
        for templates in web_fonts.families.values() {
            templates.for_all_identifiers(|identifier| {
                unused_identifiers.remove(identifier);
            });
        }

        font_data.retain(|font_identifier, _| !unused_identifiers.contains(font_identifier));

        self.have_removed_web_fonts.store(false, Ordering::Relaxed);

        let mut removed_keys: FxHashSet<FontKey> = FxHashSet::default();
        webrender_font_keys.retain(|identifier, font_key| {
            if unused_identifiers.contains(identifier) {
                removed_keys.insert(*font_key);
                false
            } else {
                true
            }
        });

        let mut removed_instance_keys: HashSet<FontInstanceKey> = HashSet::new();
        webrender_font_instance_keys.retain(|font_param, instance_key| {
            if removed_keys.contains(&font_param.font_key) {
                removed_instance_keys.insert(*instance_key);
                false
            } else {
                true
            }
        });

        (
            removed_keys.into_iter().collect(),
            removed_instance_keys.into_iter().collect(),
        )
    }

    /// Returns `true` if any font templates were removed.
    fn remove_single_font_face_rule(
        &self,
        font_face_rule: &FontFaceRuleDescriptors,
        font_store: &mut FontStore,
    ) -> bool {
        let Some(family) = font_face_rule.font_family.as_ref() else {
            return false;
        };

        let lowercase_family_name: LowercaseFontFamilyName = family.name.clone().into();
        let Some(known_family) = font_store.families.get_mut(&lowercase_family_name) else {
            return false;
        };
        if !known_family.remove_template_for_font_face_rule(font_face_rule) {
            return false;
        }
        self.fonts.write().retain(|_, font| match font {
            Some(font) => !font
                .template
                .borrow()
                .is_defined_by_font_face_rule(font_face_rule),
            _ => true,
        });

        true
    }

    pub fn add_template_to_font_context(
        &self,
        family_name: LowercaseFontFamilyName,
        new_template: FontTemplate,
    ) {
        self.web_fonts
            .write()
            .add_new_template(family_name, new_template);
        self.invalidate_font_groups_after_web_font_load();
    }

    pub fn construct_web_font_from_data(
        &self,
        data: &[u8],
        descriptors: CSSFontFaceDescriptors,
    ) -> Option<(LowercaseFontFamilyName, FontTemplate)> {
        let bytes = fontsan::process(data)
            .inspect_err(|error| {
                debug!(
                    "Sanitiser rejected FontFace font: family={} with {error:?}",
                    descriptors.family_name,
                );
            })
            .ok()?;
        let font_data = FontData::from_bytes(&bytes);

        let identifier = FontIdentifier::ArrayBuffer(Uuid::new_v4());
        let handle =
            PlatformFont::new_from_data(identifier.clone(), &font_data, None, false).ok()?;

        let new_template = FontTemplate::new(identifier.clone(), handle.descriptor(), None);

        self.font_data.write().insert(identifier, font_data);

        Some((descriptors.family_name, new_template))
    }

    fn start_loading_one_web_font(
        self: &Arc<FontContext>,
        webview_id: Option<WebViewId>,
        source_list: &SourceList,
        css_font_face_descriptors: CSSFontFaceDescriptors,
        completion_handler: WebFontLoadInitiator,
        document_context: &WebFontDocumentContext,
    ) {
        self.number_of_loading_web_fonts
            .fetch_add(1, Ordering::SeqCst);

        let sources: Vec<Source> = source_list
            .0
            .iter()
            .rev()
            .filter(Self::is_supported_web_font_source)
            .filter(|source| {
                self.is_local_or_unknown_url_font(&css_font_face_descriptors.family_name, source)
            })
            .cloned()
            .collect();

        // Fetch all local fonts first, beacause if we try to fetch them later on during the process of
        // loading the list of web font `src`s we may be running in the context of the router thread, which
        // means we won't be able to seend IPC messages to the FontCacheThread.
        //
        // TODO: This is completely wrong. The specification says that `local()` font-family should match
        // against full PostScript names, but this is matching against font family names. This works...
        // sometimes.
        let sources_transformed = sources
            .iter()
            .filter_map(|source| {
                if let Source::Local(family_name) = source {
                    Some(family_name)
                } else {
                    None
                }
            })
            .map(|family_name| {
                let family = SingleFontFamily::FamilyName(FamilyName {
                    name: family_name.name.clone(),
                    syntax: FontFamilyNameSyntax::Quoted,
                });
                let matching_font_templates = self
                    .system_font_service_proxy
                    .find_matching_font_templates(None, &family);
                let value = matching_font_templates.first();
                (family_name.name.clone(), value.cloned())
            });

        let local_fonts = FxHashMap::from_iter(sources_transformed);

        self.process_next_web_font_source(WebFontDownloadState::new(
            webview_id,
            self.clone(),
            css_font_face_descriptors,
            completion_handler,
            sources,
            local_fonts,
            document_context.clone(),
        ));
    }

    pub(crate) fn process_next_web_font_source(
        self: &Arc<FontContext>,
        mut state: WebFontDownloadState,
    ) {
        let Some(source) = state.remaining_sources.pop() else {
            state.handle_web_font_load_failure();
            return;
        };

        let this = self.clone();
        let web_font_family_name = state.css_font_face_descriptors.family_name.clone();
        match source {
            Source::Url(url_source) => {
                RemoteWebFontDownloader::download(url_source, this, web_font_family_name, state)
            },
            Source::Local(ref local_family_name) => {
                if let Some(new_template) = state
                    .local_fonts
                    .get(&local_family_name.name)
                    .cloned()
                    .flatten()
                    .and_then(|local_template| {
                        let template = FontTemplate::new_for_local_web_font(
                            local_template,
                            &state.css_font_face_descriptors,
                            state.initiator.font_face_rule().cloned(),
                        )
                        .ok()?;
                        Some(template)
                    })
                {
                    state.handle_web_font_load_success(new_template);
                } else {
                    this.process_next_web_font_source(state);
                }
            },
        }
    }

    /// Resolves the value of `font-variant-alternates` to a set of OpenType features to apply.
    pub fn resolve_font_variant_alternate_identifiers_for(
        &self,
        font: &FontRef,
        alternates: &FontVariantAlternates,
        stylist: &Stylist,
    ) -> ResolvedFontVariantAlternates {
        let mut resolved_alternates = ResolvedFontVariantAlternates::default();
        if alternates.is_empty() {
            return resolved_alternates;
        }
        let Some(family_name) = font.family_name() else {
            return resolved_alternates;
        };

        for alternate in alternates.iter() {
            match alternate {
                VariantAlternates::Stylistic(stylistic) => {
                    let Some(FontFeatureValue::Single(value)) = self
                        .look_up_font_feature_alternate_name(
                            family_name.clone(),
                            AlternateKindRequiringResolution::Stylistic,
                            stylistic.0.clone(),
                            stylist,
                        )
                    else {
                        continue;
                    };

                    resolved_alternates.stylistic = Some(value);
                },
                VariantAlternates::Styleset(styleset_list) => {
                    for styleset in styleset_list.iter() {
                        let Some(FontFeatureValue::Vector(value)) = self
                            .look_up_font_feature_alternate_name(
                                family_name.clone(),
                                AlternateKindRequiringResolution::Styleset,
                                styleset.0.clone(),
                                stylist,
                            )
                        else {
                            continue;
                        };

                        resolved_alternates.styleset.extend(value.0.iter());
                    }
                },
                VariantAlternates::CharacterVariant(character_variant_list) => {
                    for character_variant in character_variant_list.iter() {
                        let Some(FontFeatureValue::Pair(value)) = self
                            .look_up_font_feature_alternate_name(
                                family_name.clone(),
                                AlternateKindRequiringResolution::CharacterVariant,
                                character_variant.0.clone(),
                                stylist,
                            )
                        else {
                            continue;
                        };

                        resolved_alternates.character_variant.push(value);
                    }
                },
                VariantAlternates::Swash(swash) => {
                    let Some(FontFeatureValue::Single(value)) = self
                        .look_up_font_feature_alternate_name(
                            family_name.clone(),
                            AlternateKindRequiringResolution::Swash,
                            swash.0.clone(),
                            stylist,
                        )
                    else {
                        continue;
                    };

                    resolved_alternates.swash = Some(value);
                },
                VariantAlternates::Ornaments(ornaments) => {
                    let Some(FontFeatureValue::Single(value)) = self
                        .look_up_font_feature_alternate_name(
                            family_name.clone(),
                            AlternateKindRequiringResolution::Ornaments,
                            ornaments.0.clone(),
                            stylist,
                        )
                    else {
                        continue;
                    };

                    resolved_alternates.ornaments = Some(value);
                },
                VariantAlternates::Annotation(annotation) => {
                    let Some(FontFeatureValue::Single(value)) = self
                        .look_up_font_feature_alternate_name(
                            family_name.clone(),
                            AlternateKindRequiringResolution::Annotation,
                            annotation.0.clone(),
                            stylist,
                        )
                    else {
                        continue;
                    };

                    resolved_alternates.annotation = Some(value);
                },
                VariantAlternates::HistoricalForms => {
                    resolved_alternates.historical_forms = true;
                },
            }
        }

        resolved_alternates
    }

    /// Resolves a single component of `font-variant-alternates`, like `stylistic(foobar)` to a font-specific
    /// set of OpenType features to apply.
    ///
    /// If the map of `@font-feature-values` rules has not yet been computed then this method
    /// will compute it.
    fn look_up_font_feature_alternate_name(
        &self,
        family_name: Atom,
        kind: AlternateKindRequiringResolution,
        name: Atom,
        stylist: &Stylist,
    ) -> Option<FontFeatureValue> {
        // First, check if the map was initialized previously.
        let read_guard = self.font_feature_value_map.read();
        if let Some(map) = &*read_guard {
            // This is the cheap case, we just need to read from the map
            map.lookup(family_name, kind, name)
        } else {
            // Map was not initialized yet - need to acquire a mutable guard and initialize it.
            drop(read_guard);
            let mut write_guard = self.font_feature_value_map.write();
            if let Some(map) = &*write_guard {
                // We lost a race, some other thread initialized the map while we were waiting
                // on the lock.
                return map.lookup(family_name, kind, name);
            }

            log::debug!("Initializing @font-feature-values map");
            let mut map = FontFeatureValueMap::default();
            stylist
                .iter_extra_data_origins_rev()
                .flat_map(|(extra_data, _)| extra_data.font_feature_values.iter())
                .for_each(|(rule, _)| map.add_rule(rule));
            let map = &*write_guard.insert(map);
            // Finally, perform the actual lookup
            map.lookup(family_name, kind, name)
        }
    }

    pub fn invalidate_font_feature_values_map(&self) {
        self.font_feature_value_map.write().take();
    }
}

pub(crate) type ScriptWebFontLoadFinishedCallback =
    Box<dyn FnOnce(LowercaseFontFamilyName, Option<FontTemplate>) + Send>;

#[derive(MallocSizeOf)]
pub(crate) struct FontFaceRuleInitiator {
    /// A reference to the `@font-face` rule that created this web font load.
    /// This is only used to identify the font in case it is
    // TODO: It is awkward that we have to carry both the locked font face rule and the
    // unlocked copy around. Perhaps the FontContext should have access to the shared
    // lock in the future.
    #[conditional_malloc_size_of]
    created_by: ServoArc<LockedFontFaceRule>,
    font_face_rule: FontFaceRuleDescriptors,
    #[ignore_malloc_size_of = "dyn Fn"]
    callback: StylesheetWebFontLoadFinishedCallback,
}

#[derive(MallocSizeOf)]
pub(crate) enum WebFontLoadInitiator {
    Stylesheet(Box<FontFaceRuleInitiator>),
    Script(#[ignore_malloc_size_of = "dyn Fn"] ScriptWebFontLoadFinishedCallback),
}

impl WebFontLoadInitiator {
    pub(crate) fn font_face_rule(&self) -> Option<&FontFaceRuleDescriptors> {
        match self {
            Self::Stylesheet(initiator) => Some(&initiator.font_face_rule),
            Self::Script(_) => None,
        }
    }
}

struct RemoteWebFontDownloader {
    /// The URL of the font currently being loaded.
    url: ServoArc<Url>,
    web_font_family_name: LowercaseFontFamilyName,
    response_valid: bool,
    /// The data that has been received from the network thread so far.
    response_data: Vec<u8>,
    document_context: WebFontDocumentContext,
    font_context: Arc<FontContext>,
}

enum DownloaderResponseResult {
    InProcess,
    Finished,
    Failure,
}

impl RemoteWebFontDownloader {
    fn download(
        url_source: UrlSource,
        font_context: Arc<FontContext>,
        web_font_family_name: LowercaseFontFamilyName,
        state: WebFontDownloadState,
    ) {
        // https://drafts.csswg.org/css-fonts/#font-fetching-requirements
        let document_context = state.document_context.clone();
        let Some(prepared) =
            prepare_remote_font_download(&url_source, state, &document_context, |state| {
                font_context.process_next_web_font_source(state)
            })
        else {
            return;
        };
        let (url, state) = match prepared {
            Ok(prepared) => prepared,
            Err((error, state)) => {
                // Remote web fonts use the raw net-traits fetch callback and are not yet joined
                // to the document producer fence. Refuse the request before fetch dispatch and
                // finish the logical font load as failed; the controlled clock keeps the exact
                // unsupported surface sticky for settlement.
                log::error!("refusing uncontrolled remote web-font fetch: {error}");
                state.handle_web_font_load_failure();
                return;
            },
        };

        let webview_id = state.webview_id;
        if !font_context.handle_web_font_request_started(url.clone().into(), state) {
            // This URL is already being fetched for another font, and we will be
            // notified when that request completes.
            return;
        }

        let request = RequestBuilder::new(
            webview_id,
            UrlWithBlobClaim::from_url_without_having_claimed_blob(url.clone().into()),
            Referrer::ReferrerUrl(document_context.document_url.clone()),
        )
        .destination(Destination::Font)
        .mode(RequestMode::CorsMode)
        .credentials_mode(CredentialsMode::CredentialsSameOrigin)
        .service_workers_mode(ServiceWorkersMode::All)
        .policy_container(document_context.policy_container.clone())
        .client(document_context.request_client.clone());

        let core_resource_thread_clone = font_context.resource_threads.lock().clone();

        debug!("Loading @font-face {} from {}", web_font_family_name, url);
        let mut downloader = Self {
            url,
            web_font_family_name,
            response_valid: false,
            response_data: Vec::new(),
            document_context,
            font_context: font_context.clone(),
        };

        fetch_async(
            &core_resource_thread_clone,
            request,
            None,
            Box::new(move |response_message| {
                match downloader.handle_web_font_fetch_message(response_message) {
                    DownloaderResponseResult::InProcess => {},
                    DownloaderResponseResult::Finished => {
                        downloader.process_downloaded_font_and_signal_completion()
                    },
                    DownloaderResponseResult::Failure => {
                        font_context.handle_web_font_request_failed(downloader.url.clone().into());
                    },
                }
            }),
        )
    }

    /// After a download finishes, try to process the downloaded data, returning true if
    /// the font is added successfully to the [`FontContext`] or false if it isn't.
    fn process_downloaded_font_and_signal_completion(&mut self) {
        let font_data = std::mem::take(&mut self.response_data);
        trace!(
            "Downloaded @font-face {} ({} bytes)",
            self.web_font_family_name,
            font_data.len()
        );

        let font_data = match fontsan::process(&font_data) {
            Ok(bytes) => FontData::from_bytes(&bytes),
            Err(error) => {
                debug!(
                    "Sanitiser rejected web font url={:?} with {error:?}",
                    self.url.as_str(),
                );
                return self
                    .font_context
                    .handle_web_font_request_failed(self.url.clone().into());
            },
        };

        let url: ServoUrl = self.url.clone().into();
        self.font_context
            .handle_web_font_request_succeeded(font_data, url);
    }

    fn handle_web_font_fetch_message(
        &mut self,
        response_message: FetchResponseMsg,
    ) -> DownloaderResponseResult {
        match response_message {
            FetchResponseMsg::ProcessRequestBody(..) => DownloaderResponseResult::InProcess,
            FetchResponseMsg::ProcessCspViolations(_request_id, violations) => {
                self.document_context
                    .csp_handler
                    .process_violations(violations);
                DownloaderResponseResult::InProcess
            },
            FetchResponseMsg::ProcessResponse(_, meta_result) => {
                trace!(
                    "@font-face {} metadata ok={:?}",
                    self.web_font_family_name,
                    meta_result.is_ok()
                );
                self.response_valid = meta_result.is_ok();
                DownloaderResponseResult::InProcess
            },
            FetchResponseMsg::ProcessResponseChunk(_, new_bytes) => {
                trace!(
                    "@font-face {} chunk={:?}",
                    self.web_font_family_name, new_bytes
                );
                if self.response_valid {
                    self.response_data.extend(new_bytes.0)
                }
                DownloaderResponseResult::InProcess
            },
            FetchResponseMsg::ProcessResponseEOF(_, response, timing) => {
                trace!(
                    "@font-face {} EOF={:?}",
                    self.web_font_family_name, response
                );
                if response.is_err() || !self.response_valid {
                    return DownloaderResponseResult::Failure;
                }
                self.document_context
                    .network_timing_handler
                    .submit_timing(ServoUrl::from_url(self.url.as_ref().clone()), timing);
                DownloaderResponseResult::Finished
            },
            FetchResponseMsg::ProcessContentLength(_request_id, size) => {
                self.response_data.reserve(size - self.response_data.len());
                DownloaderResponseResult::InProcess
            },
        }
    }
}

fn resolve_url_source_or_continue<T>(
    url_source: &UrlSource,
    state: T,
    continue_with: impl FnOnce(T),
) -> Option<(ServoArc<Url>, T)> {
    let Some(url) = url_source.url.url() else {
        continue_with(state);
        return None;
    };

    Some((url.clone(), state))
}

type PreparedRemoteFontDownload<T> = Result<(ServoArc<Url>, T), (timers::DocumentClockError, T)>;

fn prepare_remote_font_download<T>(
    url_source: &UrlSource,
    state: T,
    document_context: &WebFontDocumentContext,
    continue_with: impl FnOnce(T),
) -> Option<PreparedRemoteFontDownload<T>> {
    let (url, state) = resolve_url_source_or_continue(url_source, state, continue_with)?;
    Some(match document_context.require_remote_font_io() {
        Ok(()) => Ok((url, state)),
        Err(error) => Err((error, state)),
    })
}

#[derive(Debug, Eq, Hash, MallocSizeOf, PartialEq)]
struct FontCacheKey {
    font_identifier: FontIdentifier,
    font_descriptor: FontDescriptor,
}

#[derive(Debug, MallocSizeOf)]
struct FontGroupCacheKey {
    #[ignore_malloc_size_of = "This is also stored as part of styling."]
    style: ServoArc<FontStyleStruct>,
    size: Au,
}

impl PartialEq for FontGroupCacheKey {
    fn eq(&self, other: &FontGroupCacheKey) -> bool {
        self.style == other.style && self.size == other.size
    }
}

impl Eq for FontGroupCacheKey {}

impl Hash for FontGroupCacheKey {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.style.hash.hash(hasher)
    }
}

#[derive(Default, MallocSizeOf)]
struct KnownFontFaceRules {
    /// Used to distinguish new, incoming `@font-face` rules from existing ones.
    ///
    /// Generations alternate between true and false, which is enough to tell one generation apart from
    /// the next.
    generation: bool,
    /// Maps from a font family name to a list of `@font-face` rules declaring fonts
    /// that belong to said family.
    contents: HashMap<Atom, Vec<KnownFontFaceRule>>,
}

#[derive(MallocSizeOf)]
struct KnownFontFaceRule {
    rule_with_origin: FontFaceRuleWithOrigin,
    generation: bool,
}

impl KnownFontFaceRules {
    fn contains_rule(&self, target_rule: &ServoArc<LockedFontFaceRule>) -> bool {
        self.contents
            .values()
            .flat_map(|bucket| bucket.iter())
            .any(|known_rule| ServoArc::ptr_eq(&known_rule.rule_with_origin.rule, target_rule))
    }

    /// Computes the difference between the `@font-face `rules that are currently in effect
    /// and the ones that the `Stylist` knows about. The caller is notified about new or removed rules
    /// with callbacks.
    fn diff_old_and_new_font_face_rules(
        &mut self,
        stylist: &Stylist,
        guards: &StylesheetGuards<'_>,
    ) -> WebFontSetDifference {
        let mut difference = WebFontSetDifference::default();
        self.generation = !self.generation;

        let font_face_rules_in_cascade_order = stylist
            .iter_extra_data_origins()
            .flat_map(|(extra_data, origin)| {
                extra_data.font_faces.iter().rev().zip(iter::repeat(origin))
            })
            .map(|((rule, _layer), origin)| FontFaceRuleWithOrigin::new(rule.clone(), origin));

        // First, find any *new* font families that were not defined previously
        let mut number_of_unchanged_rules = 0;
        let number_of_previously_known_rules: usize = self
            .contents
            .values()
            .map(|fonts_from_family| fonts_from_family.len())
            .sum();
        for rule_with_origin in font_face_rules_in_cascade_order {
            let borrowed_rule = rule_with_origin.read_with(guards);

            let Some(font_family) = borrowed_rule.descriptors.font_family.as_ref() else {
                // Per https://github.com/w3c/csswg-drafts/issues/1133 an @font-face rule
                // is valid as far as the CSS parser is concerned even if it doesn’t have
                // a font-family or src declaration.
                // However, both are required for the rule to represent an actual font face.
                continue;
            };
            if borrowed_rule.descriptors.src.is_none() {
                // @font-face rules without a src don't constitute usable font faces.
                continue;
            }

            let known_font_faces_for_family =
                self.contents.entry(font_family.name.clone()).or_default();

            let mut conflicting_declaration_with_higher_priority_exists = false;
            let mut index_of_existing_entry_for_this_rule = None;
            for (index, known_font_face) in known_font_faces_for_family.iter().enumerate() {
                // See if this is a entry for this @font-face that existed prior to the current update
                if FontFaceRuleWithOrigin::ptr_eq(
                    &known_font_face.rule_with_origin,
                    &rule_with_origin,
                ) {
                    index_of_existing_entry_for_this_rule = Some(index);
                }

                // Check if there are existing declarations with higher priority that conflict
                if conflicting_declaration_with_higher_priority_exists {
                    // We already found one conflict, no need to search for more.
                    continue;
                }
                if known_font_face.generation != self.generation {
                    // This rule was not inserted yet during this update, so it was either removed or
                    // has lower priority than the one currently being inserted.
                    continue;
                }
                if font_face_rules_conflict(
                    &known_font_face
                        .rule_with_origin
                        .read_with(guards)
                        .descriptors,
                    &borrowed_rule.descriptors,
                ) {
                    conflicting_declaration_with_higher_priority_exists = true;
                }
            }

            if let Some(index_of_existing_entry_for_this_rule) =
                index_of_existing_entry_for_this_rule
            {
                // This @font-face rule was already present in the cascade prior to this update.
                // But if during this update we inserted a rule with higher priority that overrides this one
                // then we should not update its generation so it will be dropped at the end.
                if conflicting_declaration_with_higher_priority_exists {
                    let stale_rule =
                        known_font_faces_for_family.remove(index_of_existing_entry_for_this_rule);
                    difference
                        .removed_font_faces
                        .push(stale_rule.rule_with_origin);
                } else {
                    number_of_unchanged_rules += 1;
                    known_font_faces_for_family[index_of_existing_entry_for_this_rule].generation =
                        self.generation;
                }
            } else if conflicting_declaration_with_higher_priority_exists {
                // This (new) rule does not apply to the document because another rule with higher cascade priority
                // overrides it. We can simply ignore this declaration.
                continue;
            } else {
                // This is a new rule that does not conflict with anything that previously existed, so insert it.
                difference.added_font_faces.push(rule_with_origin.clone());
                known_font_faces_for_family.push(KnownFontFaceRule {
                    rule_with_origin,
                    generation: self.generation,
                });
            }
        }

        if number_of_unchanged_rules == number_of_previously_known_rules {
            // This is the common case, where the new set of known @font-face rules is a superset of
            // the old one after applying the cascade. In this case there is nothing more to do,
            // because all old @font-face rules are still present.
            return difference;
        }

        // Remove all `@font-face` rules that were not updated - those no longer exist on the stylist.
        self.contents.retain(|_, known_font_faces_for_family| {
            known_font_faces_for_family
                .extract_if(.., |rule| rule.generation != self.generation)
                .for_each(|removed_rule| {
                    difference
                        .removed_font_faces
                        .push(removed_rule.rule_with_origin);
                });

            !known_font_faces_for_family.is_empty()
        });

        difference
    }
}

/// Returns `true` if the two `@font-face` rules cannot both apply at the same time.
///
/// Two font faces can coexist if they are different for the purposes of font matching:
/// <https://drafts.csswg.org/css-fonts-4/#font-matching-algorithm>
///
/// This method does assume that the family names have already been verified to be equal.
fn font_face_rules_conflict(
    first_rule: &FontFaceRuleDescriptors,
    second_rule: &FontFaceRuleDescriptors,
) -> bool {
    first_rule.font_stretch == second_rule.font_stretch &&
        first_rule.font_style == second_rule.font_style &&
        first_rule.font_weight == second_rule.font_weight &&
        first_rule.unicode_range == second_rule.unicode_range
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use fonts_traits::{FontTemplateDescriptor, SystemFontServiceProxySender};
    use net_traits::request::{InsecureRequestsPolicy, Origin as RequestOrigin};
    use servo_base::generic_channel;
    use servo_url::ImmutableOrigin;
    use style::font_face::{FontFaceSourceTechFlags, Source, UrlSource};
    use style::shared_lock::SharedRwLock;
    use style::stylesheets::{FontFaceRule, Origin as StylesheetOrigin};
    use style::url::SpecifiedUrl;
    use style::values::computed::font::{FamilyName, FontFamilyNameSyntax};

    use super::*;

    #[derive(Debug, malloc_size_of_derive::MallocSizeOf)]
    struct IgnoreCspViolations;

    impl CspViolationHandler for IgnoreCspViolations {
        fn process_violations(&self, _violations: Vec<Violation>) {}

        fn clone(&self) -> Box<dyn CspViolationHandler> {
            Box::new(Self)
        }
    }

    #[derive(Debug, malloc_size_of_derive::MallocSizeOf)]
    struct IgnoreNetworkTiming;

    impl NetworkTimingHandler for IgnoreNetworkTiming {
        fn submit_timing(&self, _url: ServoUrl, _response: ResourceFetchTiming) {}

        fn clone(&self) -> Box<dyn NetworkTimingHandler> {
            Box::new(Self)
        }
    }

    fn test_font_context() -> Arc<FontContext> {
        let (system_font_sender, _system_font_receiver) = generic_channel::channel().unwrap();
        let system_font_service = SystemFontServiceProxySender(system_font_sender).to_proxy();
        let (resource_sender, _resource_receiver) = generic_channel::channel().unwrap();

        Arc::new(FontContext::new(
            Arc::new(system_font_service),
            CrossProcessPaintApi::dummy(),
            ResourceThreads::new(resource_sender),
        ))
    }

    fn test_document_context() -> WebFontDocumentContext {
        WebFontDocumentContext {
            policy_container: Default::default(),
            request_client: RequestClient {
                preloaded_resources: Default::default(),
                policy_container: Default::default(),
                origin: RequestOrigin::Origin(ImmutableOrigin::new_opaque()),
                is_nested_browsing_context: false,
                insecure_requests_policy: InsecureRequestsPolicy::DoNotUpgrade,
                has_trustworthy_ancestor_origin: false,
            },
            document_url: ServoUrl::parse("https://example.test/").unwrap(),
            csp_handler: Box::new(IgnoreCspViolations),
            network_timing_handler: Box::new(IgnoreNetworkTiming),
            document_clock: DocumentClock::default(),
        }
    }

    fn script_download_state(
        font_context: Arc<FontContext>,
        completions: Arc<AtomicUsize>,
    ) -> WebFontDownloadState {
        WebFontDownloadState::new(
            None,
            font_context,
            CSSFontFaceDescriptors::new("Test"),
            WebFontLoadInitiator::Script(Box::new(move |_, template| {
                assert!(template.is_none());
                completions.fetch_add(1, Ordering::SeqCst);
            })),
            Vec::new(),
            Default::default(),
            test_document_context(),
        )
    }

    fn locked_font_face_rule(
        lock: &SharedRwLock,
    ) -> ServoArc<style::shared_lock::Locked<FontFaceRule>> {
        ServoArc::new(lock.wrap(FontFaceRule::empty(Default::default())))
    }

    fn known_rules_with(
        rule: ServoArc<style::shared_lock::Locked<FontFaceRule>>,
    ) -> KnownFontFaceRules {
        let mut known_rules = KnownFontFaceRules::default();
        known_rules.contents.insert(
            Atom::from("Test"),
            vec![KnownFontFaceRule {
                rule_with_origin: FontFaceRuleWithOrigin::new(rule, StylesheetOrigin::Author),
                generation: false,
            }],
        );
        known_rules
    }

    fn url_source(url: &str) -> UrlSource {
        UrlSource {
            url: SpecifiedUrl::new_for_testing(url),
            format_hint: None,
            tech_flags: FontFaceSourceTechFlags::empty(),
        }
    }

    fn local_source(family_name: &str) -> Source {
        Source::Local(FamilyName {
            name: Atom::from(family_name),
            syntax: FontFamilyNameSyntax::Quoted,
        })
    }

    #[test]
    fn controlled_remote_font_io_is_rejected_and_realtime_is_unchanged() {
        let mut controlled = test_document_context();
        controlled.document_clock =
            DocumentClock::new(timers::DocumentClockConfiguration::Controlled {
                initial_time_ns: 0,
                unix_time_origin_ns: timers::DocumentUnixTime::from_nanos(0),
            });
        assert!(controlled.require_remote_font_io().is_err());
        assert_eq!(
            controlled.document_clock.unsupported_surface(),
            Some(DocumentTimeSurface::ResourceThreadIo)
        );

        let realtime = test_document_context();
        assert!(realtime.require_remote_font_io().is_ok());
        assert_eq!(realtime.document_clock.unsupported_surface(), None);
    }

    #[test]
    fn controlled_unresolved_url_continues_to_fallback_without_latching_io() {
        let mut document_context = test_document_context();
        document_context.document_clock =
            DocumentClock::new(timers::DocumentClockConfiguration::Controlled {
                initial_time_ns: 0,
                unix_time_origin_ns: timers::DocumentUnixTime::from_nanos(0),
            });
        let fallback = local_source("Fallback");
        let fallback_was_attempted = Cell::new(false);

        let prepared = prepare_remote_font_download(
            &url_source(""),
            vec![fallback.clone()],
            &document_context,
            |mut remaining_sources| {
                assert_eq!(remaining_sources.pop(), Some(fallback));
                fallback_was_attempted.set(true);
            },
        );

        assert!(prepared.is_none());
        assert!(fallback_was_attempted.get());
        assert_eq!(document_context.document_clock.unsupported_surface(), None);
    }

    #[test]
    fn realtime_resolved_url_remains_ready_for_fetch_dispatch() {
        let document_context = test_document_context();
        let prepared = prepare_remote_font_download(
            &url_source("https://example.test/font.woff2"),
            (),
            &document_context,
            |_| panic!("a resolved URL must not continue to the fallback source"),
        );

        assert!(matches!(prepared, Some(Ok((_url, ())))));
        assert_eq!(document_context.document_clock.unsupported_surface(), None);
    }

    #[test]
    fn controlled_resolved_remote_font_fails_before_fetch_dispatch() {
        let font_context = test_font_context();
        let completions = Arc::new(AtomicUsize::new(0));
        let mut state = script_download_state(font_context.clone(), completions.clone());
        let controlled_clock = DocumentClock::new(timers::DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: timers::DocumentUnixTime::from_nanos(0),
        });
        state.document_context.document_clock = controlled_clock.clone();
        let web_font_family_name = state.css_font_face_descriptors.family_name.clone();
        font_context
            .number_of_loading_web_fonts
            .store(1, Ordering::SeqCst);

        RemoteWebFontDownloader::download(
            url_source("https://example.test/font.woff2"),
            font_context.clone(),
            web_font_family_name,
            state,
        );

        assert_eq!(
            controlled_clock.unsupported_surface(),
            Some(DocumentTimeSurface::ResourceThreadIo)
        );
        assert_eq!(completions.load(Ordering::SeqCst), 1);
        assert_eq!(font_context.web_fonts_still_loading(), 0);
        assert!(font_context.currently_downloading_fonts.lock().is_empty());
    }

    #[test]
    fn platform_font_rejection_continues_every_subscriber() {
        let font_context = test_font_context();
        let completions = Arc::new(AtomicUsize::new(0));
        let url = ServoUrl::parse("https://example.test/rejected.woff2").unwrap();
        let font_data = FontData::from_bytes(b"not a font");

        assert!(
            PlatformFont::new_from_data(FontIdentifier::Web(url.clone()), &font_data, None, false,)
                .is_err()
        );

        font_context
            .number_of_loading_web_fonts
            .store(2, Ordering::SeqCst);
        assert!(font_context.handle_web_font_request_started(
            url.clone(),
            script_download_state(font_context.clone(), completions.clone()),
        ));
        assert!(!font_context.handle_web_font_request_started(
            url.clone(),
            script_download_state(font_context.clone(), completions.clone()),
        ));

        assert!(!font_context.handle_web_font_request_succeeded(font_data, url));
        assert_eq!(completions.load(Ordering::SeqCst), 2);
        assert_eq!(font_context.web_fonts_still_loading(), 0);
    }

    #[test]
    fn inactive_subscriber_does_not_abandon_later_subscribers() {
        let font_context = test_font_context();
        let script_completions = Arc::new(AtomicUsize::new(0));
        let stylesheet_completions = Arc::new(AtomicUsize::new(0));
        let url = ServoUrl::parse("https://example.test/shared.woff2").unwrap();
        let lock = SharedRwLock::new();
        let inactive_rule = locked_font_face_rule(&lock);
        let font_face_rule = inactive_rule.read_with(&lock.read()).descriptors.clone();
        let stylesheet_completion_count = stylesheet_completions.clone();
        let stylesheet_state = WebFontDownloadState::new(
            None,
            font_context.clone(),
            CSSFontFaceDescriptors::new("Test"),
            WebFontLoadInitiator::Stylesheet(Box::new(FontFaceRuleInitiator {
                created_by: inactive_rule,
                font_face_rule,
                callback: Arc::new(move |_| {
                    stylesheet_completion_count.fetch_add(1, Ordering::SeqCst);
                }),
            })),
            Vec::new(),
            Default::default(),
            test_document_context(),
        );

        font_context
            .number_of_loading_web_fonts
            .store(2, Ordering::SeqCst);
        assert!(font_context.handle_web_font_request_started(url.clone(), stylesheet_state));
        assert!(!font_context.handle_web_font_request_started(
            url.clone(),
            script_download_state(font_context.clone(), script_completions.clone()),
        ));

        font_context.handle_web_font_request_failed(url);

        assert_eq!(stylesheet_completions.load(Ordering::SeqCst), 0);
        assert_eq!(script_completions.load(Ordering::SeqCst), 1);
        assert_eq!(font_context.web_fonts_still_loading(), 0);
    }

    #[test]
    fn stylesheet_removal_serializes_with_successful_download_commit() {
        let lock = SharedRwLock::new();
        let rule = locked_font_face_rule(&lock);
        let known_rules = Arc::new(Mutex::new(known_rules_with(rule.clone())));
        let web_fonts = Arc::new(CrossThreadFontStore::default());
        let family_name: LowercaseFontFamilyName = Atom::from("Test").into();
        let template = FontTemplate::new(
            FontIdentifier::Web(ServoUrl::parse("https://example.test/serialized.woff2").unwrap()),
            FontTemplateDescriptor::default(),
            None,
        );

        // Stall completion after it locks the active-rule set but before it can commit the
        // template. Stylesheet removal must wait, then remove the committed template.
        let web_fonts_guard = web_fonts.write();
        let completion = {
            let known_rules = known_rules.clone();
            let web_fonts = web_fonts.clone();
            let rule = rule.clone();
            let family_name = family_name.clone();
            thread::spawn(move || {
                add_stylesheet_web_font_template_if_active(
                    &known_rules,
                    &web_fonts,
                    &rule,
                    family_name,
                    template,
                )
            })
        };

        let deadline = Instant::now() + Duration::from_secs(10);
        while known_rules.try_lock().is_some() {
            assert!(
                Instant::now() < deadline,
                "font completion did not lock the active-rule set"
            );
            thread::yield_now();
        }

        let removal = {
            let known_rules = known_rules.clone();
            let web_fonts = web_fonts.clone();
            let family_name = family_name.clone();
            thread::spawn(move || {
                known_rules.lock().contents.clear();
                web_fonts.write().families.remove(&family_name);
            })
        };

        drop(web_fonts_guard);
        assert!(completion.join().unwrap());
        removal.join().unwrap();

        assert!(!known_rules.lock().contains_rule(&rule));
        assert!(!web_fonts.read().families.contains_key(&family_name));
    }
}
