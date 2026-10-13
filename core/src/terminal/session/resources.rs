//! One profile reservation and credential lease shared by independent PTYs.
use super::*;

struct LaunchResources {
    plan: crate::tools::launch::LaunchPlan,
    environment: Environment,
    started: bool,
}
struct State {
    members: usize,
    closed: bool,
    lease: Option<ActivityLease>,
    credentials: Option<ProfileCredentialLease>,
    launches: BTreeMap<bool, LaunchResources>,
}
pub(super) struct Resources {
    root: DataRoot,
    tool: Tool,
    profile: Profile,
    repository: Arc<ProfileRepository>,
    shared: Arc<SharedData>,
    state: Mutex<State>,
}
pub(super) struct Member {
    resources: Option<Arc<Resources>>,
    permission: bool,
    pub arguments: Vec<String>,
}
impl Resources {
    pub fn create(manager: &SessionManager, tool: &Tool, initial: &Profile) -> Result<Arc<Self>> {
        let lease = manager.repository.activities().reserve(
            tool,
            Some(&initial.id),
            &initial.settings,
            ActivityKind::Session,
        )?;
        let profile = manager.repository.get(&tool.id, &initial.id)?;
        if &profile != initial {
            return Err(Error::new(
                ErrorKind::Conflict,
                "Profile changed while reserving it",
            ));
        }
        manager.shared_data.hydrate(tool, &profile)?;
        let credentials = if tool.global_credentials() {
            let vault = manager.vault.clone().ok_or_else(|| {
                Error::new(
                    ErrorKind::Unsupported,
                    "Credential vault is required for this CLI",
                )
            })?;
            Some(ProfileCredentialLease::activate(
                manager.root.clone(),
                tool,
                &profile,
                vault,
            )?)
        } else {
            None
        };
        Ok(Arc::new(Self {
            root: manager.root.clone(),
            tool: tool.clone(),
            profile,
            repository: manager.repository.clone(),
            shared: manager.shared_data.clone(),
            state: Mutex::new(State {
                members: 0,
                closed: false,
                lease: Some(lease),
                credentials,
                launches: BTreeMap::new(),
            }),
        }))
    }
    pub fn join(
        self: &Arc<Self>,
        permission: bool,
        environment: &mut Environment,
    ) -> Result<Option<Member>> {
        let mut state = self.state.lock();
        if state.closed {
            return Ok(None);
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = state.launches.entry(permission)
        {
            let before = environment.clone();
            let plan = crate::tools::launch::LaunchPlan::prepare(
                &self.root,
                &self.tool,
                &self.profile,
                permission,
                environment,
            )?;
            let patch = environment
                .iter()
                .filter(|(key, value)| before.get(*key) != Some(*value))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            entry.insert(LaunchResources {
                plan,
                environment: patch,
                started: false,
            });
        }
        let launch = &state.launches[&permission];
        environment.extend(launch.environment.clone());
        let arguments = launch.plan.arguments.clone();
        state.members += 1;
        Ok(Some(Member {
            resources: Some(self.clone()),
            permission,
            arguments,
        }))
    }
    fn leave(&self) -> Vec<String> {
        let mut state = self.state.lock();
        state.members -= 1;
        if state.members != 0 {
            return Vec::new();
        }
        let started = state.launches.values().any(|launch| launch.started);
        let mut errors = Vec::new();
        for (_, mut launch) in std::mem::take(&mut state.launches) {
            if launch.started {
                if let Err(error) = launch.plan.finish() {
                    errors.push(error.message);
                }
            }
        }
        if let Some(mut credential) = state.credentials.take() {
            if let Err(error) = credential.finish(started) {
                self.repository.require_recovery();
                errors.push(error.message);
            }
        }
        if started {
            if let Err(error) = self.shared.persist(&self.tool, &self.profile) {
                errors.push(error.message);
            }
        }
        state.lease.take();
        state.closed = true;
        errors
    }
}
impl Member {
    pub fn started(&self) {
        if let Some(resources) = &self.resources {
            resources
                .state
                .lock()
                .launches
                .get_mut(&self.permission)
                .expect("prepared launch")
                .started = true;
        }
    }
    pub fn finish(mut self) -> Vec<String> {
        self.resources.take().map(|r| r.leave()).unwrap_or_default()
    }
}
impl Drop for Member {
    fn drop(&mut self) {
        if let Some(resources) = self.resources.take() {
            let _ = resources.leave();
        }
    }
}
impl Drop for Resources {
    fn drop(&mut self) {
        // Preparation can fail before a Member exists. Restore the host vault.
        let state = self.state.get_mut();
        if let Some(mut credential) = state.credentials.take() {
            if credential.finish(false).is_err() {
                self.repository.require_recovery();
            }
        }
    }
}
