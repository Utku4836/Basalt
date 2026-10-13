use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, ErrorKind, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplicationIdentity {
    pub display_name: String,
    pub application_id: String,
    pub version: String,
}

impl ApplicationIdentity {
    pub fn validate(&self) -> Result<()> {
        if self.display_name.trim().is_empty()
            || self.display_name.chars().any(char::is_control)
            || self.application_id.is_empty()
            || !self
                .application_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
            || self.version.is_empty()
            || self.version.chars().any(char::is_control)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Invalid application identity",
            ));
        }
        Ok(())
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let identity: Self = toml::from_str(text).map_err(Error::corrupt)?;
        identity.validate()?;
        Ok(identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProfileId(String);

impl ProfileId {
    pub fn default_profile() -> Self {
        Self("default".into())
    }

    pub fn new_unique() -> Self {
        Self(format!("p_{}", Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == "default"
    }
}

impl TryFrom<String> for ProfileId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        if value == "default"
            || (value.len() == 34
                && value.starts_with("p_")
                && value[2..]
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
        {
            Ok(Self(value))
        } else {
            Err(Error::new(
                ErrorKind::InvalidInput,
                "Invalid profile identifier",
            ))
        }
    }
}

impl From<ProfileId> for String {
    fn from(id: ProfileId) -> Self {
        id.0
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProfileSettings {
    pub full_permission: bool,
    pub shared_sessions: bool,
    pub shared_models: bool,
    pub shared_config: bool,
    pub shared_skills: bool,
    pub shared_mcp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShareCategory {
    SharedSessions,
    SharedModels,
    SharedConfig,
    SharedSkills,
    SharedMcp,
}

impl ShareCategory {
    pub const ALL: [Self; 5] = [
        Self::SharedSessions,
        Self::SharedModels,
        Self::SharedConfig,
        Self::SharedSkills,
        Self::SharedMcp,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::SharedSessions => "sharedSessions",
            Self::SharedModels => "sharedModels",
            Self::SharedConfig => "sharedConfig",
            Self::SharedSkills => "sharedSkills",
            Self::SharedMcp => "sharedMcp",
        }
    }
}

impl ProfileSettings {
    pub fn shared(&self, category: ShareCategory) -> bool {
        match category {
            ShareCategory::SharedSessions => self.shared_sessions,
            ShareCategory::SharedModels => self.shared_models,
            ShareCategory::SharedConfig => self.shared_config,
            ShareCategory::SharedSkills => self.shared_skills,
            ShareCategory::SharedMcp => self.shared_mcp,
        }
    }

    pub fn conflicts(&self, other: &Self) -> bool {
        ShareCategory::ALL
            .into_iter()
            .any(|category| self.shared(category) && other.shared(category))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub settings: ProfileSettings,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

impl Profile {
    pub fn default_at(timestamp: &str) -> Self {
        Self {
            id: ProfileId::default_profile(),
            name: "Default".into(),
            settings: ProfileSettings::default(),
            created_at: timestamp.into(),
            updated_at: timestamp.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub layout_version: u32,
    #[serde(default)]
    pub shared_install_version: u32,
    pub tools: BTreeMap<String, Vec<Profile>>,
}

impl Default for ProfileIndex {
    fn default() -> Self {
        Self {
            schema_version: 2,
            layout_version: 2,
            shared_install_version: 1,
            tools: BTreeMap::new(),
        }
    }
}

pub fn profile_name(value: &str) -> Result<String> {
    let name = value.split_whitespace().collect::<Vec<_>>().join(" ");
    windows_component(&name, 40)?;
    Ok(name)
}

/// Validate without normalizing: archive/artifact names must keep their identity.
pub fn windows_component(name: &str, max_utf16: usize) -> Result<()> {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.chars().count() == 1
                && suffix
                    .chars()
                    .all(|c| matches!(c, '1'..='9' | '¹' | '²' | '³'))
        })
    });
    if name.is_empty()
        || name.encode_utf16().count() > max_utf16
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || reserved
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Name must be a valid Windows component of 1–{max_utf16} UTF-16 units"),
        ));
    }
    Ok(())
}

pub fn same_windows_name(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        let left: Vec<_> = left.encode_utf16().collect();
        let right: Vec<_> = right.encode_utf16().collect();
        // Both buffers remain alive for the comparison; lengths are bounded by profile validation.
        unsafe {
            windows_sys::Win32::Globalization::CompareStringOrdinal(
                left.as_ptr(),
                left.len() as i32,
                right.as_ptr(),
                right.len() as i32,
                1,
            ) == 2
        }
    }
    #[cfg(not(windows))]
    {
        left.to_lowercase() == right.to_lowercase()
    }
}
