/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use serde_json::{Map, Number, Value, json};
use servo::{
    JSValue, JavaScriptEvaluationError, LoadStatus, RenderingContext, Servo, ServoBuilder,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
};
use url::Url;

use crate::protocol::ProtocolError;
use crate::wake::{ShellWaker, WaitError};

const DEFAULT_WALL_TIMEOUT: Duration = Duration::from_secs(30);

struct ShellDelegate;

impl WebViewDelegate for ShellDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
    }
}

pub struct EngineSession {
    // Drop the WebView before Servo so its close message still has an owner.
    webview: Option<WebView>,
    servo: Option<Servo>,
    _rendering_context: Rc<dyn RenderingContext>,
    waker: ShellWaker,
}

impl EngineSession {
    pub fn open(url: Url, waker: ShellWaker) -> Result<Self, EngineError> {
        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(PhysicalSize::new(1024, 768))
                .map_err(|error| EngineError::Startup(format!("rendering context: {error:?}")))?,
        );
        rendering_context
            .make_current()
            .map_err(|error| EngineError::Startup(format!("make current: {error:?}")))?;

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker.clone()))
            .build();

        let delegate = Rc::new(ShellDelegate);
        let webview = WebViewBuilder::new(&servo, rendering_context.clone())
            .url(url)
            .delegate(delegate.clone())
            .build();

        let session = Self {
            webview: Some(webview),
            servo: Some(servo),
            _rendering_context: rendering_context,
            waker,
        };
        session.wait_for_load(DEFAULT_WALL_TIMEOUT)?;
        session.servo().setup_logging();
        Ok(session)
    }

    pub fn pump(&self) {
        self.servo().spin_event_loop();
    }

    pub fn url(&self) -> Option<Url> {
        self.webview().url()
    }

    pub fn evaluate(&self, expression: &str) -> Result<Value, EngineError> {
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
        self.webview.take();
        if let Some(servo) = self.servo.as_ref() {
            servo.spin_event_loop();
        }
        self.servo.take();
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
    WallTimeLimit,
    Evaluation(JavaScriptEvaluationError),
}

impl EngineError {
    pub fn to_protocol_error(&self) -> ProtocolError {
        match self {
            Self::Startup(message) => {
                ProtocolError::operation("engine_startup_failed", message, "none")
            },
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
    use std::collections::HashMap;

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
}
