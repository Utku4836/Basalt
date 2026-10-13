//! What `install <name>` offers: the catalog's tools first, then what npm, PyPI and
//! winget have: the ones that install exactly that command, then the ones
//! around the same name, then the rest; most used first in each, ten at most.

use serde::Deserialize;
use basalt_core::{
    domain::ApplicationIdentity,
    system::process::CancellationToken,
    tools::{catalog::Catalog, install::download::Http},
};

pub const MOST: usize = 10;
/// Weekly downloads below this mark a package as less known.
const WELL_KNOWN: u64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A catalog tool, by id: installed by the backend into its own folder.
    Recipe(String),
    /// An npm package, by name.
    Npm(String),
    /// A PyPI project, installed with uv into its own environment.
    Pypi(String),
    /// A winget package, installed by winget.
    Winget { id: String, version: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The tool's own name ("Claude Code"), or the package's last segment.
    pub title: String,
    /// Where it comes from ("npm:@anthropic-ai/claude-code").
    pub package: String,
    pub source: Source,
    pub weekly: Option<u64>,
    /// Who publishes it: the npm scope or author, the PyPI author, the winget
    /// publisher. Empty when unknown.
    pub by: String,
    /// The commands it puts on PATH, where known before installing.
    pub commands: Vec<String>,
}

impl Offer {
    pub fn well_known(&self) -> bool {
        matches!(self.source, Source::Recipe(_) | Source::Winget { .. })
            || self.weekly.is_some_and(|weekly| weekly >= WELL_KNOWN)
    }

    /// The faded note at the right: weekly downloads, or the winget version.
    pub fn note(&self) -> String {
        match (&self.source, self.weekly) {
            (_, Some(weekly)) => weekly_label(weekly),
            (Source::Winget { version, .. }, _) if !version.is_empty() => format!("v{version}"),
            _ => String::new(),
        }
    }

    /// How widely used it is, comparable across sources: a winget package counts as a
    /// fairly used one.
    fn reach(&self) -> u64 {
        match (&self.source, self.weekly) {
            (_, Some(weekly)) => weekly,
            (Source::Winget { .. }, _) => 20_000,
            _ => 0,
        }
    }

    /// 0 when it is exactly the command, 1 when it is around that name, 2 otherwise.
    fn closeness(&self, command: &str) -> u8 {
        let wanted = command.to_ascii_lowercase();
        let named = self.package.split_once(':').map_or(self.package.as_str(), |(_, name)| name).to_ascii_lowercase();
        let title = self.title.to_ascii_lowercase();
        let words: Vec<&str> =
            named.split(|c: char| !c.is_ascii_alphanumeric()).chain(title.split(|c: char| !c.is_ascii_alphanumeric())).filter(|word| !word.is_empty()).collect();
        if self.provides(command) || words.contains(&wanted.as_str()) || title == wanted {
            0
        } else if words.iter().any(|word| word.starts_with(&wanted))
            // A short name inside a longer word is mostly chance ("ds" in "worlds").
            || (wanted.len() >= 4 && (named.contains(&wanted) || title.contains(&wanted)))
        {
            1
        } else {
            2
        }
    }

    /// Whether installing it makes `command` work.
    pub fn provides(&self, command: &str) -> bool {
        self.commands.iter().any(|provided| provided.eq_ignore_ascii_case(command))
    }
}

/// Weekly downloads, short: "980/wk", "41k/wk", "4.1M/wk".
pub fn weekly_label(weekly: u64) -> String {
    match weekly {
        0..1_000 => format!("{weekly}/wk"),
        1_000..1_000_000 => format!("{}k/wk", weekly / 1_000),
        _ => format!("{:.1}M/wk", weekly as f64 / 1_000_000.0),
    }
}

/// A person or company from an author field: "Jane Doe <jane@example.com>" gives
/// "Jane Doe", an address alone gives its domain.
pub fn person(text: &str) -> String {
    let text = text.trim();
    let name = text.split('<').next().unwrap_or("").trim();
    if !name.is_empty() && !name.contains('@') {
        return name.to_owned();
    }
    let address = text.trim_matches(|c| c == '<' || c == '>');
    address.split('@').nth(1).map(|domain| domain.trim_end_matches('>').to_owned()).unwrap_or_default()
}

/// How an install says the system lacks a runtime it needs (Node.js, Python), which
/// winget can install.
const NEEDS: &str = "needs runtime ";

pub fn needs(winget_id: &str, name: &str) -> String {
    format!("{NEEDS}{winget_id} {name}")
}

/// The winget id and name of the runtime an install error asks for.
pub fn needed(error: &str) -> Option<(String, String)> {
    let (id, name) = error.strip_prefix(NEEDS)?.split_once(' ')?;
    Some((id.to_owned(), name.to_owned()))
}

/// A command name worth looking up: one plain word.
pub fn plain(command: &str) -> bool {
    !command.is_empty() && command.len() <= 64 && command.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Catalog tools whose command or name holds `command`; the exact command first.
pub fn recipes(catalog: &Catalog, command: &str) -> Vec<Offer> {
    let wanted = command.to_ascii_lowercase();
    let mut tools: Vec<_> = catalog
        .tools()
        .iter()
        .filter(|tool| tool.bin.to_ascii_lowercase().contains(&wanted) || tool.name.to_ascii_lowercase().contains(&wanted))
        .collect();
    tools.sort_by_key(|tool| !tool.bin.eq_ignore_ascii_case(command));
    tools
        .into_iter()
        .map(|tool| Offer {
            title: tool.name.clone(),
            package: match tool.installer.kind.as_str() {
                "npm" => format!("npm:{}", tool.installer.package.clone().unwrap_or_default()),
                kind => format!("{kind}:{}", tool.id),
            },
            source: Source::Recipe(tool.id.clone()),
            weekly: None,
            by: tool.installer.package.as_deref().and_then(scope).unwrap_or_default(),
            commands: vec![tool.bin.clone()],
        })
        .collect()
}

/// The commands a package's `bin` field installs: a string names one command after
/// the package, an object names each.
pub fn commands(package: &str, bin: &serde_json::Value) -> Vec<String> {
    match bin {
        serde_json::Value::String(_) => vec![title(package)],
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The catalog command that starts with `typed`, for completing `install <typed>`.
pub fn complete<'a>(catalog: &'a Catalog, typed: &str) -> Option<&'a str> {
    let typed = typed.to_ascii_lowercase();
    catalog
        .tools()
        .iter()
        .map(|tool| tool.bin.as_str())
        .filter(|bin| bin.len() > typed.len() && bin.to_ascii_lowercase().starts_with(&typed))
        .min_by_key(|bin| bin.len())
}

/// What an Enter on the command line asks to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `install claude`: look up what provides it.
    Search(String),
    /// `install @scope/package`, or a pasted global npm install: that package.
    Package(String),
}

/// `install <name>` with one name, or a pasted global npm install.
pub fn request(line: &str) -> Option<Request> {
    if let Some(package) = global_npm_install(line) {
        return Some(Request::Package(package));
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    match words[..] {
        ["install", name] if plain(name) => Some(Request::Search(name.to_owned())),
        ["install", name] if name.starts_with('@') => basalt_core::tools::install::command::package_name(name)
            .ok()
            .map(|_| Request::Package(name.to_owned())),
        _ => None,
    }
}

/// The scope of a scoped npm package: "@anthropic-ai/claude-code" gives "anthropic-ai".
pub fn scope(package: &str) -> Option<String> {
    package.strip_prefix('@')?.split_once('/').map(|(scope, _)| scope.to_owned())
}

/// The account owning a GitHub or GitLab repository address: "git+https://github.com/
/// vercel/serve.git" and "github:vercel/serve" give "vercel".
pub fn owner(repository: &str) -> Option<String> {
    let path = ["github.com/", "gitlab.com/", "github:", "gitlab:"]
        .iter()
        .find_map(|host| repository.split_once(host).map(|(_, path)| path))?;
    let owner = path.split('/').next()?.trim();
    (!owner.is_empty() && path.contains('/')).then(|| owner.to_owned())
}

/// Accounts that publish for others: release bots and CI.
fn automated(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("bot") || name.contains("github actions") || name.contains("automation") || name == "ci"
}

/// A package's name without its scope.
pub fn title(package: &str) -> String {
    package.rsplit('/').next().unwrap_or(package).to_owned()
}

#[derive(Deserialize)]
struct Search {
    objects: Vec<Found>,
}
#[derive(Deserialize)]
struct Found {
    package: Package,
    #[serde(default)]
    downloads: Option<Downloads>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
}
#[derive(Deserialize)]
struct Downloads {
    #[serde(default)]
    weekly: u64,
}
#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    bin: Option<serde_json::Value>,
    #[serde(default)]
    author: Option<serde_json::Value>,
    #[serde(default, rename = "_npmUser")]
    publisher: Option<Publisher>,
    #[serde(default)]
    repository: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct Publisher {
    name: String,
}
#[derive(Deserialize)]
struct Point {
    downloads: u64,
}

async fn npm(http: &Http, command: &str, cancel: &CancellationToken) -> Vec<Offer> {
    // The text alone finds mostly libraries for a short name; the same text among
    // packages tagged as command line tools finds the tools.
    let search = |text: String| async move {
        http.json::<Search>(&format!("https://registry.npmjs.org/-/v1/search?text={text}&size=30"), cancel)
            .await
            .map(|search| search.objects)
            .unwrap_or_default()
    };
    let (plain, tools) = tokio::join!(search(command.to_owned()), search(format!("{command}+keywords:cli")));
    // The package named like the command comes first; text search may not list it.
    let mut candidates = vec![Found { package: Package { name: command.to_owned() }, downloads: None }];
    let (mut tools, mut plain) = (tools.into_iter(), plain.into_iter());
    while candidates.len() < 40 {
        let (tool, other) = (tools.next(), plain.next());
        if tool.is_none() && other.is_none() {
            break;
        }
        for found in [tool, other].into_iter().flatten() {
            if !candidates.iter().any(|known| known.package.name == found.package.name) {
                candidates.push(found);
            }
        }
    }
    // Each candidate's manifest and downloads are asked at once.
    let mut asking = tokio::task::JoinSet::new();
    for found in candidates {
        let (http, cancel, wanted) = (http.clone(), cancel.clone(), command.to_ascii_lowercase());
        asking.spawn(async move {
            let name = found.package.name;
            let manifest = format!("https://registry.npmjs.org/{}/latest", name.replace('/', "%2F"));
            let Ok(Manifest { bin: Some(bin), author, publisher, repository }) = http.json::<Manifest>(&manifest, &cancel).await else {
                return None;
            };
            // An organization's scope says most, then the owner of its repository, then
            // the author, then who published; release bots say nothing.
            let text = |value: Option<serde_json::Value>, field: &str| match value {
                Some(serde_json::Value::String(text)) => text,
                Some(serde_json::Value::Object(fields)) => fields.get(field).and_then(|value| value.as_str()).unwrap_or("").to_owned(),
                _ => String::new(),
            };
            let by = [
                scope(&name),
                owner(&text(repository, "url")),
                Some(person(&text(author, "name"))),
                publisher.map(|publisher| publisher.name),
            ]
            .into_iter()
            .flatten()
            .find(|by| !by.is_empty() && !automated(by))
            .unwrap_or_default();
            // Only packages that put a command on PATH.
            let provided = commands(&name, &bin);
            if provided.is_empty() {
                return None;
            }
            let weekly = match found.downloads {
                Some(downloads) => Some(downloads.weekly),
                None => http
                    .json::<Point>(&format!("https://api.npmjs.org/downloads/point/last-week/{name}"), &cancel)
                    .await
                    .ok()
                    .map(|point| point.downloads),
            };
            // "cli" alone says little: a scoped name that does not hold what was asked
            // keeps its scope ("claude-flow/cli").
            let short = title(&name);
            let shown = if short.to_ascii_lowercase().contains(&wanted) { short } else { name.trim_start_matches('@').to_owned() };
            Some(Offer { title: shown, package: format!("npm:{name}"), source: Source::Npm(name), weekly, by, commands: provided })
        });
    }
    let mut offers = Vec::new();
    while let Some(result) = asking.join_next().await {
        if let Ok(Some(offer)) = result {
            offers.push(offer);
        }
    }
    offers
}

/// The package of a pasted global npm install (`npm i -g x`, `npm install --global x`,
/// `pnpm add -g x`, `yarn global add x`, `bun add -g x`).
pub fn global_npm_install(line: &str) -> Option<String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let global = words.iter().any(|word| matches!(*word, "-g" | "--global")) || words.get(1) == Some(&"global");
    let install = match words.first().copied()? {
        "npm" => words.get(1).is_some_and(|word| matches!(*word, "i" | "install" | "add")),
        "pnpm" | "bun" => words.get(1).is_some_and(|word| matches!(*word, "add" | "install" | "i")),
        "yarn" => words.get(1) == Some(&"global") && words.get(2) == Some(&"add"),
        _ => false,
    };
    if !(global && install) {
        return None;
    }
    let packages: Vec<&str> = words[2..].iter().copied().filter(|word| !word.starts_with('-') && *word != "add").collect();
    let [package] = packages[..] else {
        return None;
    };
    // A version or tag after the name is left to "latest".
    let name = match package.rfind('@') {
        Some(at) if at > 0 => &package[..at],
        _ => package,
    };
    basalt_core::tools::install::command::package_name(name).ok().map(|_| name.to_owned())
}

/// Everything that provides `command`, best first: the catalog's tools, then the
/// others ranked together. Every source is asked at once. Blocks; run it off the UI
/// thread.
pub fn search(catalog: &Catalog, identity: &ApplicationIdentity, command: &str) -> Vec<Offer> {
    if !plain(command) {
        return Vec::new();
    }
    let mut offers = recipes(catalog, command);
    let found = (|| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().ok()?;
        let http = Http::new(identity, &[], std::time::Duration::from_secs(30)).ok()?;
        let cancel = CancellationToken::new();
        let winget = {
            let command = command.to_owned();
            runtime.spawn_blocking(move || crate::sources::winget(&command))
        };
        let (npm, pypi, winget) = runtime.block_on(async {
            tokio::join!(npm(&http, command, &cancel), crate::sources::pypi(&http, command, &cancel), winget)
        });
        // Each source keeps a share, so one cannot crowd out the others.
        let mut others: Vec<Offer> = npm;
        others.extend(pypi.into_iter().take(2));
        others.extend(winget.unwrap_or_default().into_iter().take(4));
        Some(others)
    })()
    .unwrap_or_default();
    let mut others = Vec::new();
    for offer in found {
        // A recipe's own package is already offered, installed the recipe's way.
        match offers.iter_mut().find(|known| known.package == offer.package) {
            Some(known) => known.weekly = offer.weekly,
            None => others.push(offer),
        }
    }
    others.sort_by_key(|offer| (offer.closeness(command), std::cmp::Reverse(offer.reach())));
    // One whose name does not hold the command stays only when it starts the same way:
    // "httpsnippet" for "httpie", never a popular "rimraf" a tag search brought.
    let start: String = command.to_ascii_lowercase().chars().take(2).collect();
    others.retain(|offer| {
        offer.closeness(command) < 2
            || offer.title.to_ascii_lowercase().starts_with(&start)
            || offer.commands.iter().any(|each| each.to_ascii_lowercase().starts_with(&start))
    });
    offers.extend(others);
    offers.truncate(MOST);
    offers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_offer_the_catalog_tool_that_installs_the_command() {
        let catalog = Catalog::built_in();
        let offers = recipes(&catalog, "claude");
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].title, "Claude Code");
        assert_eq!(offers[0].package, "npm:@anthropic-ai/claude-code");
        assert!(offers[0].well_known());
        assert!(recipes(&catalog, "not-a-tool").is_empty());
    }

    #[test]
    fn package_bins_name_their_commands() {
        assert_eq!(commands("@scope/tool", &serde_json::json!("cli.js")), ["tool"]);
        let mut both = commands("x", &serde_json::json!({"ccr": "a.js", "claude-router": "b.js"}));
        both.sort();
        assert_eq!(both, ["ccr", "claude-router"]);
        assert_eq!(title("@musistudio/claude-code-router"), "claude-code-router");
        assert_eq!(scope("@anthropic-ai/claude-code").as_deref(), Some("anthropic-ai"));
        assert_eq!(scope("serve"), None);
        assert_eq!(person("Jane Doe <jane@example.com>"), "Jane Doe");
        assert_eq!(person("<team@httpie.io>"), "httpie.io");
        assert_eq!(person("Vercel"), "Vercel");
        assert_eq!(owner("git+https://github.com/vercel/serve.git").as_deref(), Some("vercel"));
        assert_eq!(owner("github:vercel/serve").as_deref(), Some("vercel"));
        assert_eq!(owner("https://example.com/x"), None);
        assert!(automated("vercel-release-bot") && automated("GitHub Actions") && !automated("Jane Doe"));
        assert_eq!(needed(&needs("OpenJS.NodeJS.LTS", "Node.js")), Some(("OpenJS.NodeJS.LTS".into(), "Node.js".into())));
        assert_eq!(needed("npm install failed"), None);
        assert_eq!(weekly_label(980), "980/wk");
        assert_eq!(weekly_label(41_300), "41k/wk");
        assert_eq!(weekly_label(4_120_000), "4.1M/wk");
        assert!(plain("claude") && plain("gh-dash") && !plain("rm -rf") && !plain(""));
    }

    #[test]
    fn install_requests_name_one_tool_or_package() {
        assert_eq!(request("install claude"), Some(Request::Search("claude".into())));
        assert_eq!(request("  install   ds "), Some(Request::Search("ds".into())));
        assert_eq!(request("install @anthropic-ai/claude-code"), Some(Request::Package("@anthropic-ai/claude-code".into())));
        assert_eq!(request("npm i -g serve"), Some(Request::Package("serve".into())));
        assert_eq!(request("install a b"), None);
        assert_eq!(request("install"), None);
        assert_eq!(request("ech selam"), None);
        let catalog = Catalog::built_in();
        assert_eq!(complete(&catalog, "cla"), Some("claude"));
        assert_eq!(complete(&catalog, "claude"), None, "nothing left to complete");
    }

    #[test]
    fn pasted_global_installs_name_their_package() {
        assert_eq!(global_npm_install("npm i -g @anthropic-ai/claude-code").as_deref(), Some("@anthropic-ai/claude-code"));
        assert_eq!(global_npm_install("npm install --global opencode-ai@latest").as_deref(), Some("opencode-ai"));
        assert_eq!(global_npm_install("pnpm add -g typescript").as_deref(), Some("typescript"));
        assert_eq!(global_npm_install("yarn global add serve").as_deref(), Some("serve"));
        assert_eq!(global_npm_install("npm i lodash"), None, "local installs stay with npm");
        assert_eq!(global_npm_install("npm i -g a b"), None, "one package at a time");
    }
}
