//! CLI-specific launch settings with owned cleanup.
use crate::{
    Error, ErrorKind, Result, domain::Profile, profiles::environment::Environment,
    storage::DataRoot, tools::catalog::Tool,
};
use serde_json::{Map, Value};
use std::path::PathBuf;
pub struct LaunchPlan {
    pub arguments: Vec<String>,
    root: DataRoot,
    amp: Option<AmpSettings>,
    preserve: bool,
}
struct AmpSettings {
    base: PathBuf,
    temporary: PathBuf,
    before: Option<Vec<u8>>,
    permission: Option<Value>,
}
fn object(root: &DataRoot, path: &std::path::Path) -> Result<Map<String, Value>> {
    let Some(bytes) = root.read_limited(path, 8 * 1024 * 1024)? else {
        return Ok(Map::new());
    };
    let value: Value = jsonc_parser::parse_to_serde_value(
        std::str::from_utf8(&bytes).map_err(Error::corrupt)?,
        &Default::default(),
    )
    .map_err(Error::corrupt)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| Error::new(ErrorKind::CorruptData, "CLI settings must be an object"))
}
impl LaunchPlan {
    pub fn prepare(
        root: &DataRoot,
        tool: &Tool,
        profile: &Profile,
        full_permission: bool,
        environment: &mut Environment,
    ) -> Result<Self> {
        let mut plan = Self {
            arguments: if full_permission {
                tool.permission_args.clone()
            } else {
                Vec::new()
            },
            root: root.clone(),
            amp: None,
            preserve: false,
        };
        let home = tool.profile_relative(profile);
        if tool.id == "claude" {
            let settings = object(root, &home.join(".claude/settings.json"))?;
            if let Some(effort) = settings
                .get("effortLevel")
                .and_then(Value::as_str)
                .filter(|value| ["low", "medium", "high", "xhigh"].contains(value))
            {
                plan.arguments.extend(["--effort".into(), effort.into()]);
            }
        }
        if tool.id == "goose" && full_permission {
            environment.insert("GOOSE_MODE".into(), "auto".into());
        }
        if tool.id == "amp" {
            let base = home.join(".config/amp/settings.json");
            let mut settings = object(root, &base)?;
            if full_permission {
                let before = root.read(&base)?;
                let permission =
                    settings.insert("amp.dangerouslyAllowAll".into(), Value::Bool(true));
                let temporary = home
                    .join(".config/amp")
                    .join(format!(".launch-{}.json", uuid::Uuid::new_v4().simple()));
                root.write_json(&temporary, &settings)?;
                let path = root.resolve(&temporary)?.to_string_lossy().into_owned();
                environment.insert("AMP_SETTINGS_FILE".into(), path.clone());
                plan.arguments.extend(["--settings-file".into(), path]);
                plan.amp = Some(AmpSettings {
                    base,
                    temporary,
                    before,
                    permission,
                });
            } else {
                environment.insert(
                    "AMP_SETTINGS_FILE".into(),
                    root.resolve(&base)?.to_string_lossy().into_owned(),
                );
            }
        }
        Ok(plan)
    }
    pub fn finish(&mut self) -> Result<()> {
        self.preserve = true;
        let Some(amp) = self.amp.as_ref() else {
            return Ok(());
        };
        let mut changed = object(&self.root, &amp.temporary)?;
        if self.root.read(&amp.base)? != amp.before {
            let archive = PathBuf::from("_profiles/trash/launch-settings")
                .join(uuid::Uuid::new_v4().to_string())
                .join("settings.json");
            self.root.move_path(&amp.temporary, archive)?;
            self.amp.take();
            return Err(Error::new(
                ErrorKind::Conflict,
                "CLI settings changed during the session; the session copy was archived",
            ));
        }
        if let Some(value) = &amp.permission {
            changed.insert("amp.dangerouslyAllowAll".into(), value.clone());
        } else {
            changed.remove("amp.dangerouslyAllowAll");
        }
        self.root.write_json(&amp.base, &changed)?;
        self.root.remove_file(&amp.temporary)?;
        self.amp.take();
        Ok(())
    }
}
impl Drop for LaunchPlan {
    fn drop(&mut self) {
        if !self.preserve {
            if let Some(amp) = self.amp.take() {
                let _ = self.root.remove_file(amp.temporary);
            }
        }
    }
}
