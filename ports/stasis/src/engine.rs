/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Once;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use net_traits::controlled_network::ControlledNetworkSnapshot;
use net_traits::network_evidence::{
    EvidenceLedgerError, EvidenceSequence, NetworkEvidencePage, NetworkFailureReason,
    NetworkRequestsPage,
};
use net_traits::network_fixture::NetworkFixtureError;
use serde_json::{Map, Number, Value, json};
use servo::document_control::{DocumentControlCommand, DocumentControlError};
use servo::{
    ControlledNetworkConfigurationError, ControlledNetworkSession, ControlledNetworkTimeError,
    DocumentClockConfiguration, DocumentClockError, DocumentControlProfileError,
    DocumentExecutionProfileError, GenericReceiver, JSValue, JavaScriptEvaluationError, LoadStatus,
    Preferences, RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext,
    UnpublishedWebViewInitializationError, WebView, WebViewBuilder, WebViewDelegate,
};
pub use servo::{
    DocumentControlProfile, DocumentExecutionProfile, SessionNavigationAuthority,
    SessionNavigationError, SessionNavigationId,
};
use servo_base::generic_channel::TryReceiveError;
use stasis_shell::session_state::{
    self, LiveSessionStateBackend, ServoSessionStateBackend, SessionCookiesResultV1,
    SessionCookiesSetParamsV1, SessionStateAuthority, SessionStateError,
    SessionStateExportResultV1, SessionStateMutationResultV1, SessionStateToken, SessionStateV1,
    SessionStorageResultV1, SessionStorageSetParamsV1,
};
use url::Url;

use crate::protocol::ProtocolError;
use crate::wake::{ShellWaker, WaitError};

#[path = "operation.rs"]
mod operation;

pub use operation::{
    ControlOperationCompletion, ControlOperationPoll, ControlOutcomeDisposition,
    PendingControlOperation,
};

const DEFAULT_WALL_TIMEOUT: Duration = Duration::from_secs(30);
static SERVO_LOGGING: Once = Once::new();
const CONTROLLED_DISABLED_BOOLEAN_PREFERENCE_COUNT: usize = 75;

// Keep this inventory as a single source for policy application, diagnostics, and drift tests.
// Every entry is written to false even when Servo currently defaults it to false: an upstream
// default change must not silently widen the Controlled v0.1 profile. Unconditional APIs are
// rejected at their owner boundaries instead. The exact count is asserted in the tests below.
macro_rules! define_controlled_disabled_boolean_preferences {
    ($($field:ident),+ $(,)?) => {
        const CONTROLLED_DISABLED_BOOLEAN_PREFERENCES: &[&str] = &[
            $(stringify!($field)),+
        ];

        fn apply_controlled_preference_policy(preferences: &mut Preferences) {
            debug_assert_eq!(
                CONTROLLED_DISABLED_BOOLEAN_PREFERENCES.len(),
                CONTROLLED_DISABLED_BOOLEAN_PREFERENCE_COUNT,
            );
            $(preferences.$field = false;)+
        }

        #[cfg(test)]
        fn set_controlled_disabled_boolean_preferences(
            preferences: &mut Preferences,
            value: bool,
        ) {
            $(preferences.$field = value;)+
        }

        #[cfg(test)]
        fn controlled_disabled_boolean_preference_values(
            preferences: &Preferences,
        ) -> Vec<bool> {
            vec![$(preferences.$field),+]
        }
    };
}

define_controlled_disabled_boolean_preferences! {
    // Background producers which are not represented in the v0.1 pending-state proof.
    dom_allow_preloading_module_descendants,
    dom_parallel_css_parsing_enabled,
    dom_script_asynch,
    dom_servoparser_async_html_tokenizer_enabled,
    js_offthread_compilation_enabled,

    // External storage, network-adjacent, system, and device capabilities.
    accessibility_enabled,
    devtools_server_enabled,
    dom_allow_scripts_to_close_windows,
    dom_async_clipboard_enabled,
    dom_bluetooth_enabled,
    dom_bluetooth_testing_enabled,
    dom_canvas_capture_enabled,
    dom_cookiestore_enabled,
    dom_credential_management_enabled,
    dom_crypto_subtle_enabled,
    dom_entries_api_enabled,
    dom_fontface_enabled,
    dom_gamepad_enabled,
    dom_geolocation_enabled,
    dom_indexeddb_enabled,
    dom_intersection_observer_enabled,
    dom_navigator_protocol_handlers_enabled,
    dom_notification_enabled,
    dom_offscreen_canvas_enabled,
    dom_permissions_enabled,
    dom_resize_observer_enabled,
    dom_serviceworker_enabled,
    dom_sharedworker_enabled,
    dom_storage_manager_api_enabled,
    dom_wakelock_enabled,
    dom_web_animations_enabled,
    dom_webgl2_enabled,
    dom_webgpu_enabled,
    dom_webrtc_enabled,
    dom_webrtc_transceiver_enabled,
    largest_contentful_paint_enabled,
    media_glvideo_enabled,
    network_local_directory_listing_enabled,

    // WebXR and Worklet expose external devices or independent execution agents.
    dom_webxr_enabled,
    dom_webxr_first_person_observer_view,
    dom_webxr_glwindow_cubemap,
    dom_webxr_glwindow_enabled,
    dom_webxr_glwindow_left_right,
    dom_webxr_glwindow_red_cyan,
    dom_webxr_glwindow_spherical,
    dom_webxr_hands_enabled,
    dom_webxr_layers_enabled,
    dom_webxr_openxr_enabled,
    dom_webxr_sessionavailable,
    dom_webxr_test,
    dom_webxr_unsafe_assume_user_intent,
    dom_worklet_blockingsleep_enabled,
    dom_worklet_enabled,
    dom_worklet_testing_enabled,

    // Test-only and internal exposure surfaces.
    css_animations_testing_enabled,
    dom_fullscreen_test,
    dom_microdata_testing_enabled,
    dom_permissions_testing_allowed_in_nonsecure_contexts,
    dom_servo_helpers_enabled,
    dom_testbinding_enabled,
    dom_testbinding_prefcontrolled_enabled,
    dom_testbinding_prefcontrolled2_enabled,
    dom_testbinding_preference_value_falsy,
    dom_testbinding_preference_value_truthy,
    dom_testing_element_activation_enabled,
    dom_testing_html_input_element_select_files_enabled,
    dom_testperf_enabled,
    dom_testutils_enabled,
    expensive_accessibility_test_assertions_enabled,
    expose_servointernals_globally,
    inspector_show_servo_internal_shadow_roots,
    layout_animations_test_enabled,
    layout_unimplemented,
    media_testing_enabled,
    webgl_testing_context_creation_error,
}

fn controlled_preferences(document_control_profile: DocumentControlProfile) -> Preferences {
    let mut preferences = Preferences::default();
    apply_controlled_preference_policy(&mut preferences);
    // controlled-web-session-v1 owns CookieStore.set synchronously and rejects read/delete before
    // ordinary resource-thread callbacks. Keep the surface absent from frozen v0.1, but expose it
    // for the v0.2 top-level-session profile where that complete boundary is installed.
    if document_control_profile == DocumentControlProfile::TopLevelSession {
        preferences.dom_cookiestore_enabled = true;
    }
    preferences
}

fn preferences_for_clock_mode(
    clock_mode: EngineClockMode,
    document_control_profile: DocumentControlProfile,
) -> Option<Preferences> {
    match clock_mode {
        // Leaving the builder unset preserves ServoBuilder's ordinary default behavior exactly.
        EngineClockMode::Real => None,
        EngineClockMode::Controlled { .. } => {
            Some(controlled_preferences(document_control_profile))
        },
    }
}

/// Immutable document-clock selection for one shell WebView.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EngineClockMode {
    /// Preserve ordinary interactive Servo time and automatic frame painting.
    #[default]
    Real,
    /// Start document-observable time at a deterministic monotonic offset.
    Controlled {
        /// Initial document time in integer nanoseconds.
        initial_time_ns: u128,
    },
}

impl EngineClockMode {
    fn document_clock(self) -> DocumentClockConfiguration {
        match self {
            Self::Real => DocumentClockConfiguration::Realtime,
            Self::Controlled { initial_time_ns } => DocumentClockConfiguration::Controlled {
                initial_time_ns,
                unix_time_origin_ns: Default::default(),
            },
        }
    }

    const fn paints_frames_automatically(self) -> bool {
        matches!(self, Self::Real)
    }

    pub const fn is_controlled(self) -> bool {
        matches!(self, Self::Controlled { .. })
    }
}

/// Immutable construction inputs which must cross the builder before the first request starts.
pub struct EngineSessionOpenOptions {
    pub clock_mode: EngineClockMode,
    pub document_control_profile: DocumentControlProfile,
    pub document_execution_profile: DocumentExecutionProfile,
    pub state: Option<SessionStateV1>,
    pub network: Option<Value>,
}

struct ShellDelegate {
    paints_frames_automatically: bool,
}

impl WebViewDelegate for ShellDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        if self.paints_frames_automatically {
            webview.paint();
        }
    }
}

/// Owner-loop state of the single supported document-control operation.
pub enum EngineControlPoll {
    /// No control command is currently in flight.
    Idle,
    /// The command is still waiting for its typed response.
    Pending {
        /// Checked wall deadline attached at submission.
        deadline: Instant,
    },
    /// The command produced its only terminal result.
    Complete(ControlOperationCompletion),
}

/// Whether the one in-flight session-navigation callback is passive or may have admitted work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationOperationKind {
    Observe,
    Navigate,
}

/// Terminal result of a checked session-navigation observation or admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationOperationCompletion {
    kind: NavigationOperationKind,
    outcome: Result<SessionNavigationAuthority, SessionNavigationError>,
    response_received: bool,
}

impl NavigationOperationCompletion {
    const fn response(
        kind: NavigationOperationKind,
        outcome: Result<SessionNavigationAuthority, SessionNavigationError>,
    ) -> Self {
        Self {
            kind,
            outcome,
            response_received: true,
        }
    }

    #[cfg(test)]
    pub(crate) const fn test_response(
        kind: NavigationOperationKind,
        outcome: Result<SessionNavigationAuthority, SessionNavigationError>,
    ) -> Self {
        Self::response(kind, outcome)
    }

    #[cfg(test)]
    pub(crate) fn test_transport_failure(kind: NavigationOperationKind) -> Self {
        Self::transport_failure(kind)
    }

    fn transport_failure(kind: NavigationOperationKind) -> Self {
        Self {
            kind,
            outcome: Err(SessionNavigationError::ChannelClosed),
            response_received: false,
        }
    }

    pub const fn kind(&self) -> NavigationOperationKind {
        self.kind
    }

    /// A missing response after Navigate was submitted makes admission indeterminate.
    pub const fn response_received(&self) -> bool {
        self.response_received
    }

    pub fn outcome(&self) -> &Result<SessionNavigationAuthority, SessionNavigationError> {
        &self.outcome
    }

    pub fn into_outcome(self) -> Result<SessionNavigationAuthority, SessionNavigationError> {
        self.outcome
    }
}

pub enum EngineNavigationPoll {
    Idle,
    Pending { deadline: Instant },
    Complete(NavigationOperationCompletion),
}

struct PendingNavigationOperation {
    kind: NavigationOperationKind,
    receiver: GenericReceiver<Result<SessionNavigationAuthority, SessionNavigationError>>,
    deadline: Instant,
}

impl PendingNavigationOperation {
    const fn new(
        kind: NavigationOperationKind,
        receiver: GenericReceiver<Result<SessionNavigationAuthority, SessionNavigationError>>,
        deadline: Instant,
    ) -> Self {
        Self {
            kind,
            receiver,
            deadline,
        }
    }

    fn poll(self, now: Instant) -> Result<Self, NavigationOperationCompletion> {
        match self.receiver.try_recv() {
            Ok(outcome) => Err(NavigationOperationCompletion::response(self.kind, outcome)),
            Err(TryReceiveError::Empty) if now < self.deadline => Ok(self),
            Err(TryReceiveError::Empty | TryReceiveError::ReceiveError(_)) => {
                Err(NavigationOperationCompletion::transport_failure(self.kind))
            },
        }
    }

    fn cancel(self) -> NavigationOperationCompletion {
        NavigationOperationCompletion::transport_failure(self.kind)
    }
}

pub struct EngineSession {
    // Drop the WebView before Servo so its close message still has an owner.
    webview: Option<WebView>,
    servo: Option<Servo>,
    _rendering_context: Rc<dyn RenderingContext>,
    waker: ShellWaker,
    clock_mode: EngineClockMode,
    document_control_profile: DocumentControlProfile,
    document_execution_profile: DocumentExecutionProfile,
    session_state_authority: Option<Rc<RefCell<SessionStateAuthority>>>,
    controlled_network: Option<ControlledNetworkSession>,
    pending_control: Option<PendingControlOperation>,
    pending_navigation: Option<PendingNavigationOperation>,
}

impl EngineSession {
    /// Preserve the original blocking Real-mode open boundary.
    pub fn open(url: Url, waker: ShellWaker) -> Result<Self, EngineError> {
        let session = Self::start(url, waker, EngineClockMode::Real)?;
        session.wait_for_load(DEFAULT_WALL_TIMEOUT)?;
        Ok(session)
    }

    /// Construct the owner-thread engine and send its initial navigation without blocking.
    ///
    /// Controlled callers use this boundary so navigation can subsequently be driven through
    /// explicit document-control turns. The immutable clock is validated and installed before
    /// `WebViewBuilder::build` sends the initial navigation.
    pub fn start(
        url: Url,
        waker: ShellWaker,
        clock_mode: EngineClockMode,
    ) -> Result<Self, EngineError> {
        Self::start_with_profile(
            url,
            waker,
            clock_mode,
            DocumentControlProfile::SingleDocument,
        )
    }

    /// Construct an owner-thread engine with an explicit checked document-control profile.
    ///
    /// `TopLevelSession` is the internal controlled-web-session-v1 seam. It keeps one WebView and
    /// one controlled Script event loop authoritative across replacement top-level documents.
    pub fn start_with_profile(
        url: Url,
        waker: ShellWaker,
        clock_mode: EngineClockMode,
        document_control_profile: DocumentControlProfile,
    ) -> Result<Self, EngineError> {
        Self::start_with_options(
            url,
            waker,
            EngineSessionOpenOptions {
                clock_mode,
                document_control_profile,
                document_execution_profile: DocumentExecutionProfile::Baseline,
                state: None,
                network: None,
            },
        )
    }

    /// Construct a session with every stateful v0.2 input installed before initial navigation.
    pub fn start_with_options(
        url: Url,
        waker: ShellWaker,
        options: EngineSessionOpenOptions,
    ) -> Result<Self, EngineError> {
        let EngineSessionOpenOptions {
            clock_mode,
            document_control_profile,
            document_execution_profile,
            state,
            network,
        } = options;
        let top_level_session = document_control_profile == DocumentControlProfile::TopLevelSession;
        if !top_level_session && (state.is_some() || network.is_some()) {
            return Err(EngineError::SessionConfiguration(
                "state and network configuration require a top-level controlled session",
            ));
        }
        if let Some(state) = state.as_ref() {
            session_state::validate_state(state).map_err(EngineError::SessionStateValidation)?;
        }
        let controlled_network = if top_level_session {
            let fixture_value = network.unwrap_or_else(|| {
                json!({
                    "mode": "live",
                    "routes": [],
                })
            });
            let initial_virtual_time_ns = match clock_mode {
                EngineClockMode::Controlled { initial_time_ns } => initial_time_ns,
                EngineClockMode::Real => {
                    return Err(EngineError::SessionConfiguration(
                        "top-level controlled sessions require a controlled clock",
                    ));
                },
            };
            Some(
                ControlledNetworkSession::from_json(fixture_value, initial_virtual_time_ns)
                    .map_err(EngineError::NetworkFixture)?,
            )
        } else {
            None
        };
        let session_state_authority =
            top_level_session.then(|| Rc::new(RefCell::new(SessionStateAuthority::new())));

        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(PhysicalSize::new(1024, 768))
                .map_err(|error| EngineError::Startup(format!("rendering context: {error:?}")))?,
        );
        rendering_context
            .make_current()
            .map_err(|error| EngineError::Startup(format!("make current: {error:?}")))?;

        let servo_builder = ServoBuilder::default().event_loop_waker(Box::new(waker.clone()));
        let servo_builder = match preferences_for_clock_mode(clock_mode, document_control_profile) {
            Some(preferences) => servo_builder.preferences(preferences),
            None => servo_builder,
        };
        let servo = servo_builder.build();

        let delegate = Rc::new(ShellDelegate {
            paints_frames_automatically: clock_mode.paints_frames_automatically(),
        });
        let builder = WebViewBuilder::new(&servo, rendering_context.clone())
            .url(url)
            .delegate(delegate)
            .document_clock(clock_mode.document_clock())
            .map_err(EngineError::ClockConfiguration)?
            .document_control_profile(document_control_profile)
            .map_err(EngineError::ControlProfileConfiguration)?
            .document_execution_profile(document_execution_profile)
            .map_err(EngineError::ExecutionProfileConfiguration)?;
        let builder = if let Some(network) = controlled_network.as_ref() {
            builder
                .controlled_network_session(network.clone())
                .map_err(EngineError::ControlledNetworkConfiguration)?
        } else {
            builder
        };
        let initialization_error = Rc::new(RefCell::new(None));
        let webview = if let Some(authority) = session_state_authority.as_ref() {
            let authority = authority.clone();
            let initialization_error_for_callback = initialization_error.clone();
            let builder = builder
                .unpublished_initializer(Box::new(move |webview_id, site_data| {
                    match session_state::initialize_servo_session_state_before_publication(
                        site_data,
                        webview_id,
                        &mut authority.borrow_mut(),
                        state,
                    ) {
                        Ok(_) => Ok(()),
                        Err(error) => {
                            initialization_error_for_callback
                                .borrow_mut()
                                .replace(error);
                            Err(UnpublishedWebViewInitializationError::Rejected)
                        },
                    }
                }))
                .map_err(EngineError::UnpublishedInitialization)?;
            match builder.build_checked() {
                Ok(webview) => webview,
                Err(error) => {
                    if let Some(state_error) = initialization_error.borrow_mut().take() {
                        return Err(EngineError::SessionStateInitialization(state_error));
                    }
                    return Err(EngineError::UnpublishedInitialization(error));
                },
            }
        } else {
            builder.build()
        };

        let session = Self {
            webview: Some(webview),
            servo: Some(servo),
            _rendering_context: rendering_context,
            waker,
            clock_mode,
            document_control_profile,
            document_execution_profile,
            session_state_authority,
            controlled_network,
            pending_control: None,
            pending_navigation: None,
        };
        // Servo installs a process-global logger and panics if it is installed twice. The shell
        // can construct a replacement session after a recoverable open failure, so keep that
        // global boundary independent from an individual EngineSession's lifetime.
        SERVO_LOGGING.call_once(|| session.servo().setup_logging());
        Ok(session)
    }

    pub fn pump(&self) {
        self.servo().spin_event_loop();
    }

    pub fn url(&self) -> Option<Url> {
        self.webview().url()
    }

    pub const fn clock_mode(&self) -> EngineClockMode {
        self.clock_mode
    }

    pub const fn document_control_profile(&self) -> DocumentControlProfile {
        self.document_control_profile
    }

    pub const fn document_execution_profile(&self) -> DocumentExecutionProfile {
        self.document_execution_profile
    }

    pub fn session_state_token(&self) -> Result<SessionStateToken, SessionStateError> {
        let authority = self.session_state_authority()?;
        let backend =
            ServoSessionStateBackend::new(self.servo().site_data_manager(), self.webview().id());
        let revisions = backend.revisions().map_err(|_| {
            SessionStateError::BackendRejected(session_state::SessionStateBackendStage::Observe)
        })?;
        authority.borrow_mut().observe(revisions)
    }

    pub fn session_cookies_get(&self) -> Result<SessionCookiesResultV1, SessionStateError> {
        let authority = self.session_state_authority()?;
        let backend =
            ServoSessionStateBackend::new(self.servo().site_data_manager(), self.webview().id());
        session_state::session_cookies_get(&backend, &mut authority.borrow_mut())
    }

    pub fn session_storage_get(&self) -> Result<SessionStorageResultV1, SessionStateError> {
        let authority = self.session_state_authority()?;
        let backend =
            ServoSessionStateBackend::new(self.servo().site_data_manager(), self.webview().id());
        session_state::session_storage_get(&backend, &mut authority.borrow_mut())
    }

    pub fn session_state_export(&self) -> Result<SessionStateExportResultV1, SessionStateError> {
        let authority = self.session_state_authority()?;
        let backend =
            ServoSessionStateBackend::new(self.servo().site_data_manager(), self.webview().id());
        session_state::session_state_export(&backend, &mut authority.borrow_mut())
    }

    pub fn session_cookies_set(
        &self,
        params: SessionCookiesSetParamsV1,
    ) -> Result<SessionStateMutationResultV1, SessionStateError> {
        let authority = self.session_state_authority()?;
        let mut backend =
            ServoSessionStateBackend::new(self.servo().site_data_manager(), self.webview().id());
        session_state::session_cookies_set(&mut backend, &mut authority.borrow_mut(), params)
    }

    pub fn session_storage_set(
        &self,
        params: SessionStorageSetParamsV1,
    ) -> Result<SessionStateMutationResultV1, SessionStateError> {
        let authority = self.session_state_authority()?;
        let mut backend =
            ServoSessionStateBackend::new(self.servo().site_data_manager(), self.webview().id());
        session_state::session_storage_set(&mut backend, &mut authority.borrow_mut(), params)
    }

    pub fn controlled_network_snapshot(&self) -> Option<ControlledNetworkSnapshot> {
        self.controlled_network
            .as_ref()
            .map(ControlledNetworkSession::snapshot)
    }

    pub fn set_controlled_network_virtual_time_ns(
        &self,
        virtual_time_ns: u128,
    ) -> Result<(), ControlledNetworkTimeError> {
        if let Some(network) = self.controlled_network.as_ref() {
            network.set_virtual_time_ns(virtual_time_ns)?;
        }
        Ok(())
    }

    pub fn network_requests_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkRequestsPage, EvidenceLedgerError> {
        self.controlled_network().requests_page(after, limit)
    }

    pub fn network_evidence_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkEvidencePage, EvidenceLedgerError> {
        self.controlled_network().evidence_page(after, limit)
    }

    pub fn record_navigation_started(&self, authority: &SessionNavigationAuthority) {
        self.record_navigation_started_id(authority.navigation_id());
    }

    pub fn record_navigation_started_id(&self, navigation_id: SessionNavigationId) {
        if let Some(network) = self.controlled_network.as_ref() {
            network.record_navigation_started(navigation_id);
        }
    }

    pub fn record_navigation_committed(&self, authority: &SessionNavigationAuthority) {
        if let Some(network) = self.controlled_network.as_ref() {
            network.record_navigation_committed(authority.navigation_id());
        }
    }

    pub fn record_navigation_failed(
        &self,
        authority: &SessionNavigationAuthority,
        reason: NetworkFailureReason,
    ) {
        self.record_navigation_failed_id(authority.navigation_id(), reason);
    }

    pub fn record_navigation_failed_id(
        &self,
        navigation_id: SessionNavigationId,
        reason: NetworkFailureReason,
    ) {
        if let Some(network) = self.controlled_network.as_ref() {
            network.record_navigation_failed(navigation_id, reason);
        }
    }

    pub fn record_same_document_history_changed(&self, authority: &SessionNavigationAuthority) {
        if let Some(network) = self.controlled_network.as_ref() {
            network.record_same_document_history_changed(authority.navigation_id());
        }
    }

    pub fn record_settlement_terminal(&self, authority: &SessionNavigationAuthority) {
        if let Some(network) = self.controlled_network.as_ref() {
            network.record_settlement_terminal(authority.navigation_id());
        }
    }

    fn session_state_authority(
        &self,
    ) -> Result<Rc<RefCell<SessionStateAuthority>>, SessionStateError> {
        self.session_state_authority
            .as_ref()
            .cloned()
            .ok_or(SessionStateError::BackendRejected(
                session_state::SessionStateBackendStage::Observe,
            ))
    }

    fn controlled_network(&self) -> &ControlledNetworkSession {
        self.controlled_network
            .as_ref()
            .expect("top-level controlled sessions always install a network controller")
    }

    /// Capture exact engine-owned controlled-session identity without blocking the owner loop.
    pub fn observe_session_navigation(
        &self,
    ) -> Result<
        GenericReceiver<Result<SessionNavigationAuthority, SessionNavigationError>>,
        EngineError,
    > {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession {
            return Err(EngineError::TopLevelSessionRequired);
        }
        if self.pending_control.is_some() || self.pending_navigation.is_some() {
            return Err(EngineError::ControlAlreadyPending);
        }
        let response_waker = self.waker.clone();
        self.webview()
            .observe_session_navigation(move || response_waker.notify_control_response())
            .map_err(EngineError::SessionNavigation)
    }

    /// Admit an HTTP(S) replacement against exact engine-owned session identity.
    ///
    /// The callback attests admission and its new pending target. It does not claim that the
    /// replacement activated or settled; the owner must drive controlled turns and re-observe.
    pub fn navigate(
        &self,
        expected: SessionNavigationAuthority,
        url: Url,
    ) -> Result<
        GenericReceiver<Result<SessionNavigationAuthority, SessionNavigationError>>,
        EngineError,
    > {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession {
            return Err(EngineError::TopLevelSessionRequired);
        }
        if self.pending_control.is_some() || self.pending_navigation.is_some() {
            return Err(EngineError::ControlAlreadyPending);
        }
        let response_waker = self.waker.clone();
        self.webview()
            .navigate_controlled_session(expected, url, move || {
                response_waker.notify_control_response()
            })
            .map_err(EngineError::SessionNavigation)
    }

    pub fn evaluate(&self, expression: &str) -> Result<Value, EngineError> {
        if self.clock_mode.is_controlled() {
            return Err(EngineError::BlockingHelperUnavailableInControlledMode(
                "evaluate",
            ));
        }
        if self.pending_control.is_some() || self.pending_navigation.is_some() {
            return Err(EngineError::ControlAlreadyPending);
        }
        let result = Rc::new(RefCell::new(None));
        let callback_result = result.clone();
        self.webview()
            .evaluate_javascript(expression, move |evaluation| {
                callback_result.borrow_mut().replace(evaluation);
            });

        self.drive_until(DEFAULT_WALL_TIMEOUT, || result.borrow().is_some())?;
        let evaluation = result
            .borrow_mut()
            .take()
            .expect("evaluation completion was observed");
        evaluation
            .map(js_value_to_json)
            .map_err(EngineError::Evaluation)
    }

    pub fn close(&mut self) {
        if let Some(operation) = self.pending_control.take() {
            let _ = operation.cancel();
        }
        self.pending_navigation.take();
        self.webview.take();
        if let Some(servo) = self.servo.as_ref() {
            servo.spin_event_loop();
        }
        self.servo.take();
    }

    /// Submit exactly one mechanical command and retain its consuming response receiver.
    ///
    /// The response callback advances only the shell's control-response wake generation. Servo
    /// work continues to use the independent event-loop generation, so the owner can pump Servo
    /// before polling a response without manufacturing page work from a callback notification.
    pub fn submit_document_control(
        &mut self,
        command: DocumentControlCommand,
        timeout: Duration,
    ) -> Result<(), EngineError> {
        if self.pending_control.is_some() || self.pending_navigation.is_some() {
            return Err(EngineError::ControlAlreadyPending);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(EngineError::ControlDeadlineOverflow)?;
        let response_waker = self.waker.clone();
        let receiver = self
            .webview()
            .submit_document_control(command, move || {
                response_waker.notify_control_response();
            })
            .map_err(EngineError::ControlSubmission)?;
        self.pending_control = Some(PendingControlOperation::new(receiver, deadline));
        Ok(())
    }

    /// Poll the in-flight response once without blocking the protocol owner thread.
    pub fn poll_control_operation(&mut self) -> EngineControlPoll {
        let Some(operation) = self.pending_control.take() else {
            return EngineControlPoll::Idle;
        };
        match operation.poll(Instant::now()) {
            ControlOperationPoll::Pending(operation) => {
                let deadline = operation.deadline();
                self.pending_control = Some(operation);
                EngineControlPoll::Pending { deadline }
            },
            ControlOperationPoll::Complete(completion) => EngineControlPoll::Complete(completion),
        }
    }

    /// Explicitly abandon the in-flight response during close or fail-stop cancellation.
    pub fn cancel_control_operation(&mut self) -> Option<ControlOperationCompletion> {
        self.pending_control
            .take()
            .map(PendingControlOperation::cancel)
    }

    /// Submit a passive checked session-authority observation and retain its callback.
    pub fn submit_session_navigation_observation(
        &mut self,
        timeout: Duration,
    ) -> Result<(), EngineError> {
        if self.pending_control.is_some() || self.pending_navigation.is_some() {
            return Err(EngineError::ControlAlreadyPending);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(EngineError::ControlDeadlineOverflow)?;
        let receiver = self.observe_session_navigation()?;
        self.pending_navigation = Some(PendingNavigationOperation::new(
            NavigationOperationKind::Observe,
            receiver,
            deadline,
        ));
        Ok(())
    }

    /// Submit an exact checked navigation admission and retain its callback.
    pub fn submit_session_navigation(
        &mut self,
        expected: SessionNavigationAuthority,
        url: Url,
        timeout: Duration,
    ) -> Result<(), EngineError> {
        if self.pending_control.is_some() || self.pending_navigation.is_some() {
            return Err(EngineError::ControlAlreadyPending);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(EngineError::ControlDeadlineOverflow)?;
        let receiver = self.navigate(expected, url)?;
        self.pending_navigation = Some(PendingNavigationOperation::new(
            NavigationOperationKind::Navigate,
            receiver,
            deadline,
        ));
        Ok(())
    }

    /// Poll the in-flight session-navigation callback once without blocking the owner loop.
    pub fn poll_session_navigation(&mut self) -> EngineNavigationPoll {
        let Some(operation) = self.pending_navigation.take() else {
            return EngineNavigationPoll::Idle;
        };
        match operation.poll(Instant::now()) {
            Ok(operation) => {
                let deadline = operation.deadline;
                self.pending_navigation = Some(operation);
                EngineNavigationPoll::Pending { deadline }
            },
            Err(completion) => EngineNavigationPoll::Complete(completion),
        }
    }

    /// Abandon the one in-flight callback. This never promises to undo navigation admission.
    pub fn cancel_session_navigation(&mut self) -> Option<NavigationOperationCompletion> {
        self.pending_navigation
            .take()
            .map(PendingNavigationOperation::cancel)
    }

    fn wait_for_load(&self, timeout: Duration) -> Result<(), EngineError> {
        self.drive_until(timeout, || {
            self.webview().load_status() == LoadStatus::Complete
        })
    }

    fn drive_until(
        &self,
        timeout: Duration,
        completed: impl Fn() -> bool,
    ) -> Result<(), EngineError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| EngineError::Startup("wall deadline overflowed".into()))?;
        while !completed() {
            let observed = self.waker.snapshot();
            self.servo().spin_event_loop();
            if completed() {
                break;
            }
            self.waker
                .wait_for_change(observed, deadline)
                .map_err(|WaitError::DeadlineExceeded| EngineError::WallTimeLimit)?;
        }
        Ok(())
    }

    fn servo(&self) -> &Servo {
        self.servo.as_ref().expect("engine session is closed")
    }

    fn webview(&self) -> &WebView {
        self.webview.as_ref().expect("engine session is closed")
    }
}

impl Drop for EngineSession {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug)]
pub enum EngineError {
    Startup(String),
    SessionConfiguration(&'static str),
    ClockConfiguration(DocumentClockError),
    ControlProfileConfiguration(DocumentControlProfileError),
    ExecutionProfileConfiguration(DocumentExecutionProfileError),
    ControlledNetworkConfiguration(ControlledNetworkConfigurationError),
    NetworkFixture(NetworkFixtureError),
    UnpublishedInitialization(UnpublishedWebViewInitializationError),
    SessionStateValidation(SessionStateError),
    SessionStateInitialization(SessionStateError),
    ControlSubmission(DocumentControlError),
    SessionNavigation(SessionNavigationError),
    ControlAlreadyPending,
    ControlDeadlineOverflow,
    BlockingHelperUnavailableInControlledMode(&'static str),
    TopLevelSessionRequired,
    WallTimeLimit,
    Evaluation(JavaScriptEvaluationError),
}

impl EngineError {
    pub fn to_protocol_error(&self) -> ProtocolError {
        match self {
            Self::Startup(message) => {
                ProtocolError::operation("engine_startup_failed", message, "none")
            },
            Self::SessionConfiguration(message) => ProtocolError::invalid_request(*message),
            Self::ClockConfiguration(error) => ProtocolError::operation(
                "engine_clock_configuration_failed",
                format!("{error:?}"),
                "none",
            ),
            Self::ControlProfileConfiguration(error) => ProtocolError::operation(
                "engine_control_profile_configuration_failed",
                format!("{error:?}"),
                "none",
            ),
            Self::ExecutionProfileConfiguration(error) => ProtocolError::operation(
                "engine_execution_profile_configuration_failed",
                format!("{error:?}"),
                "none",
            ),
            Self::ControlledNetworkConfiguration(error) => ProtocolError {
                code: "internal_runtime_failure",
                message: format!("controlled-network builder configuration failed: {error:?}"),
                fatal: true,
                state_effect: "none",
                details: None,
            },
            Self::NetworkFixture(error) => ProtocolError::invalid_request(format!(
                "invalid immutable controlled-network configuration: {error:?}"
            )),
            Self::UnpublishedInitialization(error) => ProtocolError {
                code: "session_state_initialization_failed",
                message: format!("unpublished session initialization failed: {error:?}"),
                fatal: true,
                state_effect: "none",
                details: None,
            },
            Self::SessionStateValidation(error) => ProtocolError {
                code: session_state_error_code(*error),
                message: "session state configuration was rejected before engine construction"
                    .into(),
                fatal: false,
                state_effect: "none",
                details: None,
            },
            Self::SessionStateInitialization(error) => ProtocolError {
                code: session_state_error_code(*error),
                message: "session state initialization was rejected".into(),
                fatal: true,
                state_effect: "none",
                details: None,
            },
            Self::ControlSubmission(error) => ProtocolError::operation(
                "engine_control_submission_failed",
                format!("{error:?}"),
                "none",
            ),
            Self::SessionNavigation(error) => ProtocolError::operation(
                "engine_session_navigation_failed",
                format!("{error:?}"),
                "none",
            ),
            Self::ControlAlreadyPending => ProtocolError::operation(
                "engine_control_busy",
                "another engine control operation is already pending",
                "none",
            ),
            Self::ControlDeadlineOverflow => ProtocolError::operation(
                "engine_control_deadline_overflow",
                "engine control wall deadline overflowed",
                "none",
            ),
            Self::BlockingHelperUnavailableInControlledMode(operation) => ProtocolError::operation(
                "engine_helper_requires_realtime",
                format!("blocking {operation} cannot bypass controlled document turns"),
                "none",
            ),
            Self::TopLevelSessionRequired => ProtocolError::operation(
                "engine_top_level_session_required",
                "explicit navigation requires the top-level-session control profile",
                "none",
            ),
            Self::WallTimeLimit => ProtocolError::operation(
                "wall_time_limit_exceeded",
                "engine operation exceeded its wall-time safety limit",
                "indeterminate",
            ),
            Self::Evaluation(error) => {
                ProtocolError::operation("evaluation_failed", format!("{error:?}"), "partial")
            },
        }
    }
}

fn session_state_error_code(error: SessionStateError) -> &'static str {
    match error {
        SessionStateError::InvalidCookie => "invalid_controlled_cookie",
        _ => error.code(),
    }
}

fn js_value_to_json(value: JSValue) -> Value {
    match value {
        JSValue::Undefined => json!({"kind": "undefined"}),
        JSValue::Null => json!({"kind": "null"}),
        JSValue::Boolean(value) => json!({"kind": "boolean", "value": value}),
        JSValue::Number(value) => Number::from_f64(value)
            .map(|value| json!({"kind": "number", "value": value}))
            .unwrap_or_else(|| json!({"kind": "non_finite_number", "value": value.to_string()})),
        JSValue::String(value) => json!({"kind": "string", "value": value}),
        JSValue::Element(reference) => json!({"kind": "element", "reference": reference}),
        JSValue::ShadowRoot(reference) => {
            json!({"kind": "shadow_root", "reference": reference})
        },
        JSValue::Frame(reference) => json!({"kind": "frame", "reference": reference}),
        JSValue::Window(reference) => json!({"kind": "window", "reference": reference}),
        JSValue::Array(values) => json!({
            "kind": "array",
            "value": values.into_iter().map(js_value_to_json).collect::<Vec<_>>(),
        }),
        JSValue::Object(values) => {
            let values = values
                .into_iter()
                .map(|(key, value)| (key, js_value_to_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect::<Map<_, _>>();
            json!({"kind": "object", "value": values})
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use super::*;

    #[test]
    fn javascript_objects_cannot_collide_with_value_tags() {
        let mut object = HashMap::new();
        object.insert("kind".into(), JSValue::String("undefined".into()));

        let value = js_value_to_json(JSValue::Object(object));

        assert_eq!(value["kind"], "object");
        assert_eq!(value["value"]["kind"]["kind"], "string");
        assert_eq!(value["value"]["kind"]["value"], "undefined");
    }

    #[test]
    fn javascript_object_keys_are_emitted_in_stable_order() {
        let mut object = HashMap::new();
        object.insert("z".into(), JSValue::Null);
        object.insert("a".into(), JSValue::Null);

        let value = js_value_to_json(JSValue::Object(object));
        let keys = value["value"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();

        assert_eq!(keys, ["a", "z"]);
    }

    #[test]
    fn real_mode_preserves_realtime_and_automatic_painting() {
        let mode = EngineClockMode::Real;

        assert_eq!(mode.document_clock(), DocumentClockConfiguration::Realtime);
        assert!(mode.paints_frames_automatically());
        assert!(!mode.is_controlled());
    }

    #[test]
    fn controlled_mode_uses_a_deterministic_unix_origin_and_no_automatic_paint() {
        let mode = EngineClockMode::Controlled {
            initial_time_ns: 42,
        };

        let DocumentClockConfiguration::Controlled {
            initial_time_ns,
            unix_time_origin_ns,
        } = mode.document_clock()
        else {
            panic!("controlled engine mode selected a realtime document clock");
        };
        assert_eq!(initial_time_ns, 42);
        assert_eq!(unix_time_origin_ns.as_nanos(), 0);
        assert!(!mode.paints_frames_automatically());
        assert!(mode.is_controlled());
    }

    #[test]
    fn controlled_runtime_has_one_exact_disabled_preference_inventory() {
        assert_eq!(
            CONTROLLED_DISABLED_BOOLEAN_PREFERENCES.len(),
            CONTROLLED_DISABLED_BOOLEAN_PREFERENCE_COUNT,
        );
        assert_eq!(CONTROLLED_DISABLED_BOOLEAN_PREFERENCE_COUNT, 75);

        let unique = CONTROLLED_DISABLED_BOOLEAN_PREFERENCES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            unique.len(),
            CONTROLLED_DISABLED_BOOLEAN_PREFERENCE_COUNT,
            "the frozen Controlled policy must not contain duplicate fields",
        );

        let preferences = controlled_preferences(DocumentControlProfile::SingleDocument);
        let enabled = CONTROLLED_DISABLED_BOOLEAN_PREFERENCES
            .iter()
            .copied()
            .zip(controlled_disabled_boolean_preference_values(&preferences))
            .filter_map(|(name, value)| value.then_some(name))
            .collect::<Vec<_>>();
        assert!(
            enabled.is_empty(),
            "Controlled left excluded preferences enabled: {enabled:?}",
        );
    }

    #[test]
    fn controlled_policy_resists_upstream_defaults_drifting_true() {
        let mut simulated_future_defaults = Preferences::default();
        set_controlled_disabled_boolean_preferences(&mut simulated_future_defaults, true);

        apply_controlled_preference_policy(&mut simulated_future_defaults);

        assert!(
            controlled_disabled_boolean_preference_values(&simulated_future_defaults)
                .into_iter()
                .all(|value| !value),
            "every governed preference must be overwritten, not inherited",
        );
    }

    #[test]
    fn clock_policy_preserves_real_and_unmanaged_controlled_defaults() {
        assert!(
            preferences_for_clock_mode(
                EngineClockMode::Real,
                DocumentControlProfile::SingleDocument,
            )
            .is_none(),
        );

        let defaults = Preferences::default();
        let controlled = preferences_for_clock_mode(
            EngineClockMode::Controlled { initial_time_ns: 0 },
            DocumentControlProfile::SingleDocument,
        )
        .expect("Controlled must install its frozen preferences before Servo starts");

        assert!(!controlled.dom_cookiestore_enabled);
        assert!(
            preferences_for_clock_mode(
                EngineClockMode::Controlled { initial_time_ns: 0 },
                DocumentControlProfile::TopLevelSession,
            )
            .expect("v0.2 must install its profile-specific preferences before Servo starts")
            .dom_cookiestore_enabled,
        );

        assert_eq!(
            controlled.dom_abort_controller_enabled,
            defaults.dom_abort_controller_enabled,
        );
        assert_eq!(
            controlled.dom_canvas_text_enabled,
            defaults.dom_canvas_text_enabled,
        );
        assert_eq!(
            controlled.dom_mutation_observer_enabled,
            defaults.dom_mutation_observer_enabled,
        );
        assert_eq!(
            controlled.dom_uievent_which_enabled,
            defaults.dom_uievent_which_enabled,
        );
        assert_eq!(
            controlled.dom_visual_viewport_enabled,
            defaults.dom_visual_viewport_enabled,
        );
        assert_eq!(controlled.js_wasm_enabled, defaults.js_wasm_enabled);
        assert_eq!(controlled.layout_grid_enabled, defaults.layout_grid_enabled);
        assert_eq!(
            controlled.network_http_cache_disabled,
            defaults.network_http_cache_disabled,
        );
    }

    #[test]
    fn a_blocking_helper_cannot_claim_no_effect_after_bypassing_controlled_turns() {
        let error =
            EngineError::BlockingHelperUnavailableInControlledMode("evaluate").to_protocol_error();

        assert_eq!(error.code, "engine_helper_requires_realtime");
        assert_eq!(error.state_effect, "none");
    }
}
