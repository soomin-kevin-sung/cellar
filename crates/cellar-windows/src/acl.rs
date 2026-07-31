use std::{
    fs::File,
    path::{Path, PathBuf},
};

use sha1::{Digest, Sha1};
use thiserror::Error;

pub const PRIVATE_KEY_SERVICE_NAME: &str = "Cellar";

#[derive(Debug, Error)]
pub enum AclError {
    #[error("service name cannot be empty")]
    EmptyServiceName,
    #[error("private-key ACLs are only supported on Windows")]
    UnsupportedPlatform,
    #[error("could not construct the protected private-key security descriptor: {0}")]
    Descriptor(std::io::Error),
    #[error("could not inspect the current Windows process token: {0}")]
    Token(std::io::Error),
    #[error("could not apply the protected private-key ACL to {path}: {source}")]
    Apply {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not apply the protected private-key ACL to an open file handle: {0}")]
    ApplyHandle(std::io::Error),
}

#[derive(Clone)]
pub struct PrivateKeySecurityDescriptor {
    sddl: String,
    #[cfg(windows)]
    words: Vec<u32>,
}

impl std::fmt::Debug for PrivateKeySecurityDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrivateKeySecurityDescriptor")
            .field("sddl", &self.sddl)
            .finish_non_exhaustive()
    }
}

impl PrivateKeySecurityDescriptor {
    pub fn sddl(&self) -> &str {
        &self.sddl
    }
}

pub fn service_sid_string(service_name: &str) -> Result<String, AclError> {
    if service_name.is_empty() {
        return Err(AclError::EmptyServiceName);
    }

    let uppercase = service_name.to_uppercase();
    let utf16_le: Vec<u8> = uppercase
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let digest = Sha1::digest(utf16_le);
    let authorities = digest
        .chunks_exact(4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("SHA-1 chunk is four bytes")))
        .map(|authority| authority.to_string())
        .collect::<Vec<_>>()
        .join("-");
    Ok(format!("S-1-5-80-{authorities}"))
}

/// Returns whether the current process token has the enabled built-in
/// Administrators SID. A filtered (medium-integrity) administrator token is
/// intentionally rejected.
pub fn is_elevated_administrator() -> Result<bool, AclError> {
    #[cfg(windows)]
    {
        windows_impl::is_elevated_administrator()
    }

    #[cfg(not(windows))]
    {
        Err(AclError::UnsupportedPlatform)
    }
}

pub fn build_private_key_security_descriptor(
    service_name: &str,
) -> Result<PrivateKeySecurityDescriptor, AclError> {
    let service_sid = service_sid_string(service_name)?;
    let sddl = format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x00170089;;;{service_sid})");

    #[cfg(windows)]
    {
        let words = windows_impl::descriptor_words(&sddl)?;
        Ok(PrivateKeySecurityDescriptor { sddl, words })
    }

    #[cfg(not(windows))]
    {
        Ok(PrivateKeySecurityDescriptor { sddl })
    }
}

pub fn restrict_private_key_access(path: &Path, service_name: &str) -> Result<(), AclError> {
    let descriptor = build_private_key_security_descriptor(service_name)?;

    #[cfg(windows)]
    {
        windows_impl::apply_descriptor(path, &descriptor)
    }

    #[cfg(not(windows))]
    {
        let _ = (path, descriptor);
        Err(AclError::UnsupportedPlatform)
    }
}

/// Applies the protected private-key DACL through an already-open file handle.
///
/// Callers can create a new file, retain the handle, apply this descriptor, and
/// only then write secret bytes, avoiding a path re-open race.
pub fn restrict_private_key_handle(file: &File, service_name: &str) -> Result<(), AclError> {
    let descriptor = build_private_key_security_descriptor(service_name)?;

    #[cfg(windows)]
    {
        windows_impl::apply_descriptor_to_handle(file, &descriptor)
    }

    #[cfg(not(windows))]
    {
        let _ = (file, descriptor);
        Err(AclError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::{
        ffi::OsStr,
        fs::File,
        iter,
        mem::size_of,
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT,
                SetSecurityInfo,
            },
            CheckTokenMembership, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
            GetSecurityDescriptorDacl, GetSecurityDescriptorLength,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE,
            SetFileSecurityW, WinBuiltinAdministratorsSid,
        },
    };

    use super::{AclError, Path, PrivateKeySecurityDescriptor};

    const SDDL_REVISION_1: u32 = 1;

    pub(super) fn is_elevated_administrator() -> Result<bool, AclError> {
        let mut sid = [0_u8; SECURITY_MAX_SID_SIZE as usize];
        let mut sid_size = sid.len() as u32;
        let created = unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                ptr::null_mut(),
                sid.as_mut_ptr().cast(),
                &mut sid_size,
            )
        };
        if created == 0 {
            return Err(AclError::Token(std::io::Error::last_os_error()));
        }
        let mut is_member = 0;
        let checked = unsafe {
            CheckTokenMembership(ptr::null_mut(), sid.as_mut_ptr().cast(), &mut is_member)
        };
        if checked == 0 {
            return Err(AclError::Token(std::io::Error::last_os_error()));
        }
        Ok(is_member != 0)
    }

    struct LocalDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(iter::once(0)).collect()
    }

    pub(super) fn descriptor_words(sddl: &str) -> Result<Vec<u32>, AclError> {
        let sddl = wide(OsStr::new(sddl));
        let mut descriptor = ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(AclError::Descriptor(std::io::Error::last_os_error()));
        }
        let descriptor = LocalDescriptor(descriptor);
        let length = unsafe { GetSecurityDescriptorLength(descriptor.0) } as usize;
        if length == 0 {
            return Err(AclError::Descriptor(std::io::Error::last_os_error()));
        }
        let mut words = vec![0_u32; length.div_ceil(size_of::<u32>())];
        unsafe {
            ptr::copy_nonoverlapping(
                descriptor.0.cast::<u8>(),
                words.as_mut_ptr().cast::<u8>(),
                length,
            );
        }
        Ok(words)
    }

    pub(super) fn apply_descriptor(
        path: &Path,
        descriptor: &PrivateKeySecurityDescriptor,
    ) -> Result<(), AclError> {
        let path_wide = wide(path.as_os_str());
        let applied = unsafe {
            SetFileSecurityW(
                path_wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.words.as_ptr().cast_mut().cast(),
            )
        };
        if applied == 0 {
            return Err(AclError::Apply {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(())
    }

    pub(super) fn apply_descriptor_to_handle(
        file: &File,
        descriptor: &PrivateKeySecurityDescriptor,
    ) -> Result<(), AclError> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut();
        let extracted = unsafe {
            GetSecurityDescriptorDacl(
                descriptor.words.as_ptr().cast_mut().cast(),
                &mut present,
                &mut dacl,
                &mut defaulted,
            )
        };
        if extracted == 0 || present == 0 || dacl.is_null() {
            return Err(AclError::Descriptor(std::io::Error::last_os_error()));
        }
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(AclError::ApplyHandle(std::io::Error::from_raw_os_error(
                status as i32,
            )));
        }
        Ok(())
    }
}
