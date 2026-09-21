//! Protect config/runtime directories even when their parent has a public ACL.
use anyhow::{Context, Result, bail};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT,
    SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: Handle is only constructed from a successful owning API call.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: allocations are returned by Windows APIs documenting LocalFree.
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub(super) fn restrict_directory(path: &Path) -> Result<()> {
    let sid = current_user_sid()?;
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        bail!("configuration directory contains a NUL character");
    }
    name.push(0);
    // Opening the object itself prevents following a final-component junction.
    // Omitting FILE_SHARE_DELETE prevents replacement while we inspect and set
    // the ACL on this exact handle rather than resolving the path a second time.
    // SAFETY: name is a valid nul-terminated UTF-16 path, other pointers are null.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("opening directory permissions for {}", path.display()));
    }
    let handle = Handle(raw);
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: handle is live and information is initialized writable storage.
    if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0 {
        return Err(std::io::Error::last_os_error()).context("inspecting configuration directory");
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!(
            "refusing reparse-point configuration/runtime directory: {}",
            path.display()
        );
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        bail!("configuration path is not a directory: {}", path.display());
    }
    // Protected DACL: only this user has access. Object/container inheritance
    // makes newly created files and subdirectories private as well.
    let descriptor: Vec<u16> = format!("D:P(A;OICI;FA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut raw_descriptor = ptr::null_mut();
    // SAFETY: the string is nul terminated, and the out pointer has valid storage.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut raw_descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("building private directory ACL");
    }
    let descriptor = LocalAllocation(raw_descriptor);
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = ptr::null_mut();
    // SAFETY: descriptor remains alive until after SetSecurityInfo has copied it.
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
        == 0
    {
        return Err(std::io::Error::last_os_error()).context("reading private directory ACL");
    }
    if present == 0 || dacl.is_null() {
        bail!("private directory ACL is missing");
    }
    // SetSecurityInfo also propagates inheritable ACEs to existing unprotected
    // children. PROTECTED stops a public parent's permissions being inherited.
    // SAFETY: handle and ACL are live; owner/group/SACL are intentionally unchanged.
    let result = unsafe {
        SetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result as i32))
            .with_context(|| format!("restricting {} to the current user", path.display()));
    }
    Ok(())
}

fn current_user_sid() -> Result<String> {
    let mut raw = ptr::null_mut();
    // SAFETY: the pseudo process handle is valid and raw is writable out storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading current user token");
    }
    let token = Handle(raw);
    let mut length = 0;
    // First call obtains the required allocation size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length);
    }
    if length == 0 {
        bail!("current user token has no SID");
    }
    let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: usize storage has TOKEN_USER alignment and at least length bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("reading current user SID");
    }
    // SAFETY: a successful TokenUser call initialized the TOKEN_USER structure.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = ptr::null_mut();
    // SAFETY: SID refers to the live token buffer; text receives an allocated string.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error()).context("formatting current user SID");
    }
    let allocation = LocalAllocation(text.cast());
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW returns a nul-terminated UTF-16 allocation.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        drop(allocation);
        Ok(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    };

    fn descriptor(path: &Path) -> String {
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut raw = ptr::null_mut();
        // SAFETY: name is nul-terminated and raw is valid output storage.
        assert_eq!(
            unsafe {
                GetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut raw,
                )
            },
            0
        );
        let allocation = LocalAllocation(raw);
        let mut text = ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    allocation.0,
                    1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    ptr::null_mut(),
                )
            },
            0
        );
        let string = LocalAllocation(text.cast());
        let mut size = 0;
        let value = unsafe {
            while *text.add(size) != 0 {
                size += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(text, size))
        };
        drop(string);
        value
    }

    #[test]
    fn directory_and_new_files_are_private_to_current_user() {
        let directory = tempfile::tempdir().unwrap();
        restrict_directory(directory.path()).unwrap();
        let sid = current_user_sid().unwrap();
        let acl = descriptor(directory.path());
        assert!(acl.starts_with("D:P"), "{acl}");
        assert!(acl.contains(&format!(";;;{sid})")), "{acl}");
        assert_eq!(acl.matches('(').count(), 1, "{acl}");
        let file = directory.path().join("config.toml");
        std::fs::write(&file, "private").unwrap();
        let inherited = descriptor(&file);
        assert_eq!(inherited.matches('(').count(), 1, "{inherited}");
        assert!(inherited.contains(&format!(";;;{sid})")), "{inherited}");
    }
}
