use super::{Stream, auth_error};
use anyhow::{Context, Result, bail};
use fwm_core::paths::Paths;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, GetAce,
    GetSecurityDescriptorControl, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub struct Listener {
    server: Mutex<NamedPipeServer>,
    name: String,
    security_descriptor: String,
}

pub fn bind(paths: &Paths) -> Result<Listener> {
    let name = paths.pipe_name();
    let sid = current_user_sid()?;
    // An elevated token may default new objects to the Administrators group.
    // Use the individual account as owner as well as the only permitted user.
    let security_descriptor = format!("O:{sid}D:P(A;;GA;;;{sid})");
    let server = create_pipe(&name, &security_descriptor, true)?;
    Ok(Listener {
        server: Mutex::new(server),
        name,
        security_descriptor,
    })
}

impl Listener {
    pub async fn accept(&self) -> Result<Stream> {
        let mut server = self.server.lock().await;
        server.connect().await?;
        // Create the next instance before exposing this one so clients never see a
        // transient missing pipe between accepted connections.
        let next = create_pipe(&self.name, &self.security_descriptor, false)?;
        let accepted = std::mem::replace(&mut *server, next);
        Ok(Box::pin(accepted))
    }
}

pub async fn connect(paths: &Paths) -> Result<Stream> {
    let sid = current_user_sid()
        .map_err(|error| auth_error(format!("cannot identify current IPC user: {error:#}")))?;
    let names = paths.pipe_names();
    connect_to_names(&names, &sid).await
}

async fn connect_to_names(names: &[String], expected_sid: &str) -> Result<Stream> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let mut busy = None;
        let mut missing = None;
        for name in names {
            match ClientOptions::new()
                // Never let even a forged pipe impersonate an elevated CLI.
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name)
            {
                Ok(stream) => {
                    verify_pipe_server(&stream, expected_sid)?;
                    return Ok(Box::pin(stream));
                }
                Err(error) if error.raw_os_error() == Some(231) => busy = Some(error),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = Some(error),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(auth_error(format!(
                        "cannot open daemon pipe securely: {error}"
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        }
        if busy.is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            continue;
        }
        return Err(busy
            .or(missing)
            .unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "daemon pipe is missing")
            })
            .into());
    }
}

/// Authenticate the pipe object before sending any protocol bytes. Reading its
/// owner through the connected handle avoids pathname and process-ID reuse races.
/// The private DACL also prevents another account creating a server instance of
/// a pipe owned by this user. The same DACL authenticates clients in the kernel;
/// create_pipe additionally rejects remote clients.
fn verify_pipe_server(stream: &impl AsRawHandle, expected_sid: &str) -> Result<()> {
    verify_pipe_security(stream.as_raw_handle(), expected_sid)
        .map_err(|error| auth_error(format!("cannot authenticate daemon pipe: {error:#}")))
}

fn verify_pipe_security(handle: HANDLE, expected_sid: &str) -> Result<()> {
    let mut owner = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut security = ptr::null_mut();
    // SAFETY: the caller keeps the pipe handle alive; output storage is valid.
    // The returned owner and DACL remain valid until security is freed below.
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut security,
        )
    };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result as i32))
            .context("reading daemon pipe security");
    }
    let _security = LocalAllocation(security);
    if owner.is_null() || security.is_null() {
        bail!("daemon pipe has no owner");
    }
    // SAFETY: owner is a SID returned by GetSecurityInfo and its allocation lives.
    let owner_sid = unsafe { sid_string(owner) }?;
    let elevated = owner_sid == "S-1-5-32-544" && current_user_is_elevated()?;
    if !owner_is_trusted(&owner_sid, expected_sid, elevated) {
        bail!("daemon pipe belongs to SID {owner_sid}, expected {expected_sid}");
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: security is a live Windows security descriptor.
    if unsafe { GetSecurityDescriptorControl(security, &mut control, &mut revision) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading daemon pipe DACL control");
    }
    // SAFETY: a non-null DACL returned by GetSecurityInfo is a valid ACL.
    if control & SE_DACL_PROTECTED == 0 || dacl.is_null() || unsafe { (*dacl).AceCount } != 1 {
        bail!("daemon pipe does not have a private current-user DACL");
    }
    let mut ace = ptr::null_mut();
    // SAFETY: dacl is valid and contains exactly one entry; ace is out storage.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading daemon pipe DACL entry");
    }
    // SAFETY: Windows validated this ACL; check its type before reading the
    // ACCESS_ALLOWED_ACE-specific SID. Type 0 is ACCESS_ALLOWED_ACE_TYPE.
    let header = unsafe { &*ace.cast::<ACE_HEADER>() };
    if header.AceType != 0 || header.AceFlags != 0 {
        bail!("daemon pipe does not have a private current-user DACL entry");
    }
    // SAFETY: type 0 is an ACCESS_ALLOWED_ACE with a valid trailing SID.
    let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    let allowed_sid = unsafe { sid_string(ptr::addr_of!(ace.SidStart).cast_mut().cast()) }?;
    if allowed_sid != expected_sid {
        bail!("daemon pipe grants access to SID {allowed_sid}, expected {expected_sid}");
    }
    Ok(())
}

fn owner_is_trusted(actual_sid: &str, expected_sid: &str, elevated: bool) -> bool {
    // Earlier elevated daemons may use the token's Administrators owner. An
    // elevated caller already trusts administrators; the separate exact DACL
    // check still requires that only this account can create pipe instances.
    actual_sid == expected_sid || (actual_sid == "S-1-5-32-544" && elevated)
}

fn current_user_is_elevated() -> Result<bool> {
    let mut token = ptr::null_mut();
    // SAFETY: current process is valid and token is writable output storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading current user token");
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut length = 0;
    // SAFETY: elevation has the required size and alignment; token is live.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: OpenProcessToken returned an owned handle, closed exactly once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(error).context("reading current user elevation");
    }
    Ok(elevation.TokenIsElevated != 0)
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: this owns an allocation returned by a LocalFree-compatible API.
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// The caller must keep a valid SID alive for the duration of this call.
unsafe fn sid_string(sid: PSID) -> Result<String> {
    let mut text = ptr::null_mut();
    // SAFETY: guaranteed by the caller; text is valid writable out storage.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error()).context("formatting SID");
    }
    let _allocation = LocalAllocation(text.cast());
    let mut length = 0;
    // SAFETY: Windows returned a nul-terminated UTF-16 string.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, length,
        )))
    }
}

fn create_pipe(name: &str, descriptor: &str, first: bool) -> Result<NamedPipeServer> {
    let text: Vec<u16> = descriptor.encode_utf16().chain(Some(0)).collect();
    let mut security = ptr::null_mut();
    // SAFETY: text is a nul-terminated UTF-16 string; Windows allocates security,
    // which remains alive for CreateNamedPipe and is released afterwards.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut security,
            ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("creating current-user pipe security descriptor");
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security,
            bInheritHandle: 0,
        };
        let result = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&attributes as *const SECURITY_ATTRIBUTES).cast::<c_void>() as *mut c_void,
            );
        LocalFree(security.cast());
        result.context("creating private daemon pipe")
    }
}

pub fn current_user_sid() -> Result<String> {
    // SAFETY: each Windows call receives initialized storage of the required size;
    // token and converted string are released on every branch after allocation.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error()).context("reading current user token");
        }
        let mut length = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
        if length == 0 {
            CloseHandle(token);
            bail!("current user token has no SID");
        }
        // usize storage guarantees TOKEN_USER alignment.
        let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        );
        let error = std::io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(error).context("reading current user SID");
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid_text = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_text) == 0 {
            return Err(std::io::Error::last_os_error()).context("formatting current user SID");
        }
        let mut size = 0;
        while *sid_text.add(size) != 0 {
            size += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, size));
        LocalFree(sid_text.cast());
        Ok(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn pipe_name() -> String {
        format!(r"\\.\pipe\fwm-auth-test-{}", uuid::Uuid::new_v4())
    }

    #[tokio::test]
    async fn current_user_pipe_is_authenticated_before_exchange() {
        let sid = current_user_sid().unwrap();
        let name = pipe_name();
        let mut server = create_pipe(&name, &format!("O:{sid}D:P(A;;GA;;;{sid})"), true).unwrap();
        let names = vec![name];
        let (accepted, connected) = tokio::join!(server.connect(), connect_to_names(&names, &sid));
        accepted.unwrap();
        let mut client = connected.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut message = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.read_exact(&mut message),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&message, b"ping");
    }

    #[tokio::test]
    async fn unexpected_pipe_owner_is_rejected() {
        let sid = current_user_sid().unwrap();
        let name = pipe_name();
        let _server = create_pipe(&name, &format!("O:{sid}D:P(A;;GA;;;{sid})"), true).unwrap();
        // Exercise the actual kernel owner query without requiring a second account.
        let error = connect_to_names(&[name], "S-1-0-0").await.err().unwrap();
        assert!(super::super::is_authentication_error(&error));
        assert!(error.to_string().contains("daemon pipe belongs to SID"));
    }

    #[tokio::test]
    async fn current_owner_with_public_server_instance_access_is_rejected() {
        let sid = current_user_sid().unwrap();
        let name = pipe_name();
        let _server = create_pipe(&name, &format!("O:{sid}D:P(A;;GA;;;WD)"), true).unwrap();
        let error = connect_to_names(&[name], &sid).await.err().unwrap();
        assert!(super::super::is_authentication_error(&error));
        assert!(
            error
                .to_string()
                .contains("daemon pipe grants access to SID")
        );
    }

    #[tokio::test]
    async fn missing_private_dacl_is_rejected() {
        let sid = current_user_sid().unwrap();
        let name = pipe_name();
        let _server = create_pipe(&name, &format!("O:{sid}D:(A;;GA;;;{sid})"), true).unwrap();
        let error = connect_to_names(&[name], &sid).await.err().unwrap();
        assert!(super::super::is_authentication_error(&error));
        assert!(error.to_string().contains("private current-user DACL"));
    }

    #[test]
    fn unreadable_pipe_security_fails_closed() {
        assert!(verify_pipe_security(ptr::null_mut(), "S-1-0-0").is_err());
    }

    #[test]
    fn legacy_administrator_owner_requires_elevated_caller() {
        let user = "S-1-5-21-1-2-3-1000";
        assert!(owner_is_trusted(user, user, false));
        assert!(owner_is_trusted("S-1-5-32-544", user, true));
        assert!(!owner_is_trusted("S-1-5-32-544", user, false));
        assert!(!owner_is_trusted("S-1-5-18", user, true));
        assert!(!owner_is_trusted("S-1-5-21-1-2-3-1001", user, true));
    }

    #[tokio::test]
    async fn elevated_caller_can_authenticate_private_legacy_administrator_pipe() {
        if !current_user_is_elevated().unwrap() {
            return;
        }
        let sid = current_user_sid().unwrap();
        let name = pipe_name();
        let _server = create_pipe(&name, &format!("O:BAD:P(A;;GA;;;{sid})"), true).unwrap();
        assert!(connect_to_names(&[name], &sid).await.is_ok());
    }
}
