/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Process stdout ownership for the persistent protocol.
//!
//! The protocol writer keeps a private duplicate of the stdout authority inherited from the
//! client. Ordinary stdout is then redirected away from that authority before Servo is started,
//! so an accidental native `printf` cannot become a syntactically valid-looking NDJSON frame.

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

#[cfg(windows)]
mod platform {
    use std::io::Write;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;
    use std::sync::atomic::{AtomicU8, Ordering};

    use windows_sys::Win32::Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, GetHandleInformation, HANDLE,
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::*;

    const STDOUT_FILENO: i32 = 1;
    const STDERR_FILENO: i32 = 2;

    // 0 = unclaimed, 1 = claim in progress, 2 = claimed or permanently poisoned. A failed claim
    // may have changed one of the process-global stdout slots, so retrying must never treat that
    // partial state as the inherited protocol authority.
    static CLAIM_STATE: AtomicU8 = AtomicU8::new(0);

    unsafe extern "C" {
        #[link_name = "_errno"]
        fn crt_errno() -> *mut i32;
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
            io::stdout().lock().flush()?;

            // The Win32 standard-handle table and the CRT descriptor table are independent.
            // Require their inherited views to agree before changing either one; this also means
            // _dup2 will close the obsolete original stdout handle instead of leaking it.
            let stdout = matched_standard_handle(
                STD_OUTPUT_HANDLE,
                STDOUT_FILENO,
                "protocol stdout is unavailable",
            )?;
            let stderr = matched_standard_handle(
                STD_ERROR_HANDLE,
                STDERR_FILENO,
                "diagnostic stderr is unavailable",
            )?;
            if stdout == stderr {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "protocol stdout and diagnostic stderr handles must be distinct",
                ));
            }

            let saved = duplicate_private_handle(stdout)?;

            // SetStdHandle does not own or close either handle. The following _dup2 atomically
            // closes CRT fd 1 (the old stdout handle validated above) and duplicates fd 2 onto it.
            // The saved handle remains the sole protocol authority.
            redirect_win32_stdout(stderr)?;
            redirect_crt_descriptor(STDOUT_FILENO, STDERR_FILENO)?;
            Ok(File::from(saved))
        })();

        // A failure after SetStdHandle can leave a safe but partial redirection. Fail closed:
        // neither successful claims nor failed attempts may claim the inherited authority again.
        CLAIM_STATE.store(2, Ordering::Release);
        result
    }

    fn matched_standard_handle(
        kind: u32,
        descriptor: i32,
        context: &'static str,
    ) -> io::Result<HANDLE> {
        // SAFETY: kind is one of the documented standard-handle selectors.
        let win32 = unsafe { GetStdHandle(kind) };
        validate_handle(win32, context)?;

        let crt = crt_handle(descriptor, context)?;
        if crt != win32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{context}: Win32 and CRT handles do not match"),
            ));
        }
        Ok(win32)
    }

    fn duplicate_private_handle(source: HANDLE) -> io::Result<OwnedHandle> {
        validate_handle(source, "cannot duplicate protocol stdout")?;

        let process = unsafe { GetCurrentProcess() };
        let mut duplicate = ptr::null_mut();
        // SAFETY: source is live, both process arguments are the current process pseudo-handle,
        // and duplicate points to writable storage. FALSE makes the new handle non-inheritable.
        if unsafe {
            DuplicateHandle(
                process,
                source,
                process,
                &mut duplicate,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(last_os_error("cannot duplicate protocol stdout"));
        }
        if duplicate.is_null() || duplicate == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(
                "DuplicateHandle succeeded without a valid protocol stdout handle",
            ));
        }

        // SAFETY: DuplicateHandle returned a fresh handle with no other owner.
        let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate as RawHandle) };
        require_noninheritable(
            duplicate.as_raw_handle() as HANDLE,
            "duplicated protocol stdout remains inheritable",
        )?;
        Ok(duplicate)
    }

    fn redirect_win32_stdout(stderr: HANDLE) -> io::Result<()> {
        validate_handle(stderr, "diagnostic stderr is unavailable")?;
        // SAFETY: stderr remains owned by the process/CRT; SetStdHandle borrows the raw handle.
        if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, stderr) } == 0 {
            return Err(last_os_error(
                "cannot redirect Win32 stdout to diagnostic stderr",
            ));
        }
        // SAFETY: STD_OUTPUT_HANDLE is a documented selector.
        if unsafe { GetStdHandle(STD_OUTPUT_HANDLE) } != stderr {
            return Err(io::Error::other(
                "Win32 stdout redirection did not install diagnostic stderr",
            ));
        }
        Ok(())
    }

    fn redirect_crt_descriptor(target: i32, replacement: i32) -> io::Result<()> {
        if target == replacement {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "protocol and diagnostic CRT descriptors must be distinct",
            ));
        }
        let _ = crt_handle(target, "protocol CRT descriptor is unavailable")?;
        let _ = crt_handle(replacement, "diagnostic CRT descriptor is unavailable")?;

        // SAFETY: both descriptors are live and distinct. _dup2 closes target before duplicating
        // replacement, leaving replacement itself unchanged.
        if unsafe { libc::dup2(replacement, target) } == -1 {
            return Err(last_crt_error(
                "cannot redirect CRT stdout to diagnostic stderr",
            ));
        }
        let _ = crt_handle(target, "redirected CRT stdout is unavailable")?;
        Ok(())
    }

    fn crt_handle(descriptor: i32, context: &'static str) -> io::Result<HANDLE> {
        if descriptor < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{context}: invalid descriptor"),
            ));
        }
        // SAFETY: get_osfhandle only inspects the process CRT descriptor table.
        let handle = unsafe { libc::get_osfhandle(descriptor) };
        if handle == -1 {
            return Err(last_crt_error(context));
        }
        let handle = handle as HANDLE;
        validate_handle(handle, context)?;
        Ok(handle)
    }

    fn validate_handle(handle: HANDLE, context: &'static str) -> io::Result<()> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{context}: invalid handle"),
            ));
        }
        let mut flags = 0;
        // SAFETY: handle is borrowed and flags points to writable storage.
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
            return Err(last_os_error(context));
        }
        Ok(())
    }

    fn require_noninheritable(handle: HANDLE, context: &'static str) -> io::Result<()> {
        let mut flags = 0;
        // SAFETY: handle is live and flags points to writable storage.
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
            return Err(last_os_error(context));
        }
        if flags & HANDLE_FLAG_INHERIT != 0 {
            return Err(io::Error::other(context));
        }
        Ok(())
    }

    fn last_os_error(context: &'static str) -> io::Error {
        let error = io::Error::last_os_error();
        io::Error::new(error.kind(), format!("{context}: {error}"))
    }

    fn last_crt_error(context: &'static str) -> io::Error {
        // SAFETY: _errno returns this thread's live CRT errno slot.
        let errno = unsafe { *crt_errno() };
        io::Error::other(format!("{context}: CRT errno {errno}"))
    }

    #[cfg(test)]
    mod tests {
        use std::fs::{self, OpenOptions};
        use std::io::Write;
        use std::os::windows::ffi::OsStrExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        use super::*;

        static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

        struct TempFile(std::path::PathBuf);

        impl TempFile {
            fn new(label: &str) -> Self {
                let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
                Self(std::env::temp_dir().join(format!(
                    "stasis-stdio-{}-{sequence}-{label}.tmp",
                    std::process::id()
                )))
            }
        }

        impl Drop for TempFile {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }

        struct CrtFileDescriptor(i32);

        impl CrtFileDescriptor {
            fn create(path: &std::path::Path) -> Self {
                let mut wide_path: Vec<u16> = path.as_os_str().encode_wide().collect();
                wide_path.push(0);
                // SAFETY: wide_path is NUL-terminated and the creation mode vararg is present.
                let descriptor = unsafe {
                    libc::wopen(
                        wide_path.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_BINARY
                            | libc::O_NOINHERIT,
                        libc::S_IWRITE,
                    )
                };
                assert_ne!(descriptor, -1, "{}", last_crt_error("test _wopen failed"));
                Self(descriptor)
            }
        }

        impl Drop for CrtFileDescriptor {
            fn drop(&mut self) {
                // SAFETY: this wrapper is the sole owner of a live CRT descriptor.
                let _ = unsafe { libc::close(self.0) };
            }
        }

        #[test]
        fn duplicated_protocol_handle_is_owned_writable_and_noninheritable() {
            let path = TempFile::new("protocol-handle");
            let source = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path.0)
                .unwrap();

            let duplicate = duplicate_private_handle(source.as_raw_handle() as HANDLE).unwrap();
            require_noninheritable(
                duplicate.as_raw_handle() as HANDLE,
                "test duplicate is inheritable",
            )
            .unwrap();
            let mut protocol = File::from(duplicate);
            protocol.write_all(b"protocol").unwrap();
            protocol.flush().unwrap();
            drop(protocol);
            drop(source);

            assert_eq!(fs::read(&path.0).unwrap(), b"protocol");
        }

        #[test]
        fn crt_redirection_preserves_diagnostics_and_replaces_only_the_target() {
            let protocol_path = TempFile::new("crt-protocol");
            let diagnostic_path = TempFile::new("crt-diagnostic");
            let protocol = CrtFileDescriptor::create(&protocol_path.0);
            let diagnostic = CrtFileDescriptor::create(&diagnostic_path.0);

            redirect_crt_descriptor(protocol.0, diagnostic.0).unwrap();
            let bytes = b"diagnostic";
            // SAFETY: protocol remains live and now refers to diagnostic's destination.
            assert_eq!(
                unsafe { libc::write(protocol.0, bytes.as_ptr().cast(), bytes.len() as u32) },
                bytes.len() as i32
            );
            drop(protocol);
            drop(diagnostic);

            assert!(fs::read(&protocol_path.0).unwrap().is_empty());
            assert_eq!(fs::read(&diagnostic_path.0).unwrap(), bytes);
        }

        #[test]
        fn invalid_protocol_handle_is_rejected_without_ownership() {
            assert!(duplicate_private_handle(ptr::null_mut()).is_err());
            assert!(duplicate_private_handle(INVALID_HANDLE_VALUE).is_err());
        }
    }
}
/// Claim the stdout pipe inherited from the client for protocol frames and redirect ordinary
/// standard output away from it. Call exactly once, before Servo or any native worker threads are
/// started, and pass the returned file to `ProtocolWriter`.
#[cfg(any(unix, windows))]
pub fn claim_protocol_stdout() -> io::Result<File> {
    platform::claim_protocol_stdout()
}

#[cfg(not(any(unix, windows)))]
pub fn claim_protocol_stdout() -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "saved protocol stdout is not implemented on this platform",
    ))
}
