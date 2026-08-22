/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Process stdout ownership for the persistent protocol.
//!
//! The protocol writer keeps a duplicate of the stdout descriptor inherited from the client.
//! Descriptor 1 itself is then redirected to stderr before Servo is started, so an accidental
//! native `printf` cannot become a syntactically valid-looking NDJSON frame.

use std::fs::File;
use std::io;

#[cfg(unix)]
mod platform {
    use std::io::Write;
    #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
    use std::os::fd::AsRawFd;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::*;

    const STDOUT_FILENO: RawFd = 1;
    const STDERR_FILENO: RawFd = 2;
    const F_GETFD: i32 = 1;
    #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
    const F_DUPFD: i32 = 0;
    #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
    const F_SETFD: i32 = 2;
    #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
    const FD_CLOEXEC: i32 = 1;
    const MIN_PRIVATE_FD: i32 = 3;
    #[cfg(any(target_os = "android", target_os = "linux"))]
    const F_DUPFD_CLOEXEC: i32 = 1030;
    #[cfg(target_vendor = "apple")]
    const F_DUPFD_CLOEXEC: i32 = 67;

    // 0 = unclaimed, 1 = claim in progress, 2 = successfully claimed. A process has exactly one
    // inherited stdout authority, so silently claiming it twice would route protocol frames into
    // the diagnostic stream.
    static CLAIM_STATE: AtomicU8 = AtomicU8::new(0);

    unsafe extern "C" {
        fn dup2(source: i32, destination: i32) -> i32;
        fn fcntl(file_descriptor: i32, command: i32, ...) -> i32;
    }

    pub(super) fn claim_protocol_stdout() -> io::Result<File> {
        CLAIM_STATE
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "protocol stdout has already been claimed",
                )
            })?;

        let result = (|| {
            // Flush Rust's process-global handle before descriptor 1 changes meaning.
            io::stdout().lock().flush()?;
            let saved = save_and_redirect_fd(STDOUT_FILENO, STDERR_FILENO)?;
            Ok(File::from(saved))
        })();

        CLAIM_STATE.store(if result.is_ok() { 2 } else { 0 }, Ordering::Release);
        result
    }

    fn save_and_redirect_fd(target: RawFd, replacement: RawFd) -> io::Result<OwnedFd> {
        if target == replacement {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "protocol and diagnostic descriptors must be distinct",
            ));
        }
        // Validate the diagnostic destination before duplicating stdout. In particular, a closed
        // fd 2 must not become the descriptor returned for the protocol authority.
        // SAFETY: `fcntl(F_GETFD)` has no pointer arguments and does not mutate the descriptor.
        if unsafe { fcntl(replacement, F_GETFD) } == -1 {
            return Err(last_os_error("diagnostic descriptor is unavailable"));
        }
        let saved = duplicate_private_fd(target)?;

        // SAFETY: both arguments are live descriptors; `dup2` atomically replaces `target` and
        // leaves `saved` referring to the original open file description.
        if unsafe { dup2(replacement, target) } == -1 {
            return Err(last_os_error("cannot redirect stdout to diagnostics"));
        }
        Ok(saved)
    }

    fn duplicate_private_fd(source: RawFd) -> io::Result<OwnedFd> {
        #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
        // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` has no pointer arguments. Success creates a new owned
        // descriptor at or above 3 and sets close-on-exec atomically.
        let saved = unsafe { fcntl(source, F_DUPFD_CLOEXEC, MIN_PRIVATE_FD) };

        #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
        // SAFETY: `fcntl(F_DUPFD)` has no pointer arguments. Success creates a new owned
        // descriptor at or above 3. This fallback is used only on other Unix targets.
        let saved = unsafe { fcntl(source, F_DUPFD, MIN_PRIVATE_FD) };

        if saved == -1 {
            return Err(last_os_error("cannot duplicate protocol stdout"));
        }
        // SAFETY: `saved` was freshly returned by `fcntl` and has exactly one owner.
        let saved = unsafe { OwnedFd::from_raw_fd(saved) };

        #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
        // SAFETY: `saved` is live and `fcntl(F_SETFD, FD_CLOEXEC)` has no pointer arguments.
        if unsafe { fcntl(saved.as_raw_fd(), F_SETFD, FD_CLOEXEC) } == -1 {
            return Err(last_os_error("cannot mark protocol stdout close-on-exec"));
        }

        Ok(saved)
    }

    fn last_os_error(context: &'static str) -> io::Error {
        let error = io::Error::last_os_error();
        io::Error::new(error.kind(), format!("{context}: {error}"))
    }

    #[cfg(test)]
    mod tests {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        use super::*;

        #[test]
        fn saved_descriptor_and_redirected_descriptor_have_distinct_destinations() {
            let (mut original_reader, original_writer) = UnixStream::pair().unwrap();
            let (mut replacement_reader, replacement_writer) = UnixStream::pair().unwrap();
            original_reader
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            replacement_reader
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut saved = File::from(
                save_and_redirect_fd(original_writer.as_raw_fd(), replacement_writer.as_raw_fd())
                    .unwrap(),
            );

            saved.write_all(b"protocol").unwrap();
            (&original_writer).write_all(b"diagnostic").unwrap();

            let mut protocol = [0; 8];
            original_reader.read_exact(&mut protocol).unwrap();
            assert_eq!(&protocol, b"protocol");
            let mut diagnostic = [0; 10];
            replacement_reader.read_exact(&mut diagnostic).unwrap();
            assert_eq!(&diagnostic, b"diagnostic");
        }

        #[test]
        fn an_invalid_diagnostic_descriptor_does_not_replace_the_target() {
            let (mut original_reader, original_writer) = UnixStream::pair().unwrap();
            original_reader
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();

            assert!(save_and_redirect_fd(original_writer.as_raw_fd(), -1).is_err());
            (&original_writer).write_all(b"still-original").unwrap();
            let mut bytes = [0; 14];
            original_reader.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"still-original");
        }
    }
}

/// Claim the stdout pipe inherited from the client for protocol frames and redirect ordinary
/// descriptor-1 output to stderr. Call exactly once, before Servo or any native worker threads are
/// started, and pass the returned file to `ProtocolWriter`.
#[cfg(unix)]
pub fn claim_protocol_stdout() -> io::Result<File> {
    platform::claim_protocol_stdout()
}

#[cfg(not(unix))]
pub fn claim_protocol_stdout() -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "saved protocol stdout is not implemented on this platform",
    ))
}
