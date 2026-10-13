//! Windows Credential Manager and DPAPI vault in pure Rust.

use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};

use crate::{Error, ErrorKind, Result};

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Credential {
    pub target: String,
    pub user: String,
    pub secret: Vec<u8>,
    #[serde(default)]
    pub persistence: CredentialPersistence,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialPersistence {
    Session,
    #[default]
    LocalMachine,
    Enterprise,
    Other(u32),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[cfg(windows)]
pub(crate) struct ProfileCredentialLease {
    root: crate::storage::DataRoot,
    file: std::path::PathBuf,
    vault: Arc<dyn CredentialVault>,
    before: Option<Credential>,
    restored: bool,
}

#[cfg(windows)]
impl ProfileCredentialLease {
    const TARGET: &'static str = "gemini:antigravity";
    const JOURNAL: &'static str = "_profiles/credential-lease.dpapi";
    pub(crate) fn activate(
        root: crate::storage::DataRoot,
        tool: &crate::tools::catalog::Tool,
        profile: &crate::domain::Profile,
        vault: Arc<dyn CredentialVault>,
    ) -> Result<Self> {
        let file = tool.profile_relative(profile).join(".credentials.dpapi");
        let desired: Option<Credential> = match root.read_limited(&file, 128 * 1024)? {
            None => None,
            Some(bytes) => {
                serde_json::from_slice(&dpapi_unprotect(&bytes, None)?).map_err(Error::corrupt)?
            }
        };
        if desired
            .as_ref()
            .is_some_and(|credential| credential.target != Self::TARGET)
        {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Profile credential target is invalid",
            ));
        }
        let before = vault.read(Self::TARGET)?;
        if root.resolve(Self::JOURNAL)?.exists() {
            return Err(Error::new(
                ErrorKind::Recovery,
                "An unfinished Windows credential lease needs recovery",
            ));
        }
        root.write(
            Self::JOURNAL,
            &dpapi_protect(&serde_json::to_vec(&before).map_err(Error::corrupt)?, None)?,
        )?;
        let mut lease = Self {
            root,
            file,
            vault,
            before,
            restored: false,
        };
        if let Err(error) = lease.apply(desired.as_ref()) {
            lease.finish(false)?;
            return Err(error);
        }
        Ok(lease)
    }
    fn apply(&self, credential: Option<&Credential>) -> Result<()> {
        match credential {
            Some(credential) => self.vault.write_record(credential),
            None => self.vault.delete(Self::TARGET).map(|_| ()),
        }
    }
    pub(crate) fn finish(&mut self, save: bool) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let saved = if save {
            (|| {
                let current = self.vault.read(Self::TARGET)?;
                let bytes = serde_json::to_vec(&current).map_err(Error::corrupt)?;
                let protected = dpapi_protect(&bytes, None)?;
                self.root.write(&self.file, &protected)
            })()
        } else {
            Ok(())
        };
        let restored = self.apply(self.before.as_ref());
        self.restored = restored.is_ok();
        if self.restored {
            self.root.remove_file(Self::JOURNAL)?;
        }
        match (saved, restored) {
            (_, Err(error)) => Err(Error::new(
                ErrorKind::Recovery,
                format!("Restore Windows credential: {error}"),
            )),
            (Err(error), _) => Err(error),
            _ => Ok(()),
        }
    }
}
#[cfg(windows)]
impl Drop for ProfileCredentialLease {
    fn drop(&mut self) {
        let _ = self.finish(false);
    }
}

#[cfg(windows)]
pub(crate) fn recover_profile_credentials(
    root: &crate::storage::DataRoot,
    vault: &dyn CredentialVault,
) -> Result<()> {
    let Some(bytes) = root.read_limited(ProfileCredentialLease::JOURNAL, 128 * 1024)? else {
        return Ok(());
    };
    let before: Option<Credential> =
        serde_json::from_slice(&dpapi_unprotect(&bytes, None)?).map_err(Error::corrupt)?;
    match before {
        Some(credential) if credential.target == ProfileCredentialLease::TARGET => {
            vault.write_record(&credential)?
        }
        None => {
            vault.delete(ProfileCredentialLease::TARGET)?;
        }
        _ => {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Credential recovery target is invalid",
            ));
        }
    }
    root.remove_file(ProfileCredentialLease::JOURNAL)
}

impl Credential {
    pub fn new(
        target: impl Into<String>,
        user: impl Into<String>,
        secret: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            target: target.into(),
            user: user.into(),
            secret: secret.into(),
            persistence: CredentialPersistence::default(),
        }
    }
}

pub trait CredentialVault: Send + Sync {
    fn read(&self, target: &str) -> Result<Option<Credential>>;
    fn write(&self, target: &str, user: &str, secret: &[u8]) -> Result<()>;
    fn write_record(&self, credential: &Credential) -> Result<()> {
        self.write(&credential.target, &credential.user, &credential.secret)
    }
    fn delete(&self, target: &str) -> Result<bool>;
}

fn validate_target(target: &str) -> Result<()> {
    if target.is_empty() || target.contains('\0') || target.encode_utf16().count() > 32767 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid credential target",
        ));
    }
    Ok(())
}
fn validate_record(target: &str, user: &str, secret: &[u8]) -> Result<()> {
    validate_target(target)?;
    if user.contains('\0') || user.encode_utf16().count() > 513 || secret.len() > 2560 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid credential record",
        ));
    }
    Ok(())
}

/// In-memory fake vault for testing and non-Windows environments.
#[derive(Default, Clone)]
pub struct MemoryVault {
    entries: Arc<Mutex<BTreeMap<String, Credential>>>,
}

impl MemoryVault {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.entries.lock().len()
    }
}

impl CredentialVault for MemoryVault {
    fn read(&self, target: &str) -> Result<Option<Credential>> {
        validate_target(target)?;
        if target.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Target cannot be empty",
            ));
        }
        Ok(self.entries.lock().get(target).cloned())
    }

    fn write(&self, target: &str, user: &str, secret: &[u8]) -> Result<()> {
        validate_record(target, user, secret)?;
        if target.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Target cannot be empty",
            ));
        }
        self.entries.lock().insert(
            target.to_string(),
            Credential {
                target: target.to_string(),
                user: user.to_string(),
                secret: secret.to_vec(),
                persistence: CredentialPersistence::default(),
            },
        );
        Ok(())
    }

    fn write_record(&self, credential: &Credential) -> Result<()> {
        validate_record(&credential.target, &credential.user, &credential.secret)?;
        if credential.target.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Target cannot be empty",
            ));
        }
        self.entries
            .lock()
            .insert(credential.target.clone(), credential.clone());
        Ok(())
    }

    fn delete(&self, target: &str) -> Result<bool> {
        validate_target(target)?;
        if target.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Target cannot be empty",
            ));
        }
        Ok(self.entries.lock().remove(target).is_some())
    }
}

#[cfg(windows)]
pub use windows::{WindowsVault, dpapi_protect, dpapi_unprotect};

#[cfg(windows)]
pub mod windows {
    use super::*;
    use std::{ffi::OsStr, iter::once, os::windows::ffi::OsStrExt, ptr, slice};

    use windows_sys::Win32::{
        Foundation::{ERROR_NOT_FOUND, GetLastError, LocalFree},
        Security::Credentials::{
            CRED_PERSIST_ENTERPRISE, CRED_PERSIST_LOCAL_MACHINE, CRED_PERSIST_SESSION,
            CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
        },
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(once(0)).collect()
    }

    fn from_wide_ptr(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0;
        unsafe {
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = slice::from_raw_parts(ptr, len);
            String::from_utf16_lossy(slice)
        }
    }

    /// Windows Credential Manager implementation (`advapi32`).
    #[derive(Default, Clone)]
    pub struct WindowsVault;

    impl WindowsVault {
        pub fn new() -> Self {
            Self
        }
    }

    impl CredentialVault for WindowsVault {
        fn read(&self, target: &str) -> Result<Option<Credential>> {
            validate_target(target)?;
            if target.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Target cannot be empty",
                ));
            }
            let target_w = to_wide(target);
            let mut cred_ptr: *mut CREDENTIALW = ptr::null_mut();

            let success =
                unsafe { CredReadW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred_ptr) };

            if success == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_NOT_FOUND {
                    return Ok(None);
                }
                return Err(Error::new(
                    ErrorKind::Process,
                    format!("CredReadW failed for '{target}' with error {err}"),
                ));
            }

            struct CredGuard(*mut CREDENTIALW);
            impl Drop for CredGuard {
                fn drop(&mut self) {
                    if !self.0.is_null() {
                        unsafe { CredFree(self.0.cast()) };
                    }
                }
            }
            let _guard = CredGuard(cred_ptr);

            let cred = unsafe { &*cred_ptr };
            let user = from_wide_ptr(cred.UserName);
            let persistence = CredentialPersistence::from_windows(cred.Persist);
            let secret = if cred.CredentialBlobSize > 0 && !cred.CredentialBlob.is_null() {
                unsafe {
                    slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
                        .to_vec()
                }
            } else {
                Vec::new()
            };

            Ok(Some(Credential {
                target: target.to_string(),
                user,
                secret,
                persistence,
            }))
        }

        fn write(&self, target: &str, user: &str, secret: &[u8]) -> Result<()> {
            self.write_record(&Credential::new(target, user, secret))
        }

        fn write_record(&self, credential: &Credential) -> Result<()> {
            validate_record(&credential.target, &credential.user, &credential.secret)?;
            if credential.target.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Target cannot be empty",
                ));
            }
            let mut target_w = to_wide(&credential.target);
            let mut user_w = to_wide(&credential.user);

            let cred = CREDENTIALW {
                Flags: 0,
                Type: CRED_TYPE_GENERIC,
                TargetName: target_w.as_mut_ptr(),
                Comment: ptr::null_mut(),
                LastWritten: windows_sys::Win32::Foundation::FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                },
                CredentialBlobSize: credential.secret.len() as u32,
                CredentialBlob: credential.secret.as_ptr() as *mut u8,
                Persist: credential.persistence.to_windows(),
                AttributeCount: 0,
                Attributes: ptr::null_mut(),
                TargetAlias: ptr::null_mut(),
                UserName: user_w.as_mut_ptr(),
            };

            let success = unsafe { CredWriteW(&cred, 0) };
            if success == 0 {
                let err = unsafe { GetLastError() };
                return Err(Error::new(
                    ErrorKind::Process,
                    format!(
                        "CredWriteW failed for '{}' with error {err}",
                        credential.target
                    ),
                ));
            }

            Ok(())
        }

        fn delete(&self, target: &str) -> Result<bool> {
            validate_target(target)?;
            if target.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Target cannot be empty",
                ));
            }
            let target_w = to_wide(target);

            let success = unsafe { CredDeleteW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0) };
            if success == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_NOT_FOUND {
                    return Ok(false);
                }
                return Err(Error::new(
                    ErrorKind::Process,
                    format!("CredDeleteW failed for '{target}' with error {err}"),
                ));
            }

            Ok(true)
        }
    }

    impl CredentialPersistence {
        fn from_windows(value: u32) -> Self {
            match value {
                CRED_PERSIST_SESSION => Self::Session,
                CRED_PERSIST_LOCAL_MACHINE => Self::LocalMachine,
                CRED_PERSIST_ENTERPRISE => Self::Enterprise,
                value => Self::Other(value),
            }
        }

        fn to_windows(self) -> u32 {
            match self {
                Self::Session => CRED_PERSIST_SESSION,
                Self::LocalMachine => CRED_PERSIST_LOCAL_MACHINE,
                Self::Enterprise => CRED_PERSIST_ENTERPRISE,
                Self::Other(value) => value,
            }
        }
    }

    /// Protect data with Windows DPAPI (`CryptProtectData`).
    pub fn dpapi_protect(data: &[u8], optional_entropy: Option<&[u8]>) -> Result<Vec<u8>> {
        validate_blob(data, optional_entropy)?;
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };

        let mut entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        let entropy_ptr = if let Some(entropy) = optional_entropy {
            entropy_blob.cbData = entropy.len() as u32;
            entropy_blob.pbData = entropy.as_ptr() as *mut u8;
            &mut entropy_blob as *mut CRYPT_INTEGER_BLOB
        } else {
            ptr::null_mut()
        };

        let mut data_out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };

        let success = unsafe {
            CryptProtectData(
                &data_in,
                ptr::null(),
                entropy_ptr,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut data_out,
            )
        };

        if success == 0 {
            let err = unsafe { GetLastError() };
            return Err(Error::new(
                ErrorKind::Process,
                format!("CryptProtectData failed with error {err}"),
            ));
        }

        let result = if data_out.cbData == 0 {
            Vec::new()
        } else {
            unsafe { slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec() }
        };
        unsafe { LocalFree(data_out.pbData.cast()) };
        Ok(result)
    }

    /// Unprotect data with Windows DPAPI (`CryptUnprotectData`).
    pub fn dpapi_unprotect(data: &[u8], optional_entropy: Option<&[u8]>) -> Result<Vec<u8>> {
        validate_blob(data, optional_entropy)?;
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };

        let mut entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        let entropy_ptr = if let Some(entropy) = optional_entropy {
            entropy_blob.cbData = entropy.len() as u32;
            entropy_blob.pbData = entropy.as_ptr() as *mut u8;
            &mut entropy_blob as *mut CRYPT_INTEGER_BLOB
        } else {
            ptr::null_mut()
        };

        let mut data_out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };

        let success = unsafe {
            CryptUnprotectData(
                &data_in,
                ptr::null_mut(),
                entropy_ptr,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut data_out,
            )
        };

        if success == 0 {
            let err = unsafe { GetLastError() };
            return Err(Error::new(
                ErrorKind::Process,
                format!("CryptUnprotectData failed with error {err}"),
            ));
        }

        let result = if data_out.cbData == 0 {
            Vec::new()
        } else {
            unsafe { slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec() }
        };
        unsafe { LocalFree(data_out.pbData.cast()) };
        Ok(result)
    }

    fn validate_blob(data: &[u8], entropy: Option<&[u8]>) -> Result<()> {
        if data.len() > 16 * 1024 * 1024
            || entropy.is_some_and(|value| value.len() > 16 * 1024 * 1024)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "DPAPI payload is too large",
            ));
        }
        Ok(())
    }
}
