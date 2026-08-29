/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Memory profiling functions.

use std::borrow::ToOwned;
use std::collections::HashMap;
use std::fs::File;
use std::thread;

use log::debug;
use profile_traits::mem::{
    MemoryReport, MemoryReportResult, ProfilerChan, ProfilerMsg, Report, Reporter, ReporterRequest,
    ReportsChan,
};
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};

use crate::system_reporter;

const LOG_FILE_VAR: &str = "UNTRACKED_LOG_FILE";

/// Physical ownership of the memory-profiler thread.
///
/// Keeping the channel alone does not prove that the profiler thread and its reporter map have
/// finished dropping. The embedding runtime retains this owner until every reporter-owning helper
/// has terminated, then sends [`ProfilerMsg::Exit`] and joins the thread.
#[must_use = "dropping the memory profiler thread owner detaches the profiler thread"]
pub struct MemoryProfilerThreadOwner {
    join_handle: thread::JoinHandle<()>,
}

impl MemoryProfilerThreadOwner {
    fn new(join_handle: thread::JoinHandle<()>) -> Self {
        Self { join_handle }
    }

    /// Wait for the profiler thread and all of its thread-owned state to finish dropping.
    pub fn join(self) -> thread::Result<()> {
        self.join_handle.join()
    }
}

pub struct Profiler {
    /// The port through which messages are received.
    pub port: GenericReceiver<ProfilerMsg>,

    /// Registered memory reporters.
    reporters: HashMap<String, Reporter>,
}

impl Profiler {
    pub fn create() -> ProfilerChan {
        Self::create_owned().0
    }

    /// Create the memory profiler while retaining authority to join its physical termination.
    pub fn create_owned() -> (ProfilerChan, MemoryProfilerThreadOwner) {
        let (chan, port) = generic_channel::channel().unwrap();

        if servo_allocator::is_tracking_unmeasured() && std::env::var(LOG_FILE_VAR).is_err() {
            eprintln!("Allocation tracking is enabled but {LOG_FILE_VAR} is unset.");
        }

        // Always spawn the memory profiler. If there is no timer thread it won't receive regular
        // `Print` events, but it will still receive the other events.
        let join_handle = thread::Builder::new()
            .name("MemoryProfiler".to_owned())
            .spawn(move || {
                let mut mem_profiler = Profiler::new(port);
                mem_profiler.start();
            })
            .expect("Thread spawning failed");

        let mem_profiler_chan = ProfilerChan(chan);

        // Register the system memory reporter, which will run on its own thread. It never needs to
        // be unregistered, because as long as the memory profiler is running the system memory
        // reporter can make measurements.
        let callback = GenericCallback::new(|message| {
            let request: ReporterRequest = message.unwrap();
            system_reporter::collect_reports(request)
        })
        .expect("Could not create system reporter callback");
        mem_profiler_chan.send(ProfilerMsg::RegisterReporter(
            "system-main".to_owned(),
            Reporter(callback),
        ));

        (
            mem_profiler_chan,
            MemoryProfilerThreadOwner::new(join_handle),
        )
    }

    pub fn new(port: GenericReceiver<ProfilerMsg>) -> Profiler {
        Profiler {
            port,
            reporters: HashMap::new(),
        }
    }

    pub fn start(&mut self) {
        while let Ok(msg) = self.port.recv() {
            if !self.handle_msg(msg) {
                break;
            }
        }
    }

    fn handle_msg(&mut self, msg: ProfilerMsg) -> bool {
        match msg {
            ProfilerMsg::RegisterReporter(name, reporter) => {
                debug!("Registering memory reporter: {}", name);
                // Panic if it has already been registered.
                let name_clone = name.clone();
                match self.reporters.insert(name, reporter) {
                    None => true,
                    Some(_) => panic!("RegisterReporter: '{}' name is already in use", name_clone),
                }
            },

            ProfilerMsg::UnregisterReporter(name) => {
                debug!("Unregistering memory reporter: {}", name);
                // Panic if it hasn't previously been registered.
                match self.reporters.remove(&name) {
                    Some(_) => true,
                    None => panic!("UnregisterReporter: '{}' name is unknown", name),
                }
            },

            ProfilerMsg::Report(sender) => {
                let main_pid = std::process::id();

                let reports = self.collect_reports();
                // Turn the pid -> reports map into a vector and add the
                // hint to find the main process.
                let results: Vec<MemoryReport> = reports
                    .into_iter()
                    .map(|(pid, reports)| MemoryReport {
                        pid,
                        reports,
                        is_main_process: pid == main_pid,
                    })
                    .collect();
                let _ = sender.send(MemoryReportResult { results });

                if let Ok(value) = std::env::var(LOG_FILE_VAR) {
                    match File::create(&value) {
                        Ok(file) => {
                            servo_allocator::dump_unmeasured(file);
                        },
                        Err(error) => {
                            log::error!("Error creating log file: {error:?}");
                        },
                    }
                }
                true
            },
            ProfilerMsg::Exit => false,
        }
    }

    /// Returns a map of pid -> reports
    fn collect_reports(&self) -> HashMap<u32, Vec<Report>> {
        let mut result = HashMap::new();

        for reporter in self.reporters.values() {
            let (chan, port) = generic_channel::channel().unwrap();
            reporter.collect_reports(ReportsChan(chan));
            if let Ok(mut reports) = port.recv() {
                result
                    .entry(reports.pid)
                    .or_insert(vec![])
                    .append(&mut reports.reports);
            }
        }
        result
    }
}

#[cfg(test)]
mod thread_ownership_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use profile_traits::mem::ProfilerMsg;

    use super::{MemoryProfilerThreadOwner, Profiler};

    struct DropSentinel(Arc<AtomicBool>);

    impl Drop for DropSentinel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn memory_profiler_owner_joins_the_real_profiler_after_exit() {
        let (chan, owner) = Profiler::create_owned();
        chan.send(ProfilerMsg::Exit);
        owner
            .join()
            .expect("the memory profiler should physically terminate after Exit");
    }

    #[test]
    fn memory_profiler_owner_join_fences_thread_owned_state() {
        let dropped = Arc::new(AtomicBool::new(false));
        let thread_dropped = dropped.clone();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let owner = MemoryProfilerThreadOwner::new(thread::spawn(move || {
            let _sentinel = DropSentinel(thread_dropped);
            ready_sender
                .send(())
                .expect("the deterministic readiness receiver should remain open");
            release_receiver
                .recv()
                .expect("the deterministic release sender should remain open");
        }));

        ready_receiver
            .recv()
            .expect("the deterministic profiler should report readiness");
        assert!(
            !dropped.load(Ordering::SeqCst),
            "a live profiler thread must still own its state"
        );
        release_sender
            .send(())
            .expect("the deterministic profiler should remain blocked until release");
        owner
            .join()
            .expect("the deterministic profiler thread should exit cleanly");
        assert!(
            dropped.load(Ordering::SeqCst),
            "joining must fence profiler-owned state drop"
        );
    }
}
