//! Owner-only ACLs for Windows IPC secrets, independent of inherited defaults.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{NonNull, null_mut};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, INHERIT_ONLY_ACE,
    INHERITED_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    WRITE_DAC,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct LocalAllocation(NonNull<c_void>);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: each value owns one allocation returned by a LocalAlloc-based
        // Win32 security API. No pointers into it outlive this owner.
        unsafe {
            LocalFree(self.0.as_ptr());
        }
    }
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn current_sid() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: valid process pseudo-handle and writable output handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful OpenProcessToken transfers this fresh handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut required = 0;
    // SAFETY: a null output buffer with length zero queries the required size.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            null_mut(),
            0,
            &mut required,
        );
    }
    if required < size_of::<TOKEN_USER>() as u32 || required > 65_536 {
        return Err(denied("Invalid Windows token user size"));
    }
    // Word storage provides the pointer alignment TOKEN_USER requires.
    let mut storage = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
    // SAFETY: storage is aligned, writable and at least `required` bytes long.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser query populated this structure and SID.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    // SAFETY: user SID remains owned by storage throughout the call.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let text_owner =
        LocalAllocation(NonNull::new(text.cast()).ok_or_else(|| denied("Missing SID"))?);
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW returns an allocated NUL-terminated string.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
    }
    // SAFETY: the scan found this exact initialized UTF-16 prefix.
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })
        .map_err(|_| denied("Invalid account SID encoding"))?;
    drop(text_owner);
    Ok(sid)
}

/// Security descriptor granting full access only to the actual process user SID.
pub(super) struct PrivateAcl(LocalAllocation);

impl PrivateAcl {
    pub(super) fn new() -> io::Result<Self> {
        let sid = current_sid()?;
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;OICI;FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = null_mut();
        // SAFETY: NUL-terminated input and valid writable output pointer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalAllocation(NonNull::new(descriptor).ok_or_else(
            || denied("Missing private security descriptor"),
        )?)))
    }

    fn owner(&self) -> io::Result<PSID> {
        let mut owner = null_mut();
        let mut defaulted = 0;
        // SAFETY: this object owns the valid descriptor and both outputs.
        if unsafe { GetSecurityDescriptorOwner(self.0.0.as_ptr(), &mut owner, &mut defaulted) } == 0
            || owner.is_null()
        {
            return Err(denied("Private descriptor has no owner"));
        }
        Ok(owner)
    }

    fn dacl(&self) -> io::Result<*mut windows_sys::Win32::Security::ACL> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        // SAFETY: this object owns the valid descriptor and all outputs.
        if unsafe {
            GetSecurityDescriptorDacl(self.0.0.as_ptr(), &mut present, &mut acl, &mut defaulted)
        } == 0
            || present == 0
            || acl.is_null()
        {
            return Err(denied("Private descriptor has no DACL"));
        }
        Ok(acl)
    }

    /// Create one directory with its account owner and protected DACL already installed.
    fn create_directory(&self, path: &Path) -> io::Result<()> {
        let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
        if name.contains(&0) {
            return Err(denied("IPC directory path contains NUL"));
        }
        name.push(0);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0.as_ptr(),
            bInheritHandle: 0,
        };
        // SAFETY: the NUL-terminated path and owned descriptor remain valid
        // through this synchronous call. Windows copies the security descriptor.
        if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
            // Existing objects are not trusted here. secure_directory pins and
            // validates the final object's type and owner before changing it.
            Ok(())
        } else {
            Err(error)
        }
    }

    fn create_directory_tree(&self, path: &Path) -> io::Result<()> {
        let mut pending = Vec::new();
        let mut candidate = path;
        loop {
            match self.create_directory(candidate) {
                Ok(()) => break,
                Err(error) if error.raw_os_error() == Some(ERROR_PATH_NOT_FOUND as i32) => {
                    pending.push(candidate);
                    candidate = candidate
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .ok_or(error)?;
                }
                Err(error) => return Err(error),
            }
        }
        for directory in pending.into_iter().rev() {
            self.create_directory(directory)?;
        }
        Ok(())
    }

    /// Pin an owned directory, protect its DACL, and keep it from being renamed.
    pub(super) fn secure_directory(&self, path: &Path) -> io::Result<File> {
        self.create_directory_tree(path)?;
        let directory = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(denied("IPC directory must not be a reparse point"));
        }
        self.check_owner(&directory)?;
        // SAFETY: directory handle is live; this descriptor owns a valid ACL.
        let error = unsafe {
            SetSecurityInfo(
                directory.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                self.dacl()?,
                null_mut(),
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        self.validate(&directory)?;
        Ok(directory)
    }

    fn descriptor(file: &File) -> io::Result<LocalAllocation> {
        let mut descriptor = null_mut();
        // SAFETY: file handle is live and descriptor output is writable.
        let error = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(LocalAllocation(
            NonNull::new(descriptor).ok_or_else(|| denied("Missing file DACL"))?,
        ))
    }

    fn check_owner(&self, file: &File) -> io::Result<()> {
        let descriptor = Self::descriptor(file)?;
        let mut owner = null_mut();
        let mut defaulted = 0;
        // SAFETY: both security descriptors are live and owner outputs valid.
        if unsafe { GetSecurityDescriptorOwner(descriptor.0.as_ptr(), &mut owner, &mut defaulted) }
            == 0
            || owner.is_null()
            || unsafe { EqualSid(owner, self.owner()?) } == 0
        {
            return Err(denied("IPC object is not owned by the daemon account"));
        }
        Ok(())
    }

    /// Refuse inherited, null, foreign-owner or additional-account permissions.
    pub(super) fn validate(&self, file: &File) -> io::Result<()> {
        self.check_owner(file)?;
        let descriptor = Self::descriptor(file)?;
        let mut control = 0;
        let mut revision = 0;
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        // SAFETY: descriptor is valid and each output is writable.
        if unsafe {
            GetSecurityDescriptorControl(descriptor.0.as_ptr(), &mut control, &mut revision)
        } == 0
            || control & SE_DACL_PROTECTED == 0
            || unsafe {
                GetSecurityDescriptorDacl(
                    descriptor.0.as_ptr(),
                    &mut present,
                    &mut acl,
                    &mut defaulted,
                )
            } == 0
            || present == 0
            || acl.is_null()
        {
            return Err(denied("IPC secret requires a protected non-null DACL"));
        }
        // SAFETY: GetSecurityDescriptorDacl returned an ACL within descriptor.
        if unsafe { (*acl).AceCount } != 1 {
            return Err(denied("IPC DACL must grant only its owner"));
        }
        let mut ace = null_mut();
        // SAFETY: ACL contains one ACE; output pointer is writable.
        if unsafe { GetAce(acl, 0, &mut ace) } == 0 || ace.is_null() {
            return Err(denied("Invalid IPC access entry"));
        }
        // SAFETY: GetAce returns a validated ACE. Type/size are checked before
        // interpreting its access mask and variable-length SID.
        let header = unsafe { &*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
        if header.AceType != 0
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
            || u32::from(header.AceFlags) & (INHERITED_ACE | INHERIT_ONLY_ACE) != 0
        {
            return Err(denied("Unexpected IPC access entry"));
        }
        // SAFETY: validated access-allowed ACE contains its mask and SIDStart.
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
        // SAFETY: both SIDs live in valid, owned security descriptors.
        if allowed.Mask != FILE_ALL_ACCESS || unsafe { EqualSid(sid, self.owner()?) } == 0 {
            return Err(denied("IPC access is not restricted to its owner"));
        }
        Ok(())
    }

    /// Create a new file with its restrictive descriptor before any secret write.
    pub(super) fn create_file(&self, path: &Path) -> io::Result<File> {
        let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
        if name.contains(&0) {
            return Err(denied("IPC path contains NUL"));
        }
        name.push(0);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0.as_ptr(),
            bInheritHandle: 0,
        };
        // SAFETY: path/descriptor live through the call; CREATE_NEW refuses any
        // pre-existing object, including symlinks and planted temporary files.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful CreateFileW transfers a new owned handle.
        let file = unsafe { File::from_raw_handle(handle) };
        self.validate(&file)?;
        Ok(file)
    }

    /// Open a token without following a reparse point, then validate its ACL.
    pub(super) fn open_file(&self, path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .access_mode(GENERIC_READ | READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(denied("IPC token must be a regular file"));
        }
        self.validate(&file)?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::PrivateAcl;
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn directory_tree_is_private_before_any_later_hardening() {
        let root = tempfile::tempdir().expect("fixture directory");
        let parent = root.path().join("private");
        let child = parent.join("nested");
        let acl = PrivateAcl::new().expect("account policy");
        acl.create_directory_tree(&child)
            .expect("protected creation");
        for path in [&parent, &child] {
            let directory = std::fs::OpenOptions::new()
                .access_mode(super::FILE_READ_ATTRIBUTES | super::READ_CONTROL)
                .custom_flags(
                    super::FILE_FLAG_BACKUP_SEMANTICS | super::FILE_FLAG_OPEN_REPARSE_POINT,
                )
                .open(path)
                .expect("inspect newly created directory");
            acl.validate(&directory)
                .expect("owner and protection precede hardening");
        }
    }

    #[test]
    fn token_is_protected_before_write_and_cannot_truncate_a_planted_file() {
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("private");
        let acl = PrivateAcl::new().expect("account policy");
        let _directory = acl.secure_directory(&path).expect("private directory");
        let token = path.join("token");
        let mut file = acl.create_file(&token).expect("atomic protected creation");
        acl.validate(&file).expect("protection precedes content");
        file.write_all(b"secret").expect("write fixture");
        assert!(acl.create_file(&token).is_err());
        assert_eq!(std::fs::read(&token).unwrap(), b"secret");
        assert!(std::fs::rename(&path, root.path().join("substituted")).is_err());
    }

    #[test]
    fn inherited_token_acl_is_rejected_until_explicitly_protected() {
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("private");
        let acl = PrivateAcl::new().expect("account policy");
        let _directory = acl.secure_directory(&path).expect("private directory");
        let inherited = path.join("inherited");
        std::fs::write(&inherited, b"old").expect("inherited fixture");
        assert!(acl.open_file(&inherited).is_err());
        let private = path.join("protected");
        drop(acl.create_file(&private).expect("private fixture"));
        assert!(acl.open_file(&private).is_ok());
    }
}
