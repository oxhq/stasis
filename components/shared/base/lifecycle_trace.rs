/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Opt-in, fixed-vocabulary lifecycle evidence for Stasis release qualification.
//!
//! The trace deliberately excludes values supplied by a document or host environment. Each
//! emitted line contains only a schema identifier and one compile-time phase name, so release
//! diagnostics can retain shutdown ordering without retaining browsing data.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::sync::OnceLock;

const LIFECYCLE_TRACE_ENV: &str = "STASIS_LIFECYCLE_TRACE_V1";
static LIFECYCLE_TRACE_ENABLED: OnceLock<bool> = OnceLock::new();

/// A fixed phase in the normal Stasis browser-session shutdown lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePhase {
    CloseAccepted,
    EngineSessionDropBegin,
    EngineCloseBegin,
    WebViewDropBegin,
    PainterDropBegin,
    PainterWebRenderShutdownBegin,
    PainterWebRenderShutdownAckObserved,
    PainterWebRenderShutdownFailed,
    PainterWebRenderThreadsJoinBegin,
    PainterWebRenderThreadsJoinEnd,
    PainterWebRenderThreadsJoinFailed,
    PainterWebRenderWorkersJoinBegin,
    PainterWebRenderWorkersJoinEnd,
    PainterWebRenderWorkersJoinFailed,
    PainterRendererDeinitBegin,
    PainterRendererDeinitEnd,
    PainterRendererDeinitFailed,
    PainterDropBodyEnd,
    WebViewDropEnd,
    PreShutdownSpinBegin,
    PreShutdownSpinEnd,
    ServoOwnerDropBegin,
    ServoInnerDropBegin,
    ConstellationExitSendBegin,
    ScriptThreadsJoinBegin,
    ScriptThreadsJoinEnd,
    ScriptThreadsJoinFailed,
    StyleThreadPoolShutdownBegin,
    StyleThreadPoolShutdownEnd,
    StyleThreadPoolShutdownFailed,
    FetchThreadJoinBegin,
    FetchThreadJoinEnd,
    FetchThreadJoinFailed,
    CanvasPaintThreadJoinBegin,
    CanvasPaintThreadJoinEnd,
    CanvasPaintThreadJoinFailed,
    SystemFontServiceJoinBegin,
    SystemFontServiceJoinEnd,
    SystemFontServiceJoinFailed,
    ResourceManagerJoinBegin,
    ResourceManagerJoinEnd,
    ResourceManagerJoinFailed,
    StorageThreadsJoinBegin,
    StorageThreadsJoinEnd,
    StorageThreadsJoinFailed,
    GlobalThreadPoolShutdownBegin,
    GlobalThreadPoolShutdownEnd,
    GlobalThreadPoolShutdownFailed,
    AsyncRuntimeShutdownBegin,
    AsyncRuntimeShutdownEnd,
    AsyncRuntimeShutdownFailed,
    SubsystemsShutdownEnd,
    SubsystemsShutdownFailed,
    ConstellationRunEnd,
    ConstellationStateDropBegin,
    ConstellationStateDropEnd,
    ShutdownCompleteSendBegin,
    ShutdownCompleteObserved,
    ConstellationJoinBegin,
    ConstellationJoinEnd,
    ConstellationJoinFailed,
    TlsPrewarmJoinBegin,
    TlsPrewarmJoinEnd,
    TlsPrewarmJoinFailed,
    ServoInnerDropBodyEnd,
    MemoryProfilerExitSendBegin,
    MemoryProfilerJoinBegin,
    MemoryProfilerJoinEnd,
    MemoryProfilerJoinFailed,
    JsEngineDropBegin,
    JsEngineDropEnd,
    JsEngineDropFailed,
    ServoOwnerDropEnd,
    EngineCloseEnd,
    EngineSessionDropEnd,
    RenderingContextOwnerDropBegin,
    SoftwareRenderingContextDropBegin,
    SoftwareRenderingContextDropBodyEnd,
    SurfmanRenderingContextDropBegin,
    SurfmanRenderingContextDropBodyEnd,
    RenderingContextOwnerDropEnd,
    CloseResponseWritten,
    ShellRunEnd,
    ProtocolReaderJoinBegin,
    ProtocolReaderJoinEnd,
    ProtocolReaderJoinFailed,
    ShellDropBegin,
    ShellDropEnd,
    MainBodyEnd,
}

impl LifecyclePhase {
    /// All v1 phases in their stable vocabulary order.
    pub const ALL: &'static [Self] = &[
        Self::CloseAccepted,
        Self::EngineCloseBegin,
        Self::WebViewDropBegin,
        Self::PainterDropBegin,
        Self::PainterWebRenderShutdownBegin,
        Self::PainterWebRenderShutdownAckObserved,
        Self::PainterWebRenderShutdownFailed,
        Self::PainterWebRenderThreadsJoinBegin,
        Self::PainterWebRenderThreadsJoinEnd,
        Self::PainterWebRenderThreadsJoinFailed,
        Self::PainterWebRenderWorkersJoinBegin,
        Self::PainterWebRenderWorkersJoinEnd,
        Self::PainterWebRenderWorkersJoinFailed,
        Self::PainterRendererDeinitBegin,
        Self::PainterRendererDeinitEnd,
        Self::PainterRendererDeinitFailed,
        Self::PainterDropBodyEnd,
        Self::WebViewDropEnd,
        Self::PreShutdownSpinBegin,
        Self::PreShutdownSpinEnd,
        Self::ServoOwnerDropBegin,
        Self::ServoInnerDropBegin,
        Self::ConstellationExitSendBegin,
        Self::ScriptThreadsJoinBegin,
        Self::ScriptThreadsJoinEnd,
        Self::ScriptThreadsJoinFailed,
        Self::StyleThreadPoolShutdownBegin,
        Self::StyleThreadPoolShutdownEnd,
        Self::StyleThreadPoolShutdownFailed,
        Self::FetchThreadJoinBegin,
        Self::FetchThreadJoinEnd,
        Self::FetchThreadJoinFailed,
        Self::CanvasPaintThreadJoinBegin,
        Self::CanvasPaintThreadJoinEnd,
        Self::CanvasPaintThreadJoinFailed,
        Self::ResourceManagerJoinBegin,
        Self::ResourceManagerJoinEnd,
        Self::ResourceManagerJoinFailed,
        Self::StorageThreadsJoinBegin,
        Self::StorageThreadsJoinEnd,
        Self::StorageThreadsJoinFailed,
        Self::GlobalThreadPoolShutdownBegin,
        Self::GlobalThreadPoolShutdownEnd,
        Self::GlobalThreadPoolShutdownFailed,
        Self::SystemFontServiceJoinBegin,
        Self::SystemFontServiceJoinEnd,
        Self::SystemFontServiceJoinFailed,
        Self::AsyncRuntimeShutdownBegin,
        Self::AsyncRuntimeShutdownEnd,
        Self::AsyncRuntimeShutdownFailed,
        Self::SubsystemsShutdownEnd,
        Self::SubsystemsShutdownFailed,
        Self::ConstellationRunEnd,
        Self::ConstellationStateDropBegin,
        Self::ConstellationStateDropEnd,
        Self::ShutdownCompleteSendBegin,
        Self::ShutdownCompleteObserved,
        Self::ConstellationJoinBegin,
        Self::ConstellationJoinEnd,
        Self::ConstellationJoinFailed,
        Self::TlsPrewarmJoinBegin,
        Self::TlsPrewarmJoinEnd,
        Self::TlsPrewarmJoinFailed,
        Self::ServoInnerDropBodyEnd,
        Self::MemoryProfilerExitSendBegin,
        Self::MemoryProfilerJoinBegin,
        Self::MemoryProfilerJoinEnd,
        Self::MemoryProfilerJoinFailed,
        Self::JsEngineDropBegin,
        Self::JsEngineDropEnd,
        Self::JsEngineDropFailed,
        Self::ServoOwnerDropEnd,
        Self::EngineCloseEnd,
        Self::EngineSessionDropBegin,
        Self::EngineSessionDropEnd,
        Self::RenderingContextOwnerDropBegin,
        Self::SoftwareRenderingContextDropBegin,
        Self::SoftwareRenderingContextDropBodyEnd,
        Self::SurfmanRenderingContextDropBegin,
        Self::SurfmanRenderingContextDropBodyEnd,
        Self::RenderingContextOwnerDropEnd,
        Self::CloseResponseWritten,
        Self::ShellRunEnd,
        Self::ProtocolReaderJoinBegin,
        Self::ProtocolReaderJoinEnd,
        Self::ProtocolReaderJoinFailed,
        Self::ShellDropBegin,
        Self::ShellDropEnd,
        Self::MainBodyEnd,
    ];

    /// Return the stable ASCII wire name used by the release diagnostic sanitizer.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CloseAccepted => "close_accepted",
            Self::EngineSessionDropBegin => "engine_session_drop_begin",
            Self::EngineCloseBegin => "engine_close_begin",
            Self::WebViewDropBegin => "webview_drop_begin",
            Self::PainterDropBegin => "painter_drop_begin",
            Self::PainterWebRenderShutdownBegin => "painter_webrender_shutdown_begin",
            Self::PainterWebRenderShutdownAckObserved => "painter_webrender_shutdown_ack_observed",
            Self::PainterWebRenderShutdownFailed => "painter_webrender_shutdown_failed",
            Self::PainterWebRenderThreadsJoinBegin => "painter_webrender_threads_join_begin",
            Self::PainterWebRenderThreadsJoinEnd => "painter_webrender_threads_join_end",
            Self::PainterWebRenderThreadsJoinFailed => "painter_webrender_threads_join_failed",
            Self::PainterWebRenderWorkersJoinBegin => "painter_webrender_workers_join_begin",
            Self::PainterWebRenderWorkersJoinEnd => "painter_webrender_workers_join_end",
            Self::PainterWebRenderWorkersJoinFailed => "painter_webrender_workers_join_failed",
            Self::PainterRendererDeinitBegin => "painter_renderer_deinit_begin",
            Self::PainterRendererDeinitEnd => "painter_renderer_deinit_end",
            Self::PainterRendererDeinitFailed => "painter_renderer_deinit_failed",
            Self::PainterDropBodyEnd => "painter_drop_body_end",
            Self::WebViewDropEnd => "webview_drop_end",
            Self::PreShutdownSpinBegin => "pre_shutdown_spin_begin",
            Self::PreShutdownSpinEnd => "pre_shutdown_spin_end",
            Self::ServoOwnerDropBegin => "servo_owner_drop_begin",
            Self::ServoInnerDropBegin => "servo_inner_drop_begin",
            Self::ConstellationExitSendBegin => "constellation_exit_send_begin",
            Self::ScriptThreadsJoinBegin => "script_threads_join_begin",
            Self::ScriptThreadsJoinEnd => "script_threads_join_end",
            Self::ScriptThreadsJoinFailed => "script_threads_join_failed",
            Self::StyleThreadPoolShutdownBegin => "style_thread_pool_shutdown_begin",
            Self::StyleThreadPoolShutdownEnd => "style_thread_pool_shutdown_end",
            Self::StyleThreadPoolShutdownFailed => "style_thread_pool_shutdown_failed",
            Self::FetchThreadJoinBegin => "fetch_thread_join_begin",
            Self::FetchThreadJoinEnd => "fetch_thread_join_end",
            Self::FetchThreadJoinFailed => "fetch_thread_join_failed",
            Self::CanvasPaintThreadJoinBegin => "canvas_paint_thread_join_begin",
            Self::CanvasPaintThreadJoinEnd => "canvas_paint_thread_join_end",
            Self::CanvasPaintThreadJoinFailed => "canvas_paint_thread_join_failed",
            Self::SubsystemsShutdownEnd => "subsystems_shutdown_end",
            Self::SubsystemsShutdownFailed => "subsystems_shutdown_failed",
            Self::ConstellationRunEnd => "constellation_run_end",
            Self::ConstellationStateDropBegin => "constellation_state_drop_begin",
            Self::ConstellationStateDropEnd => "constellation_state_drop_end",
            Self::ShutdownCompleteSendBegin => "shutdown_complete_send_begin",
            Self::ShutdownCompleteObserved => "shutdown_complete_observed",
            Self::ConstellationJoinBegin => "constellation_join_begin",
            Self::ConstellationJoinEnd => "constellation_join_end",
            Self::ConstellationJoinFailed => "constellation_join_failed",
            Self::SystemFontServiceJoinBegin => "system_font_service_join_begin",
            Self::SystemFontServiceJoinEnd => "system_font_service_join_end",
            Self::SystemFontServiceJoinFailed => "system_font_service_join_failed",
            Self::ResourceManagerJoinBegin => "resource_manager_join_begin",
            Self::ResourceManagerJoinEnd => "resource_manager_join_end",
            Self::ResourceManagerJoinFailed => "resource_manager_join_failed",
            Self::StorageThreadsJoinBegin => "storage_threads_join_begin",
            Self::StorageThreadsJoinEnd => "storage_threads_join_end",
            Self::StorageThreadsJoinFailed => "storage_threads_join_failed",
            Self::GlobalThreadPoolShutdownBegin => "global_thread_pool_shutdown_begin",
            Self::GlobalThreadPoolShutdownEnd => "global_thread_pool_shutdown_end",
            Self::GlobalThreadPoolShutdownFailed => "global_thread_pool_shutdown_failed",
            Self::AsyncRuntimeShutdownBegin => "async_runtime_shutdown_begin",
            Self::AsyncRuntimeShutdownEnd => "async_runtime_shutdown_end",
            Self::AsyncRuntimeShutdownFailed => "async_runtime_shutdown_failed",
            Self::TlsPrewarmJoinBegin => "tls_prewarm_join_begin",
            Self::TlsPrewarmJoinEnd => "tls_prewarm_join_end",
            Self::TlsPrewarmJoinFailed => "tls_prewarm_join_failed",
            Self::MemoryProfilerExitSendBegin => "memory_profiler_exit_send_begin",
            Self::MemoryProfilerJoinBegin => "memory_profiler_join_begin",
            Self::MemoryProfilerJoinEnd => "memory_profiler_join_end",
            Self::MemoryProfilerJoinFailed => "memory_profiler_join_failed",
            Self::ServoInnerDropBodyEnd => "servo_inner_drop_body_end",
            Self::JsEngineDropBegin => "js_engine_drop_begin",
            Self::JsEngineDropEnd => "js_engine_drop_end",
            Self::JsEngineDropFailed => "js_engine_drop_failed",
            Self::ServoOwnerDropEnd => "servo_owner_drop_end",
            Self::EngineCloseEnd => "engine_close_end",
            Self::EngineSessionDropEnd => "engine_session_drop_end",
            Self::RenderingContextOwnerDropBegin => "rendering_context_owner_drop_begin",
            Self::SoftwareRenderingContextDropBegin => "software_rendering_context_drop_begin",
            Self::SoftwareRenderingContextDropBodyEnd => "software_rendering_context_drop_body_end",
            Self::SurfmanRenderingContextDropBegin => "surfman_rendering_context_drop_begin",
            Self::SurfmanRenderingContextDropBodyEnd => "surfman_rendering_context_drop_body_end",
            Self::RenderingContextOwnerDropEnd => "rendering_context_owner_drop_end",
            Self::CloseResponseWritten => "close_response_written",
            Self::ShellRunEnd => "shell_run_end",
            Self::ProtocolReaderJoinBegin => "protocol_reader_join_begin",
            Self::ProtocolReaderJoinEnd => "protocol_reader_join_end",
            Self::ProtocolReaderJoinFailed => "protocol_reader_join_failed",
            Self::ShellDropBegin => "shell_drop_begin",
            Self::ShellDropEnd => "shell_drop_end",
            Self::MainBodyEnd => "main_body_end",
        }
    }
}

/// Cache exact trace enablement before Stasis starts any runtime-owned threads.
pub fn initialize_lifecycle_trace() {
    let _ = lifecycle_trace_enabled();
}

/// Emit one fixed lifecycle phase when the v1 diagnostic trace is explicitly enabled.
pub fn emit_lifecycle_phase(phase: LifecyclePhase) {
    if !lifecycle_trace_enabled() {
        return;
    }

    let _ = writeln!(
        io::stderr().lock(),
        "stasis_lifecycle_v1 phase={}",
        phase.as_str()
    );
}

fn lifecycle_trace_enabled() -> bool {
    *LIFECYCLE_TRACE_ENABLED
        .get_or_init(|| lifecycle_trace_requested(std::env::var_os(LIFECYCLE_TRACE_ENV).as_deref()))
}

fn lifecycle_trace_requested(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsStr;

    use super::{LifecyclePhase, lifecycle_trace_requested};

    #[test]
    fn lifecycle_phase_wire_names_are_unique_fixed_ascii_tokens() {
        let names: BTreeSet<_> = LifecyclePhase::ALL
            .iter()
            .map(|phase| phase.as_str())
            .collect();

        assert_eq!(names.len(), LifecyclePhase::ALL.len());
        assert!(names.iter().all(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }));
    }

    #[test]
    fn lifecycle_trace_enablement_requires_the_exact_v1_value() {
        assert!(lifecycle_trace_requested(Some(OsStr::new("1"))));
        for value in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("0")),
            Some(OsStr::new("true")),
        ] {
            assert!(!lifecycle_trace_requested(value));
        }
    }
}
