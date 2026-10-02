//! The little bit of Win32 the relay needs: who we are, and who is on the other
//! end of the pipe.
//!
//! Named pipes live in a machine-wide namespace, so `\\.\pipe\coucou-<name>` can
//! be created by *any* account that gets there first. Three defences, all cheap:
//! the pipe name carries our SID; Coucou creates every instance with a DACL that
//! admits only that SID (src-tauri/src/pipe_acl.rs); and once connected we check
//! the server process really belongs to us before sending anything, having
//! opened the pipe so that the server can identify us but not impersonate us.

use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::CONNECT_TIMEOUT;

/// `ERROR_PIPE_BUSY` — every instance is serving someone else right now. This is
/// the one error worth retrying: the server exists and a slot will free up.
const ERROR_PIPE_BUSY: i32 = 231;

/// `SECURITY_IDENTIFICATION`: the server may learn who we are but may not act as
/// us. Without a security-QoS flag a server that reads from the pipe can call
/// `ImpersonateNamedPipeClient` and run with our token. Only the real Coucou
/// should be on the other end; if it is not, this keeps it from being worse.
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

/// Opens `\\.\pipe\coucou-<sid>`. The SID keeps two accounts on the same machine
/// from ever meeting on the same pipe. Without a readable SID there is nobody we
/// can vouch for, so there is no connection (the server refuses to start in the
/// same case).
pub fn connect() -> Option<std::fs::File> {
    let sid = current_user_sid()?;
    connect_to(&format!(r"\\.\pipe\coucou-{sid}"), &sid)
}

/// Opens the pipe at `path` and hands it back only if the process serving it runs
/// as `sid`. Retries only while the server is busy: any other error means there is
/// nothing to talk to, and waiting would only delay Claude Code.
fn connect_to(path: &str, sid: &str) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(path)
        {
            Ok(file) => {
                let handle = HANDLE(file.as_raw_handle());
                // Somebody else's server on our pipe name gets nothing from us:
                // the file is dropped here, before a byte is written or read.
                return server_runs_as(handle, sid).then_some(file);
            }
            Err(err) => {
                if err.raw_os_error() != Some(ERROR_PIPE_BUSY) || Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

/// The SID of the account this process runs as, as `S-1-5-21-…`.
pub fn current_user_sid() -> Option<String> {
    unsafe { token_sid(GetCurrentProcess()) }
}

/// True when the process serving `handle` runs as the account `sid`.
///
/// A failure to answer is treated as "not ours": refusing to talk to a pipe we
/// cannot vouch for costs one hook event, while trusting it could hand another
/// account on this machine the contents of every tool call. (This names the
/// process that created the instance; it says nothing about a different process
/// of the same user, which is outside what this relay defends against.)
fn server_runs_as(handle: HANDLE, mine: &str) -> bool {
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeServerProcessId(handle, &mut pid).is_err() || pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let theirs = token_sid(process);
        let _ = CloseHandle(process);
        theirs.as_deref() == Some(mine)
    }
}

/// The user SID behind a process handle. `process` is borrowed, never closed.
unsafe fn token_sid(process: HANDLE) -> Option<String> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;

    // First call sizes the buffer, second fills it.
    let mut needed = 0u32;
    let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    if needed == 0 {
        let _ = CloseHandle(token);
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = GetTokenInformation(
        token,
        TokenUser,
        Some(buf.as_mut_ptr().cast()),
        needed,
        &mut needed,
    )
    .is_ok();
    let _ = CloseHandle(token);
    if !ok {
        return None;
    }

    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut text = PWSTR::null();
    ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
    let sid = text.to_string().ok();
    let _ = LocalFree(Some(HLOCAL(text.0 as *mut _)));
    sid
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// A real pipe server made by this process, so `GetNamedPipeServerProcessId`
    /// answers with our own pid and the check is exercised for real.
    struct Server(HANDLE, String);

    impl Server {
        fn new(tag: &str) -> Server {
            let path = format!(r"\\.\pipe\coucou-hook-test-{tag}-{}", std::process::id());
            let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(wide.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            assert!(!handle.is_invalid(), "CreateNamedPipeW failed: {:?}", std::io::Error::last_os_error());
            Server(handle, path)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[test]
    fn a_server_running_as_us_is_accepted() {
        let server = Server::new("same");
        let mine = current_user_sid().unwrap();
        assert!(connect_to(&server.1, &mine).is_some());
    }

    #[test]
    fn a_server_that_is_not_who_we_expect_is_never_handed_a_connection() {
        let server = Server::new("other");
        // The server is us, so any other SID stands in for "a different account".
        // LOCAL SYSTEM is not us unless the tests run as SYSTEM, which they do not.
        assert_ne!(current_user_sid().unwrap(), "S-1-5-18");
        assert!(connect_to(&server.1, "S-1-5-18").is_none());
        // A malformed expectation is no more trusting.
        assert!(connect_to(&server.1, "").is_none());
    }

    #[test]
    fn no_pipe_means_no_connection_and_no_wait() {
        let started = Instant::now();
        let mine = current_user_sid().unwrap();
        assert!(connect_to(r"\\.\pipe\coucou-hook-test-nobody-home", &mine).is_none());
        assert!(started.elapsed() < Duration::from_millis(250), "NotFound must not be retried");
    }
}
