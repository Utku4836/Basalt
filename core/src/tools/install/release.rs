use super::download::{Checksum, Http, release_url};
use crate::{
    Error, ErrorKind, Result, domain::windows_component, system::process::CancellationToken,
};
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub checksum: Option<Checksum>,
    pub size: Option<u64>,
}

#[derive(Deserialize)]
struct AgyManifest {
    version: String,
    url: String,
    sha512: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    assets: Vec<GithubAsset>,
}
#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct PypiRelease {
    info: PypiInfo,
}
#[derive(Deserialize)]
struct PypiInfo {
    version: String,
}

impl Http {
    pub async fn aider_release(&self, cancel: &CancellationToken) -> Result<Release> {
        let version = self
            .json::<PypiRelease>("https://pypi.org/pypi/aider-chat/json", cancel)
            .await?
            .info
            .version;
        version_name(&version)?;
        let asset = match Architecture::current() {
            Architecture::X64 => "uv-x86_64-pc-windows-msvc.zip",
            Architecture::Arm64 => "uv-aarch64-pc-windows-msvc.zip",
        };
        let mut release = self
            .github_release(
                "https://api.github.com/repos/astral-sh/uv/releases/latest",
                asset,
                cancel,
            )
            .await?;
        release.version = version;
        Ok(release)
    }
    pub async fn agy_release(&self, manifest: &str, cancel: &CancellationToken) -> Result<Release> {
        let manifest: AgyManifest = self.json(manifest, cancel).await?;
        version_name(&manifest.version)?;
        release_url(&manifest.url)?;
        if manifest.sha512.len() != 128
            || !manifest.sha512.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Release manifest has an invalid SHA512",
            ));
        }
        Ok(Release {
            version: manifest.version,
            url: manifest.url,
            checksum: Some(Checksum::Sha512(manifest.sha512)),
            size: None,
        })
    }

    pub async fn github_release(
        &self,
        metadata_url: &str,
        asset_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Release> {
        windows_component(asset_name, 255)?;
        let release: GithubRelease = self.json(metadata_url, cancel).await?;
        if release.draft {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Draft release cannot be installed",
            ));
        }
        let version = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            .to_owned();
        version_name(&version)?;
        let mut matching = release
            .assets
            .into_iter()
            .filter(|asset| asset.name == asset_name);
        let asset = matching.next().ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                "Release does not contain the requested artifact",
            )
        })?;
        if matching.next().is_some() || asset.size == 0 {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Release artifact is ambiguous or empty",
            ));
        }
        release_url(&asset.browser_download_url)?;
        let checksum = match asset.digest {
            Some(digest) => {
                let value = digest.strip_prefix("sha256:").ok_or_else(|| {
                    Error::new(ErrorKind::CorruptData, "Unsupported release digest")
                })?;
                if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(Error::new(ErrorKind::CorruptData, "Invalid release digest"));
                }
                Some(Checksum::Sha256(value.to_owned()))
            }
            None => None,
        };
        Ok(Release {
            version,
            url: asset.browser_download_url,
            checksum,
            size: Some(asset.size),
        })
    }

    pub async fn cursor_release(
        &self,
        bootstrap: &str,
        architecture: Architecture,
        cancel: &CancellationToken,
    ) -> Result<Release> {
        let bytes = self.bytes(bootstrap, 2 * 1024 * 1024, cancel).await?;
        let script = std::str::from_utf8(&bytes).map_err(Error::corrupt)?;
        // Read declarative values only; never evaluate or run the bootstrap.
        let prefix = bootstrap_value(script, "downloadUrl")?;
        let version = bootstrap_value(script, "version")?;
        version_name(&version)?;
        let mut url = release_url(&prefix)?;
        if url.query().is_some() {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Cursor download prefix has unexpected query parameters",
            ));
        }
        let path = format!(
            "{}windows/{}/agent-cli-package.zip",
            url.path().trim_end_matches('/').to_owned() + "/",
            architecture.name()
        );
        url.set_path(&path);
        Ok(Release {
            version,
            url: url.to_string(),
            checksum: None,
            size: None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Architecture {
    X64,
    Arm64,
}
impl Architecture {
    pub fn current() -> Self {
        if cfg!(target_arch = "aarch64") {
            Self::Arm64
        } else {
            Self::X64
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

pub fn version_name(value: &str) -> Result<()> {
    windows_component(value, 128)?;
    if value.trim() != value || value.contains(['$', '{', '}', ';']) {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Invalid release version identifier",
        ));
    }
    Ok(())
}
fn bootstrap_value(script: &str, name: &str) -> Result<String> {
    let pattern = format!(
        r#"(?m)^\s*\${}\s*=\s*['"]([^'"\r\n]+)['"]\s*(?:#.*)?$"#,
        regex::escape(name)
    );
    let regex = Regex::new(&pattern).map_err(Error::corrupt)?;
    let mut matches = regex.captures_iter(script);
    let value = matches.next().ok_or_else(|| {
        Error::new(
            ErrorKind::CorruptData,
            "Cursor bootstrap did not expose a static release value",
        )
    })?[1]
        .to_owned();
    if matches.next().is_some() {
        return Err(Error::new(
            ErrorKind::CorruptData,
            "Cursor bootstrap has ambiguous release values",
        ));
    }
    Ok(value)
}
