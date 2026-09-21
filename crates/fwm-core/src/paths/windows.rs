//! Protect config/runtime directories even when their parent has a public ACL.
use anyhow::{Context, Result, bail};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PATH_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
    TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
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

pub(super) fn ensure_private_directory(path: &Path) -> Result<()> {
    let sid = current_user_sid()?;
    // Set the owner during creation, including when an elevated token's default
    // owner is Administrators. Never take ownership of an existing directory.
    let descriptor = private_descriptor(&sid)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    create_directories(path, &attributes)?;
    restrict_directory(path, &sid)
}

fn wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        bail!("configuration directory contains a NUL character");
    }
    name.push(0);
    Ok(name)
}

fn create_directories(path: &Path, attributes: &SECURITY_ATTRIBUTES) -> Result<()> {
    let name = wide_path(path)?;
    // SAFETY: name and security attributes remain alive throughout the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), attributes) } != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_ALREADY_EXISTS) => Ok(()),
        Some(ERROR_PATH_NOT_FOUND) => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            let Some(parent) = parent else {
                return Err(error).context("finding configuration directory parent");
            };
            create_directories(parent, attributes)?;
            // SAFETY: identical live inputs; a concurrent creator is validated
            // by restrict_directory after this function returns.
            if unsafe { CreateDirectoryW(name.as_ptr(), attributes) } == 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                    return Err(error).context("creating private configuration directory");
                }
            }
            Ok(())
        }
        _ => Err(error).context("creating private configuration directory"),
    }
}

fn restrict_directory(path: &Path, sid: &str) -> Result<()> {
    let name = wide_path(path)?;
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
    verify_directory_owner(handle.0, sid).with_context(|| {
        format!(
            "refusing untrusted configuration/runtime directory {}",
            path.display()
        )
    })?;
    // Protected DACL: only this user has access. Object/container inheritance
    // makes newly created files and subdirectories private as well.
    let descriptor = private_descriptor(sid)?;
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

fn private_descriptor(sid: &str) -> Result<LocalAllocation> {
    let descriptor: Vec<u16> = format!("O:{sid}D:P(A;OICI;FA;;;{sid})")
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
    Ok(LocalAllocation(raw_descriptor))
}

fn verify_directory_owner(handle: HANDLE, expected_sid: &str) -> Result<()> {
    let mut owner = ptr::null_mut();
    let mut security = ptr::null_mut();
    // SAFETY: handle stays open during this call and its owner check; outputs
    // point into the security allocation, which lives until after SID conversion.
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut security,
        )
    };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result as i32))
            .context("reading directory owner");
    }
    let _security = LocalAllocation(security);
    if owner.is_null() {
        bail!("configuration directory has no owner");
    }
    // SAFETY: owner refers to a valid SID in the live security descriptor.
    let actual_sid = unsafe { sid_string(owner) }?;
    let elevated = actual_sid == "S-1-5-32-544" && current_user_is_elevated()?;
    if !owner_is_trusted(&actual_sid, expected_sid, elevated) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("directory belongs to SID {actual_sid}, expected {expected_sid}; use a directory owned by the current user"),
        )).context("validating configuration directory owner");
    }
    Ok(())
}

fn owner_is_trusted(actual_sid: &str, expected_sid: &str, elevated: bool) -> bool {
    // Administrators already control an elevated caller. Preserve access to
    // directories created by earlier elevated fwm versions without taking their
    // ownership. An ordinary account/group owner can still change the DACL and
    // must be rejected even if it has granted us WRITE_DAC.
    actual_sid == expected_sid || (actual_sid == "S-1-5-32-544" && elevated)
}

fn current_user_is_elevated() -> Result<bool> {
    let mut raw = ptr::null_mut();
    // SAFETY: current process is valid and raw is writable output storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(std::io::Error::last_os_error()).context("reading current user token");
    }
    let token = Handle(raw);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut length = 0;
    // SAFETY: elevation has the required size and alignment; token is live.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("reading current user elevation");
    }
    Ok(elevation.TokenIsElevated != 0)
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
    // SAFETY: SID refers to the live token buffer.
    unsafe { sid_string(user.User.Sid) }
}

/// The caller must keep a valid SID alive for the duration of the call.
unsafe fn sid_string(sid: PSID) -> Result<String> {
    let mut text = ptr::null_mut();
    // SAFETY: SID validity is guaranteed by the caller; text receives an allocation.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
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
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_private_directory(&directory).unwrap();
        let sid = current_user_sid().unwrap();
        let acl = descriptor(&directory);
        assert!(acl.starts_with("D:P"), "{acl}");
        assert!(acl.contains(&format!(";;;{sid})")), "{acl}");
        assert_eq!(acl.matches('(').count(), 1, "{acl}");
        let file = directory.join("config.toml");
        std::fs::write(&file, "private").unwrap();
        let inherited = descriptor(&file);
        assert_eq!(inherited.matches('(').count(), 1, "{inherited}");
        assert!(inherited.contains(&format!(";;;{sid})")), "{inherited}");
    }

    #[test]
    fn different_directory_owner_is_rejected_without_mutating_acl() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_private_directory(&directory).unwrap();
        let before = descriptor(&directory);
        // The real handle owner differs from this expected SID; no second
        // account or privileged ownership change is needed to exercise rejection.
        let error = restrict_directory(&directory, "S-1-0-0").unwrap_err();
        assert!(format!("{error:#}").contains("directory belongs to SID"));
        assert_eq!(descriptor(&directory), before);
    }

    #[test]
    fn missing_nested_directories_are_created_with_private_permissions() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("config");
        let directory = parent.join("nested").join("state");
        ensure_private_directory(&directory).unwrap();
        let sid = current_user_sid().unwrap();
        for path in [&parent, &parent.join("nested"), &directory] {
            restrict_directory(path, &sid).unwrap();
            let acl = descriptor(path);
            assert!(acl.starts_with("D:P"), "{acl}");
            assert_eq!(acl.matches('(').count(), 1, "{acl}");
        }
    }

    #[test]
    fn unreadable_directory_owner_is_rejected() {
        assert!(verify_directory_owner(ptr::null_mut(), "S-1-0-0").is_err());
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

    #[test]
    fn elevated_caller_can_restrict_legacy_administrator_directory() {
        if !current_user_is_elevated().unwrap() {
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("legacy");
        let security = private_descriptor("S-1-5-32-544").unwrap();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: 0,
        };
        create_directories(&directory, &attributes).unwrap();
        ensure_private_directory(&directory).unwrap();
        let sid = current_user_sid().unwrap();
        let acl = descriptor(&directory);
        assert!(acl.contains(&format!(";;;{sid})")), "{acl}");
        assert_eq!(acl.matches('(').count(), 1, "{acl}");
    }
}
