#[cfg(not(windows))]
use cellar_windows::acl::restrict_private_key_access;
use cellar_windows::acl::{
    PRIVATE_KEY_SERVICE_NAME, build_private_key_security_descriptor, restrict_private_key_handle,
    service_sid_string,
};

#[test]
fn cellar_service_sid_is_derived_without_account_lookup() {
    assert_eq!(
        service_sid_string(PRIVATE_KEY_SERVICE_NAME).unwrap(),
        "S-1-5-80-3653650444-108827001-3922763823-736153100-155111920"
    );
}

#[cfg(not(windows))]
#[test]
fn applying_a_private_key_acl_is_explicitly_unsupported_off_windows() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let error = restrict_private_key_access(temp.path(), PRIVATE_KEY_SERVICE_NAME).unwrap_err();
    assert!(error.to_string().contains("only supported on Windows"));
}

#[cfg(windows)]
mod windows {
    use std::{
        collections::BTreeMap,
        ffi::OsStr,
        fs::OpenOptions,
        iter,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
        ptr,
    };

    use super::*;
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, LocalFree},
        Security::{
            ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
            INHERITED_ACE, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        },
        Storage::FileSystem::READ_CONTROL,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const DELETE: u32 = 0x0001_0000;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
    const FILE_GENERIC_READ: u32 = 0x0012_0089;
    const WRITE_DAC: u32 = 0x0004_0000;

    struct LocalPtr(*mut core::ffi::c_void);

    impl Drop for LocalPtr {
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

    unsafe fn sid_string(sid: *mut core::ffi::c_void) -> String {
        let mut text: *mut u16 = ptr::null_mut();
        assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
        let _text = LocalPtr(text.cast());
        let length = (0..)
            .find(|index| unsafe { *text.add(*index) } == 0)
            .unwrap();
        String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap()
    }

    unsafe fn dacl_entries(acl: *mut ACL) -> BTreeMap<String, (u8, u32)> {
        let mut information = ACL_SIZE_INFORMATION::default();
        assert_ne!(
            unsafe {
                GetAclInformation(
                    acl,
                    (&raw mut information).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            },
            0
        );
        let mut entries = BTreeMap::new();
        for index in 0..information.AceCount {
            let mut ace = ptr::null_mut();
            assert_ne!(unsafe { GetAce(acl, index, &mut ace) }, 0);
            let ace = ace.cast::<u8>();
            let ace_type = unsafe { *ace };
            let flags = unsafe { *ace.add(1) };
            assert_eq!(ace_type, ACCESS_ALLOWED_ACE_TYPE);
            assert_eq!(u32::from(flags) & INHERITED_ACE, 0);
            let mask = unsafe { ptr::read_unaligned(ace.add(4).cast::<u32>()) };
            let sid = unsafe { ace.add(8).cast() };
            entries.insert(unsafe { sid_string(sid) }, (flags, mask));
        }
        entries
    }

    #[test]
    fn descriptor_is_protected_and_allows_only_system_admins_and_cellar() {
        let descriptor = build_private_key_security_descriptor(PRIVATE_KEY_SERVICE_NAME).unwrap();
        assert!(descriptor.sddl().starts_with("D:P"));
        assert!(!descriptor.sddl().contains(";;;WD)"));
        assert!(!descriptor.sddl().contains(";;;BU)"));
        assert!(!descriptor.sddl().contains(";;;AU)"));
    }

    #[test]
    fn handle_applied_dacl_has_no_broad_or_inherited_access() {
        let temp = tempfile::TempDir::new().unwrap();
        let key_path = temp.path().join("key.pem");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
            .open(&key_path)
            .unwrap();
        restrict_private_key_handle(&file, PRIVATE_KEY_SERVICE_NAME).unwrap();

        let path = wide(key_path.as_os_str());
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        let _descriptor = LocalPtr(descriptor);
        let mut control = 0_u16;
        let mut revision = 0_u32;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);

        let entries = unsafe { dacl_entries(dacl) };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries["S-1-5-18"].1, FILE_ALL_ACCESS);
        assert_eq!(entries["S-1-5-32-544"].1, FILE_ALL_ACCESS);
        assert_eq!(
            entries[&service_sid_string(PRIVATE_KEY_SERVICE_NAME).unwrap()].1,
            FILE_GENERIC_READ | DELETE | WRITE_DAC
        );
        assert!(!entries.contains_key("S-1-1-0"));
        assert!(!entries.contains_key("S-1-5-11"));
        assert!(!entries.contains_key("S-1-5-32-545"));
    }
}
