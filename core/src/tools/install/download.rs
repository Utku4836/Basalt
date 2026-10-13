pub use reqwest::Certificate;
use reqwest::{Client, Url};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256, Sha512};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

use crate::{
    Error, ErrorKind, Result, domain::ApplicationIdentity, storage::DataRoot,
    system::process::CancellationToken,
};

#[derive(Clone)]
pub struct Http {
    client: Client,
}

#[derive(Debug, Clone)]
pub enum Checksum {
    Sha256(String),
    Sha512(String),
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub bytes: u64,
    pub sha256: String,
    pub sha512: String,
    pub verified: bool,
}

impl Http {
    /// Extra trusted roots support explicit enterprise/verification certificates;
    /// verification never disables HTTPS, hostname or certificate validation.
    pub fn new(
        identity: &ApplicationIdentity,
        extra_roots: &[Certificate],
        timeout: Duration,
    ) -> Result<Self> {
        identity.validate()?;
        if timeout.is_zero() || timeout > Duration::from_secs(3600) {
            return Err(Error::new(ErrorKind::InvalidInput, "Invalid HTTP timeout"));
        }
        let mut builder = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .timeout(timeout)
            .user_agent(format!("{}/{}", identity.application_id, identity.version));
        for certificate in extra_roots {
            builder = builder.add_root_certificate(certificate.clone());
        }
        Ok(Self {
            client: builder.build().map_err(network_error)?,
        })
    }

    async fn request(&self, uri: &str, cancel: &CancellationToken) -> Result<reqwest::Response> {
        let url = release_url(uri)?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(cancelled()),
            response = self.client.get(url).send() => response.map_err(network_error)?.error_for_status().map_err(network_error),
        }
    }

    pub async fn bytes(
        &self,
        uri: &str,
        maximum: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        if maximum == 0 || maximum > 16 * 1024 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Invalid release metadata limit",
            ));
        }
        let mut response = self.request(uri, cancel).await?;
        if response
            .content_length()
            .is_some_and(|size| size > maximum as u64)
        {
            return Err(Error::new(
                ErrorKind::Network,
                "Release metadata exceeds its size limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = next_chunk(&mut response, cancel).await? {
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > maximum)
            {
                return Err(Error::new(
                    ErrorKind::Network,
                    "Release metadata exceeds its size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    pub async fn json<T: DeserializeOwned>(
        &self,
        uri: &str,
        cancel: &CancellationToken,
    ) -> Result<T> {
        serde_json::from_slice(&self.bytes(uri, 8 * 1024 * 1024, cancel).await?)
            .map_err(Error::corrupt)
    }

    /// Destination must be an unused job-owned scratch file. Its caller controls
    /// later activation; a completed download is never an installed executable.
    pub async fn download(
        &self,
        root: &DataRoot,
        uri: &str,
        destination: &Path,
        maximum: u64,
        checksum: Option<&Checksum>,
        cancel: &CancellationToken,
    ) -> Result<Receipt> {
        if maximum == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Invalid download limit",
            ));
        }
        if let Some(checksum) = checksum {
            checksum.validate()?;
        }
        let path = root.resolve(destination)?;
        if path.exists() {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Download destination already exists",
            ));
        }
        let mut response = self.request(uri, cancel).await?;
        let expected_length = response.content_length();
        if expected_length.is_some_and(|size| size > maximum) {
            return Err(Error::new(
                ErrorKind::Network,
                "Download exceeds its size limit",
            ));
        }
        let created = root.create_new_file(destination)?;
        let mut file = tokio::fs::File::from_std(created);
        let result = (async {
            let mut bytes = 0u64; let mut sha256 = Sha256::new(); let mut sha512 = Sha512::new();
            while let Some(chunk) = next_chunk(&mut response, cancel).await? {
                bytes = bytes.checked_add(chunk.len() as u64).ok_or_else(|| Error::new(ErrorKind::Network, "Download size overflow"))?;
                if bytes > maximum { return Err(Error::new(ErrorKind::Network, "Download exceeds its size limit")); }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(cancelled()),
                    written = file.write_all(&chunk) => written.map_err(|error| Error::io("Write download", &path, error))?,
                }
                sha256.update(&chunk); sha512.update(&chunk);
            }
            if cancel.is_cancelled() { return Err(cancelled()); }
            if bytes == 0 || expected_length.is_some_and(|expected| expected != bytes) { return Err(Error::new(ErrorKind::Integrity, "Download is empty or incomplete")); }
            file.flush().await.map_err(|error| Error::io("Flush download", &path, error))?;
            file.sync_all().await.map_err(|error| Error::io("Sync download", &path, error))?;
            let receipt = Receipt { bytes, sha256: format!("{:x}",sha256.finalize()), sha512: format!("{:x}",sha512.finalize()), verified: checksum.is_some() };
            if checksum.is_some_and(|expected| !expected.matches(&receipt)) { return Err(Error::new(ErrorKind::Integrity, "Release artifact checksum does not match")); }
            Ok(receipt)
        }).await;
        drop(file);
        match result {
            Ok(receipt) => Ok(receipt),
            Err(primary) => match root.remove_file(destination) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(Error::new(
                    ErrorKind::Recovery,
                    format!("{primary}; download cleanup: {cleanup}"),
                )),
            },
        }
    }
}

impl Checksum {
    fn validate(&self) -> Result<()> {
        let (value, length) = match self {
            Self::Sha256(value) => (value, 64),
            Self::Sha512(value) => (value, 128),
        };
        if value.len() != length || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Invalid release checksum",
            ));
        }
        Ok(())
    }
    fn matches(&self, receipt: &Receipt) -> bool {
        match self {
            Self::Sha256(value) => value.eq_ignore_ascii_case(&receipt.sha256),
            Self::Sha512(value) => value.eq_ignore_ascii_case(&receipt.sha512),
        }
    }
}

pub fn release_url(value: &str) -> Result<Url> {
    if value.len() > 8192 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Release URL is too long",
        ));
    }
    let url = Url::parse(value)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid release URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Release URL must use HTTPS without embedded credentials or fragments",
        ));
    }
    Ok(url)
}

async fn next_chunk(
    response: &mut reqwest::Response,
    cancel: &CancellationToken,
) -> Result<Option<bytes::Bytes>> {
    tokio::select! { biased; _=cancel.cancelled()=>Err(cancelled()), chunk=response.chunk()=>chunk.map_err(network_error) }
}
fn cancelled() -> Error {
    Error::new(ErrorKind::Cancelled, "Release download cancelled")
}
fn network_error(error: reqwest::Error) -> Error {
    let message = if error.is_timeout() {
        "HTTP request timed out".to_owned()
    } else {
        format!("HTTP request failed: {}", error.without_url())
    };
    Error::new(ErrorKind::Network, message)
}
