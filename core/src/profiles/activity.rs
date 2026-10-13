use std::{collections::BTreeMap, sync::Arc};

use parking_lot::Mutex;
use uuid::Uuid;

use crate::{
    Error, ErrorKind, Result,
    domain::{ProfileId, ProfileSettings},
    tools::catalog::Tool,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Edit,
    Rename,
    Install,
    Inspect,
    Session,
}

#[derive(Debug, Clone)]
struct Request {
    tool_id: String,
    profile_id: Option<ProfileId>,
    settings: ProfileSettings,
    kind: ActivityKind,
    global_credentials: bool,
}

#[derive(Clone, Default)]
pub struct ActivityBook {
    requests: Arc<Mutex<BTreeMap<Uuid, Request>>>,
}

pub struct ActivityLease {
    id: Uuid,
    book: ActivityBook,
}

impl ActivityBook {
    pub fn reserve(
        &self,
        tool: &Tool,
        profile_id: Option<&ProfileId>,
        settings: &ProfileSettings,
        kind: ActivityKind,
    ) -> Result<ActivityLease> {
        let request = Request {
            tool_id: tool.id.clone(),
            profile_id: profile_id.cloned(),
            settings: settings.clone(),
            kind,
            global_credentials: tool.global_credentials(),
        };
        let mut requests = self.requests.lock();
        if requests
            .values()
            .any(|existing| conflicts(existing, &request))
        {
            return Err(Error::new(
                ErrorKind::Busy,
                "An active operation owns this profile, CLI installation or shared data",
            ));
        }
        let id = Uuid::new_v4();
        requests.insert(id, request);
        Ok(ActivityLease {
            id,
            book: self.clone(),
        })
    }

    pub fn active_count(&self) -> usize {
        self.requests.lock().len()
    }

    pub fn is_tool_busy(&self, tool_id: &str) -> bool {
        self.requests
            .lock()
            .values()
            .any(|request| request.tool_id == tool_id)
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        self.book.requests.lock().remove(&self.id);
    }
}

fn conflicts(left: &Request, right: &Request) -> bool {
    if left.tool_id != right.tool_id {
        return false;
    }
    if matches!(left.kind, ActivityKind::Rename | ActivityKind::Install)
        || matches!(right.kind, ActivityKind::Rename | ActivityKind::Install)
        || left.profile_id == right.profile_id
    {
        return true;
    }
    matches!(left.kind, ActivityKind::Session | ActivityKind::Inspect)
        && matches!(right.kind, ActivityKind::Session | ActivityKind::Inspect)
        && (left.global_credentials
            || right.global_credentials
            || left.settings.conflicts(&right.settings))
}
