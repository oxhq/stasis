/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Once;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use serde_json::{Map, Number, Value, json};
use servo::document_control::{DocumentControlCommand, DocumentControlError};
use servo::{
    DocumentClockConfiguration, DocumentClockError, JSValue, JavaScriptEvaluationError, LoadStatus,
    Preferences, RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext, WebView,
    WebViewBuilder, WebViewDelegate,
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
// default change must not silently widen the Controlled alpha. Unconditional APIs are rejected at
// their owner boundaries instead. The exact count is asserted in the tests below.
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
    // Background producers which are not represented in the alpha pending-state proof.
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

fn controlled_preferences() -> Preferences {
    let mut preferences = Preferences::default();
    apply_controlled_preference_policy(&mut preferences);
    preferences
}

fn preferences_for_clock_mode(clock_mode: EngineClockMode) -> Option<Preferences> {
    match clock_mode {
        // Leaving the builder unset preserves ServoBuilder's ordinary default behavior exactly.
        EngineClockMode::Real => None,
        EngineClockMode::Controlled { .. } => Some(controlled_preferences()),
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

pub struct EngineSession {
    // Drop the WebView before Servo so its close message still has an owner.
    webview: Option<WebView>,
    servo: Option<Servo>,
    _rendering_context: Rc<dyn RenderingContext>,
    waker: ShellWaker,
    clock_mode: EngineClockMode,
    pending_control: Option<PendingControlOperation>,
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
        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(PhysicalSize::new(1024, 768))
                .map_err(|error| EngineError::Startup(format!("rendering context: {error:?}")))?,
        );
        rendering_context
            .make_current()
            .map_err(|error| EngineError::Startup(format!("make current: {error:?}")))?;

        let servo_builder = ServoBuilder::default().event_loop_waker(Box::new(waker.clone()));
        let servo_builder = match preferences_for_clock_mode(clock_mode) {
            Some(preferences) => servo_builder.preferences(preferences),
            None => servo_builder,
        };
        let servo = servo_builder.build();

        let delegate = Rc::new(ShellDelegate {
            paints_frames_automatically: clock_mode.paints_frames_automatically(),
        });
        let webview = WebViewBuilder::new(&servo, rendering_context.clone())
            .url(url)
            .delegate(delegate)
            .document_clock(clock_mode.document_clock())
            .map_err(EngineError::ClockConfiguration)?
            .build();

        let session = Self {
            webview: Some(webview),
            servo: Some(servo),
            _rendering_context: rendering_context,
            waker,
            clock_mode,
            pending_control: None,
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

    pub fn evaluate(&self, expression: &str) -> Result<Value, EngineError> {
        if self.clock_mode.is_controlled() {
            return Err(EngineError::BlockingHelperUnavailableInControlledMode(
                "evaluate",
            ));
        }
        if self.pending_control.is_some() {
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
        if self.pending_control.is_some() {
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
    ClockConfiguration(DocumentClockError),
    ControlSubmission(DocumentControlError),
    ControlAlreadyPending,
    ControlDeadlineOverflow,
    BlockingHelperUnavailableInControlledMode(&'static str),
    WallTimeLimit,
    Evaluation(JavaScriptEvaluationError),
}

impl EngineError {
    pub fn to_protocol_error(&self) -> ProtocolError {
        match self {
            Self::Startup(message) => {
                ProtocolError::operation("engine_startup_failed", message, "none")
            },
            Self::ClockConfiguration(error) => ProtocolError::operation(
                "engine_clock_configuration_failed",
                format!("{error:?}"),
                "none",
            ),
            Self::ControlSubmission(error) => ProtocolError::operation(
                "engine_control_submission_failed",
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
    fn controlled_alpha_has_one_exact_disabled_preference_inventory() {
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

        let preferences = controlled_preferences();
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
        assert!(preferences_for_clock_mode(EngineClockMode::Real).is_none());

        let defaults = Preferences::default();
        let controlled =
            preferences_for_clock_mode(EngineClockMode::Controlled { initial_time_ns: 0 })
                .expect("Controlled must install its frozen preferences before Servo starts");

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
