use super::Stream;
use anyhow::{Context, Result, bail};
use fwm_core::paths::Paths;
use std::ffi::c_void;
use std::ptr;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub struct Listener {
    server: Mutex<NamedPipeServer>,
    name: String,
    security_descriptor: String,
}

pub fn bind(paths: &Paths) -> Result<Listener> {
    let name = paths.pipe_name();
    let security_descriptor = format!("D:P(A;;GA;;;{})", current_user_sid()?);
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
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let names = paths.pipe_names();
    loop {
        let mut busy = None;
        let mut missing = None;
        for name in &names {
            match ClientOptions::new().open(name) {
                Ok(stream) => return Ok(Box::pin(stream)),
                Err(error) if error.raw_os_error() == Some(231) => busy = Some(error),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = Some(error),
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
