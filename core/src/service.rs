//! High-level unified backend service interface.
//!
//! Provides a unified, UI-independent facade coordinating profile repository,
//! credential vault, shared data synchronization, and profile-aware terminal
//! session lifecycle management.

use std::{path::PathBuf, sync::Arc, time::Duration};

use uuid::Uuid;

use crate::{
    Result,
    domain::{ApplicationIdentity, Profile, ProfileId, ProfileSettings},
    profiles::credentials::CredentialVault,
    profiles::sharing::SharedData,
    profiles::{Mutation, ProfileRepository},
    storage::DataRoot,
    tools::catalog::Catalog,
};

#[cfg(not(windows))]
use crate::profiles::credentials::MemoryVault;

#[cfg(windows)]
use crate::{
    profiles::credentials::WindowsVault,
    terminal::pty::ExitSummary,
    terminal::session::{Session, SessionManager, SessionStartRequest, SessionSummary},
};

/// High-level unified facade for the backend infrastructure.
pub struct BackendService {
    root: DataRoot,
    identity: ApplicationIdentity,
    catalog: Catalog,
    profiles: Arc<ProfileRepository>,
    shared_data: Arc<SharedData>,
    vault: Arc<dyn CredentialVault>,
    #[cfg(windows)]
    sessions: Arc<SessionManager>,
}

impl BackendService {
    /// Opens the backend service using standard platform defaults.
    /// On Windows, this connects to the Windows Credential Manager
    /// Profile leases save isolated records and restore the host credential.
    pub fn open(root: DataRoot, identity: ApplicationIdentity) -> Result<Self> {
        identity.validate()?;
        #[cfg(windows)]
        let vault: Arc<dyn CredentialVault> = Arc::new(WindowsVault::new());
        #[cfg(not(windows))]
        let vault: Arc<dyn CredentialVault> = Arc::new(MemoryVault::new());

        Self::open_with_vault(root, identity, vault)
    }

    /// Opens the backend service with a custom credential vault (e.g. for testing with `MemoryVault`).
    pub fn open_with_vault(
        root: DataRoot,
        identity: ApplicationIdentity,
        vault: Arc<dyn CredentialVault>,
    ) -> Result<Self> {
        identity.validate()?;
        let catalog = Catalog::built_in();
        let profiles = Arc::new(ProfileRepository::open(root.clone(), catalog.clone())?);
        #[cfg(windows)]
        crate::profiles::credentials::recover_profile_credentials(&root, vault.as_ref())?;
        let shared_data = Arc::new(SharedData::new(root.clone()));

        #[cfg(windows)]
        let sessions = Arc::new(SessionManager::new(
            root.clone(),
            catalog.clone(),
            profiles.clone(),
            shared_data.clone(),
            identity.clone(),
            Some(vault.clone()),
        ));

        Ok(Self {
            root,
            identity,
            catalog,
            profiles,
            shared_data,
            vault,
            #[cfg(windows)]
            sessions,
        })
    }

    pub fn root(&self) -> &DataRoot {
        &self.root
    }

    pub fn identity(&self) -> &ApplicationIdentity {
        &self.identity
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn profiles(&self) -> Arc<ProfileRepository> {
        self.profiles.clone()
    }

    pub fn shared_data(&self) -> Arc<SharedData> {
        self.shared_data.clone()
    }

    pub fn vault(&self) -> Arc<dyn CredentialVault> {
        self.vault.clone()
    }

    #[cfg(windows)]
    pub fn sessions(&self) -> Arc<SessionManager> {
        self.sessions.clone()
    }

    // Profile convenience methods
    pub fn list_profiles(&self, tool_id: &str) -> Result<Vec<Profile>> {
        self.profiles.list(tool_id)
    }

    pub fn get_profile(&self, tool_id: &str, id: &ProfileId) -> Result<Profile> {
        self.profiles.get(tool_id, id)
    }

    pub fn create_profile(
        &self,
        tool_id: &str,
        name: &str,
        settings: ProfileSettings,
    ) -> Result<Mutation<Profile>> {
        self.profiles.create(tool_id, name, settings)
    }

    pub fn rename_profile(
        &self,
        tool_id: &str,
        id: &ProfileId,
        new_name: &str,
    ) -> Result<Mutation<Profile>> {
        self.profiles.rename(tool_id, id, new_name)
    }

    pub fn delete_profile(&self, tool_id: &str, id: &ProfileId) -> Result<Mutation<PathBuf>> {
        self.profiles.delete(tool_id, id)
    }

    // Sharing convenience methods
    pub fn hydrate_sharing(&self, tool_id: &str, profile_id: &ProfileId) -> Result<()> {
        let tool = self.catalog.get(tool_id)?;
        let profile = self.profiles.get(tool_id, profile_id)?;
        let _lease = self.profiles.activities().reserve(
            tool,
            Some(profile_id),
            &profile.settings,
            crate::profiles::activity::ActivityKind::Inspect,
        )?;
        if profile != self.profiles.get(tool_id, profile_id)? {
            return Err(crate::Error::new(
                crate::ErrorKind::Conflict,
                "Profile changed during sharing preparation",
            ));
        }
        self.shared_data.hydrate(tool, &profile)
    }

    pub fn persist_sharing(&self, tool_id: &str, profile_id: &ProfileId) -> Result<()> {
        let tool = self.catalog.get(tool_id)?;
        let profile = self.profiles.get(tool_id, profile_id)?;
        let _lease = self.profiles.activities().reserve(
            tool,
            Some(profile_id),
            &profile.settings,
            crate::profiles::activity::ActivityKind::Inspect,
        )?;
        if profile != self.profiles.get(tool_id, profile_id)? {
            return Err(crate::Error::new(
                crate::ErrorKind::Conflict,
                "Profile changed during sharing preparation",
            ));
        }
        self.shared_data.persist(tool, &profile)
    }

    // Session convenience methods
    #[cfg(windows)]
    pub fn start_session(&self, request: SessionStartRequest) -> Result<Arc<Session>> {
        self.sessions.start(request)
    }

    #[cfg(windows)]
    pub fn get_session(&self, id: &Uuid) -> Option<Arc<Session>> {
        self.sessions.get(id)
    }

    #[cfg(windows)]
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.sessions.list()
    }

    #[cfg(windows)]
    pub fn stop_session(&self, id: &Uuid, grace: Duration) -> Result<ExitSummary> {
        self.sessions.stop(id, grace)
    }

    /// Gracefully closes all active sessions.
    pub fn shutdown(&self, grace: Duration) -> Vec<crate::Error> {
        #[cfg(windows)]
        {
            self.sessions.close_all(grace)
        }
        #[cfg(not(windows))]
        {
            let _ = grace;
            Vec::new()
        }
    }
}
