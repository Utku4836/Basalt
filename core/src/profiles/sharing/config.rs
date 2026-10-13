use std::path::Path;

use serde_json::{Map, Value as JsonValue};
use serde_yaml_ng::{Mapping as YamlMap, Value as YamlValue};

use crate::{
    Error, ErrorKind, Result, domain::ShareCategory, storage::DataRoot, tools::catalog::Tool,
};

use super::CategoryState;

enum Document {
    Json(Map<String, JsonValue>),
    Toml(toml::Table),
    Yaml(YamlMap),
}

enum SectionValue {
    Json(JsonValue),
    Toml(toml::Value),
    Yaml(YamlValue),
}

impl Document {
    fn empty(format: &str) -> Result<Self> {
        match format {
            "jsonc" => Ok(Self::Json(Map::new())),
            "toml" => Ok(Self::Toml(toml::Table::new())),
            "yaml" => Ok(Self::Yaml(YamlMap::new())),
            _ => Err(Error::new(
                ErrorKind::Unsupported,
                "Unsupported shared configuration format",
            )),
        }
    }

    fn read(root: &DataRoot, path: &Path, format: &str) -> Result<Option<Self>> {
        let Some(bytes) = root.read_limited(path, 8 * 1024 * 1024)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes).map_err(Error::corrupt)?;
        let parsed = match format {
            "jsonc" => {
                let value: JsonValue =
                    jsonc_parser::parse_to_serde_value(text, &Default::default())
                        .map_err(Error::corrupt)?;
                Self::Json(value.as_object().cloned().ok_or_else(|| {
                    Error::new(ErrorKind::CorruptData, "CLI JSONC config must be an object")
                })?)
            }
            "toml" => Self::Toml(text.parse::<toml::Table>().map_err(Error::corrupt)?),
            "yaml" => {
                let value: YamlValue = serde_yaml_ng::from_str(text).map_err(Error::corrupt)?;
                Self::Yaml(value.as_mapping().cloned().ok_or_else(|| {
                    Error::new(ErrorKind::CorruptData, "CLI YAML config must be a mapping")
                })?)
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "Unsupported shared configuration format",
                ));
            }
        };
        parsed.keys()?;
        Ok(Some(parsed))
    }

    fn keys(&self) -> Result<Vec<String>> {
        match self {
            Self::Json(map) => Ok(map.keys().cloned().collect()),
            Self::Toml(map) => Ok(map.keys().cloned().collect()),
            Self::Yaml(map) => map
                .keys()
                .map(|key| {
                    key.as_str().map(str::to_owned).ok_or_else(|| {
                        Error::new(ErrorKind::CorruptData, "CLI YAML config needs string keys")
                    })
                })
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<SectionValue> {
        match self {
            Self::Json(map) => map.get(key).cloned().map(SectionValue::Json),
            Self::Toml(map) => map.get(key).cloned().map(SectionValue::Toml),
            Self::Yaml(map) => map
                .get(YamlValue::String(key.into()))
                .cloned()
                .map(SectionValue::Yaml),
        }
    }

    fn remove(&mut self, key: &str) {
        match self {
            Self::Json(map) => {
                map.remove(key);
            }
            Self::Toml(map) => {
                map.remove(key);
            }
            Self::Yaml(map) => {
                map.remove(YamlValue::String(key.into()));
            }
        }
    }

    fn insert(&mut self, key: &str, value: SectionValue) -> Result<()> {
        match (self, value) {
            (Self::Json(map), SectionValue::Json(value)) => {
                map.insert(key.into(), value);
            }
            (Self::Toml(map), SectionValue::Toml(value)) => {
                map.insert(key.into(), value);
            }
            (Self::Yaml(map), SectionValue::Yaml(value)) => {
                map.insert(YamlValue::String(key.into()), value);
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::CorruptData,
                    "Mismatched CLI config format",
                ));
            }
        }
        Ok(())
    }

    fn write(&self, root: &DataRoot, path: &Path) -> Result<()> {
        let content = match self {
            Self::Json(map) => format!(
                "{}\n",
                serde_json::to_string_pretty(map).map_err(Error::corrupt)?
            ),
            Self::Toml(map) => toml::to_string(map).map_err(Error::corrupt)?,
            Self::Yaml(map) => serde_yaml_ng::to_string(map).map_err(Error::corrupt)?,
        };
        root.write(path, content.as_bytes())
    }
}

pub(super) fn synchronize(
    root: &DataRoot,
    source: &Path,
    target: &Path,
    tool: &Tool,
    category: ShareCategory,
    previous: &CategoryState,
) -> Result<(Vec<String>, bool)> {
    let Some(rule) = &tool.mcp else {
        return Ok((Vec::new(), false));
    };
    if rule.format == "file" {
        return Ok((Vec::new(), false));
    }
    let key = rule
        .key
        .as_deref()
        .ok_or_else(|| Error::new(ErrorKind::CorruptData, "MCP rule has no section key"))?;
    let source_path = source.join(&rule.path);
    let target_path = target.join(&rule.path);
    let source = Document::read(root, &source_path, &rule.format)?;
    let existing = Document::read(root, &target_path, &rule.format)?;
    if source.is_none() && existing.is_none() {
        return Ok((Vec::new(), false));
    }
    let mut destination = existing.unwrap_or(Document::empty(&rule.format)?);
    match category {
        ShareCategory::SharedConfig => {
            let source_keys = source
                .as_ref()
                .map(Document::keys)
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .filter(|name| name != key)
                .collect::<Vec<_>>();
            for old in &previous.config_keys {
                if old == key {
                    return Err(Error::new(
                        ErrorKind::CorruptData,
                        "MCP key appears in shared config manifest",
                    ));
                }
                if !source_keys.contains(old) {
                    destination.remove(old);
                }
            }
            if let Some(source) = &source {
                for name in &source_keys {
                    destination.insert(name, source.get(name).expect("key from source"))?;
                }
            }
            destination.write(root, &target_path)?;
            Ok((source_keys, false))
        }
        ShareCategory::SharedMcp => {
            let current = source.as_ref().and_then(|document| document.get(key));
            let present = current.is_some();
            if let Some(current) = current {
                destination.insert(key, current)?;
            } else if previous.mcp_present {
                destination.remove(key);
            } else {
                return Ok((Vec::new(), false));
            }
            destination.write(root, &target_path)?;
            Ok((Vec::new(), present))
        }
        _ => Ok((Vec::new(), false)),
    }
}

pub(super) fn preflight(root: &DataRoot, source: &Path, target: &Path, tool: &Tool) -> Result<()> {
    let Some(rule) = &tool.mcp else {
        return Ok(());
    };
    if rule.format == "file" {
        return Ok(());
    }
    Document::read(root, &source.join(&rule.path), &rule.format)?;
    Document::read(root, &target.join(&rule.path), &rule.format)?;
    Ok(())
}

/// The number of MCP servers configured under `home`.
pub(super) fn mcp_count(root: &DataRoot, home: &Path, tool: &Tool) -> usize {
    let Some(rule) = &tool.mcp else {
        return 0;
    };
    let path = home.join(&rule.path);
    if rule.format == "file" {
        // A file of its own: the servers are its `mcpServers` object, or the whole object.
        let Ok(Some(Document::Json(map))) = Document::read(root, &path, "jsonc") else {
            return 0;
        };
        return match map.get("mcpServers") {
            Some(JsonValue::Object(servers)) => servers.len(),
            _ => map.len(),
        };
    }
    let Some(key) = &rule.key else {
        return 0;
    };
    match Document::read(root, &path, &rule.format)
        .ok()
        .flatten()
        .and_then(|document| document.get(key))
    {
        Some(SectionValue::Json(JsonValue::Object(servers))) => servers.len(),
        Some(SectionValue::Toml(toml::Value::Table(servers))) => servers.len(),
        Some(SectionValue::Yaml(YamlValue::Mapping(servers))) => servers.len(),
        _ => 0,
    }
}

/// Add the MCP servers configured under `home` to those under `shared`, keeping the
/// shared ones where both have a server of the same name.
pub(super) fn merge_mcp(root: &DataRoot, home: &Path, shared: &Path, tool: &Tool) -> Result<()> {
    let Some(rule) = &tool.mcp else {
        return Ok(());
    };
    if rule.format == "file" {
        // Whole files are joined by the file sync, which keeps the shared one.
        return Ok(());
    }
    let key = rule
        .key
        .as_deref()
        .ok_or_else(|| Error::new(ErrorKind::CorruptData, "MCP rule has no section key"))?;
    let Some(own) = Document::read(root, &home.join(&rule.path), &rule.format)?
        .and_then(|document| document.get(key))
    else {
        return Ok(());
    };
    let target = shared.join(&rule.path);
    let mut document =
        Document::read(root, &target, &rule.format)?.unwrap_or(Document::empty(&rule.format)?);
    let merged = match (document.get(key), own) {
        (None, own) => own,
        (
            Some(SectionValue::Json(JsonValue::Object(mut servers))),
            SectionValue::Json(JsonValue::Object(own)),
        ) => {
            for (name, server) in own {
                servers.entry(name).or_insert(server);
            }
            SectionValue::Json(JsonValue::Object(servers))
        }
        (
            Some(SectionValue::Toml(toml::Value::Table(mut servers))),
            SectionValue::Toml(toml::Value::Table(own)),
        ) => {
            for (name, server) in own {
                servers.entry(name).or_insert(server);
            }
            SectionValue::Toml(toml::Value::Table(servers))
        }
        (
            Some(SectionValue::Yaml(YamlValue::Mapping(mut servers))),
            SectionValue::Yaml(YamlValue::Mapping(own)),
        ) => {
            for (name, server) in own {
                if !servers.contains_key(&name) {
                    servers.insert(name, server);
                }
            }
            SectionValue::Yaml(YamlValue::Mapping(servers))
        }
        (Some(existing), _) => existing,
    };
    document.insert(key, merged)?;
    document.write(root, &target)
}
