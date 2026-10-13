//! What a coding agent is doing, read from what it shows: the bottom of its terminal,
//! its title and its progress reports, matched against that agent's rules
//! (`agent_rules/`). The rule of highest priority that matches decides; a known agent
//! that matches none is idle.
use regex::Regex;
use serde::Deserialize;
use std::{collections::HashMap, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    /// Waiting for you: a permission, a question, a choice.
    Blocked,
}

/// One reading of a screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub state: State,
    /// Idle only because nothing showed work: no rule saw the agent's idle prompt.
    /// A turn that pauses between steps looks like this for a moment.
    pub plain: bool,
}

/// What the screen shows, or None when it cannot tell (a transcript or menu over the
/// agent's own view) and the previous reading stands.
pub fn read(agent: &str, screen: &str, title: &str, progress: &str) -> Option<Reading> {
    let input = Input { screen, title, progress };
    let Some(rules) = rules().get(rules_name(agent)) else {
        return Some(spinning(&input));
    };
    let mut best: Option<&Rule> = None;
    for rule in rules {
        if best.is_some_and(|best| best.priority >= rule.priority) {
            continue;
        }
        let text = region(&input, &rule.region);
        if rule.gate.matches(text, &text.to_lowercase()) {
            best = Some(rule);
        }
    }
    match best {
        Some(rule) if rule.skip || rule.state.is_none() => None,
        Some(rule) => rule.state.map(|state| Reading { state, plain: state == State::Idle && !rule.visible }),
        None => Some(Reading { state: State::Idle, plain: true }),
    }
}

/// An agent without rules is working while a spinner turns in its title or near the
/// bottom of its screen.
fn spinning(input: &Input) -> Reading {
    static SPINNER: OnceLock<Regex> = OnceLock::new();
    let spinner = SPINNER.get_or_init(|| Regex::new(r"(?m)^\s*[\x{2801}-\x{28FF}\x{25D0}-\x{25D3}](?:\s|$)").unwrap());
    let working = spinner.is_match(input.title) || spinner.is_match(bottom_non_empty_lines(input.screen, 8));
    Reading { state: if working { State::Working } else { State::Idle }, plain: !working }
}

/// The rules file an agent id reads.
fn rules_name(agent: &str) -> &str {
    match agent {
        "copilot" => "github-copilot",
        "agy" => "antigravity",
        "cursor-agent" => "cursor",
        other => other,
    }
}

const FILES: &[(&str, &str)] = &[
    ("amp", include_str!("agent_rules/amp.toml")),
    ("antigravity", include_str!("agent_rules/antigravity.toml")),
    ("claude", include_str!("agent_rules/claude.toml")),
    ("cline", include_str!("agent_rules/cline.toml")),
    ("codex", include_str!("agent_rules/codex.toml")),
    ("cursor", include_str!("agent_rules/cursor.toml")),
    ("devin", include_str!("agent_rules/devin.toml")),
    ("droid", include_str!("agent_rules/droid.toml")),
    ("gemini", include_str!("agent_rules/gemini.toml")),
    ("github-copilot", include_str!("agent_rules/github-copilot.toml")),
    ("grok", include_str!("agent_rules/grok.toml")),
    ("hermes", include_str!("agent_rules/hermes.toml")),
    ("kilo", include_str!("agent_rules/kilo.toml")),
    ("kimi", include_str!("agent_rules/kimi.toml")),
    ("kiro", include_str!("agent_rules/kiro.toml")),
    ("letta", include_str!("agent_rules/letta.toml")),
    ("maki", include_str!("agent_rules/maki.toml")),
    ("muse", include_str!("agent_rules/muse.toml")),
    ("opencode", include_str!("agent_rules/opencode.toml")),
    ("pi", include_str!("agent_rules/pi.toml")),
    ("qodercli", include_str!("agent_rules/qodercli.toml")),
    ("qwen", include_str!("agent_rules/qwen.toml")),
];

/// Every agent's rules, read once. A file that does not parse is a build mistake the
/// tests catch; in the app that agent falls back to the spinner.
pub fn rules() -> &'static HashMap<&'static str, Vec<Rule>> {
    static RULES: OnceLock<HashMap<&'static str, Vec<Rule>>> = OnceLock::new();
    RULES.get_or_init(|| {
        FILES
            .iter()
            .filter_map(|(name, text)| parse(text).ok().map(|rules| (*name, rules)))
            .collect()
    })
}

pub fn parse(text: &str) -> Result<Vec<Rule>, String> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        rules: Vec<RawRule>,
    }
    #[derive(Deserialize)]
    struct RawRule {
        state: Option<String>,
        #[serde(default)]
        priority: i32,
        #[serde(default = "whole")]
        region: String,
        #[serde(default)]
        skip_state_update: bool,
        #[serde(default)]
        visible_idle: bool,
        #[serde(flatten)]
        gate: RawGate,
    }
    fn whole() -> String {
        "whole_recent".into()
    }
    let file: File = toml::from_str(text).map_err(|error| error.to_string())?;
    file.rules
        .into_iter()
        .map(|rule| {
            let state = match rule.state.as_deref() {
                Some("idle") => Some(State::Idle),
                Some("working") => Some(State::Working),
                Some("blocked") => Some(State::Blocked),
                Some("unknown") | None => None,
                Some(other) => return Err(format!("unknown state {other}")),
            };
            Ok(Rule {
                state,
                priority: rule.priority,
                region: Region::parse(&rule.region)?,
                skip: rule.skip_state_update,
                visible: rule.visible_idle,
                gate: rule.gate.compile()?,
            })
        })
        .collect()
}

pub struct Rule {
    state: Option<State>,
    priority: i32,
    region: Region,
    skip: bool,
    /// An idle rule that sees the agent's prompt, not just the absence of work.
    visible: bool,
    gate: Gate,
}

#[derive(Deserialize, Default)]
struct RawGate {
    #[serde(default)]
    all: Vec<RawGate>,
    #[serde(default)]
    any: Vec<RawGate>,
    #[serde(default, rename = "not")]
    none: Vec<RawGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

impl RawGate {
    fn compile(self) -> Result<Gate, String> {
        let compile = |patterns: Vec<String>| {
            patterns
                .iter()
                .map(|pattern| Regex::new(pattern).map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Gate {
            all: self.all.into_iter().map(RawGate::compile).collect::<Result<_, _>>()?,
            any: self.any.into_iter().map(RawGate::compile).collect::<Result<_, _>>()?,
            none: self.none.into_iter().map(RawGate::compile).collect::<Result<_, _>>()?,
            contains: self.contains.iter().map(|text| text.to_lowercase()).collect(),
            regex: compile(self.regex)?,
            line_regex: compile(self.line_regex)?,
        })
    }
}

/// Every condition of a gate must hold: each `contains` text (without case), each
/// `regex` on the whole region, each `line_regex` on some line, every gate in `all`,
/// one in `any` when it has any, and none in `not`.
struct Gate {
    all: Vec<Gate>,
    any: Vec<Gate>,
    none: Vec<Gate>,
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

impl Gate {
    fn matches(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|needle| lower.contains(needle.as_str()))
            && self.regex.iter().all(|regex| regex.is_match(text))
            && self.line_regex.iter().all(|regex| text.lines().any(|line| regex.is_match(line)))
            && self.all.iter().all(|gate| gate.matches(text, lower))
            && (self.any.is_empty() || self.any.iter().any(|gate| gate.matches(text, lower)))
            && !self.none.iter().any(|gate| gate.matches(text, lower))
    }
}

struct Input<'a> {
    screen: &'a str,
    title: &'a str,
    progress: &'a str,
}

/// The part of the screen a rule looks at.
enum Region {
    Whole,
    Title,
    Progress,
    BottomLines(usize),
    BottomNonEmpty(usize),
    TopNonEmpty(usize),
    AfterLastRule,
    PromptBoxBody,
    AbovePromptBox,
    LastAbovePromptBox,
    AfterLastPrompt,
    BeforeCurrentPrompt,
    WholeWithoutCurrentPrompt,
    CurrentPromptBlockMarker,
    AfterCurrentPromptBlockMarker,
}

impl Region {
    fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let counted = |name: &str| {
            spec.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('('))
                .and_then(|rest| rest.strip_suffix(')'))
                .and_then(|count| count.parse::<usize>().ok())
        };
        Ok(match spec {
            "whole_recent" => Self::Whole,
            "osc_title" => Self::Title,
            "osc_progress" => Self::Progress,
            "after_last_horizontal_rule" => Self::AfterLastRule,
            "prompt_box_body" => Self::PromptBoxBody,
            "above_prompt_box" => Self::AbovePromptBox,
            "last_non_empty_above_prompt_box" => Self::LastAbovePromptBox,
            "after_last_prompt_marker" => Self::AfterLastPrompt,
            "before_current_prompt_marker" => Self::BeforeCurrentPrompt,
            "whole_recent_without_current_prompt_marker" => Self::WholeWithoutCurrentPrompt,
            "current_prompt_block_marker" => Self::CurrentPromptBlockMarker,
            "after_current_prompt_block_marker" => Self::AfterCurrentPromptBlockMarker,
            _ => {
                if let Some(count) = counted("bottom_non_empty_lines") {
                    Self::BottomNonEmpty(count)
                } else if let Some(count) = counted("bottom_lines") {
                    Self::BottomLines(count)
                } else if let Some(count) = counted("top_non_empty_lines") {
                    Self::TopNonEmpty(count)
                } else {
                    return Err(format!("unknown region {spec}"));
                }
            },
        })
    }
}

fn region<'a>(input: &Input<'a>, region: &Region) -> &'a str {
    let screen = input.screen;
    match region {
        Region::Whole => screen,
        Region::Title => input.title,
        Region::Progress => input.progress,
        Region::BottomLines(count) => {
            let lines: Vec<&str> = screen.lines().collect();
            from_line(screen, lines.len().saturating_sub(*count))
        },
        Region::BottomNonEmpty(count) => bottom_non_empty_lines(screen, *count),
        Region::TopNonEmpty(count) => {
            let end = screen
                .lines()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
                .take(*count)
                .last()
                .map_or(0, |(index, _)| index + 1);
            &screen[..line_offset(screen, end)]
        },
        Region::AfterLastRule => {
            let lines: Vec<&str> = screen.lines().collect();
            match lines.iter().rposition(|line| horizontal_rule(line)) {
                Some(index) => from_line(screen, index + 1),
                None => screen,
            }
        },
        Region::PromptBoxBody => {
            let lines: Vec<&str> = screen.lines().collect();
            let Some(top) = prompt_box_top(&lines) else {
                return "";
            };
            let end = lines[top + 1..]
                .iter()
                .position(|line| horizontal_rule(line))
                .map_or(lines.len(), |relative| top + 1 + relative);
            &screen[line_offset(screen, top + 1)..line_offset(screen, end)]
        },
        Region::AbovePromptBox => above_prompt_box(screen),
        Region::LastAbovePromptBox => {
            above_prompt_box(screen).lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("")
        },
        Region::AfterLastPrompt => {
            let lines: Vec<&str> = screen.lines().collect();
            match lines.iter().rposition(|line| codex_prompt(line)) {
                Some(index) => from_line(screen, index + 1),
                None => screen,
            }
        },
        Region::BeforeCurrentPrompt => {
            let lines: Vec<&str> = screen.lines().collect();
            match current_codex_prompt(&lines) {
                Some(index) => &screen[..line_offset(screen, index)],
                None => screen,
            }
        },
        Region::WholeWithoutCurrentPrompt => {
            let lines: Vec<&str> = screen.lines().collect();
            if current_codex_prompt(&lines).is_some() { "" } else { screen }
        },
        Region::CurrentPromptBlockMarker => {
            let lines: Vec<&str> = screen.lines().collect();
            current_codex_prompt(&lines)
                .and_then(|prompt| lines[..prompt].iter().rev().find(|line| codex_block_marker(line)).copied())
                .unwrap_or("")
        },
        Region::AfterCurrentPromptBlockMarker => {
            let lines: Vec<&str> = screen.lines().collect();
            current_codex_prompt(&lines)
                .and_then(|prompt| lines[..prompt].iter().rposition(|line| codex_block_marker(line)))
                .map_or("", |block| from_line(screen, block))
        },
    }
}

fn bottom_non_empty_lines(screen: &str, count: usize) -> &str {
    let lines: Vec<&str> = screen.lines().collect();
    match lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
    {
        Some((index, _)) => from_line(screen, index),
        None => "",
    }
}

fn above_prompt_box(screen: &str) -> &str {
    let lines: Vec<&str> = screen.lines().collect();
    match prompt_box_top(&lines) {
        Some(top) => &screen[..line_offset(screen, top)],
        None => screen,
    }
}

/// The top border of the input box: the second horizontal rule from the bottom.
fn prompt_box_top(lines: &[&str]) -> Option<usize> {
    lines.iter().enumerate().rev().filter(|(_, line)| horizontal_rule(line)).nth(1).map(|(index, _)| index)
}

/// A line drawn with `─`, alone or as at least three before a label.
fn horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let dashes = trimmed.chars().take_while(|ch| *ch == '─').count();
    if dashes == 0 {
        return false;
    }
    let rest = trimmed[dashes * '─'.len_utf8()..].trim_start();
    rest.is_empty() || dashes >= 3
}

fn codex_prompt(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker(line: &str) -> bool {
    line.starts_with(['•', '■', '✗', '✓'])
}

/// Codex's prompt line, when no new block of output has started under it.
fn current_codex_prompt(lines: &[&str]) -> Option<usize> {
    let prompt = lines.iter().rposition(|line| codex_prompt(line))?;
    (!lines[prompt + 1..].iter().any(|line| codex_block_marker(line))).then_some(prompt)
}

/// Byte offset of line `index` (or the end) in `text`.
fn line_offset(text: &str, index: usize) -> usize {
    text.split_inclusive('\n').take(index).map(str::len).sum::<usize>().min(text.len())
}

fn from_line(text: &str, index: usize) -> &str {
    &text[line_offset(text, index)..]
}

/// How long a turn must look plainly idle before it counts as over.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// The state each agent's terminal shows, read again only when its screen or title
/// changed. A reading that cannot tell keeps the previous state, and work that seems
/// to stop without the agent's prompt showing must stay stopped for [`SETTLE`].
#[derive(Default)]
pub struct Readings {
    tabs: HashMap<uuid::Uuid, Tracked>,
}

struct Tracked {
    revision: u64,
    heading: u64,
    reading: Reading,
    shown: State,
    idle_since: Option<std::time::Instant>,
}

impl Readings {
    /// Terminal `tab`'s state now: `revision` is its screen's, and `live` reads the
    /// screen when that or the title changed. Call it again while a turn settles.
    pub fn update(
        &mut self,
        tab: uuid::Uuid,
        agent: &str,
        revision: u64,
        title: &str,
        live: impl FnOnce() -> basalt_core::terminal::LiveText,
        now: std::time::Instant,
    ) -> Reading {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (agent, title).hash(&mut hasher);
        let heading = hasher.finish();
        let tracked = self.tabs.entry(tab).or_insert(Tracked {
            revision: revision.wrapping_add(1),
            heading,
            reading: Reading { state: State::Idle, plain: false },
            shown: State::Idle,
            idle_since: None,
        });
        if tracked.revision != revision || tracked.heading != heading {
            let live = live();
            if let Some(reading) = read(agent, &live.text, title, &live.progress) {
                tracked.reading = reading;
            }
            tracked.revision = revision;
            tracked.heading = heading;
        }
        let reading = tracked.reading;
        if reading.state == State::Idle && reading.plain && tracked.shown == State::Working {
            let since = *tracked.idle_since.get_or_insert(now);
            if now.saturating_duration_since(since) < SETTLE {
                return Reading { state: State::Working, plain: false };
            }
        }
        tracked.idle_since = None;
        tracked.shown = reading.state;
        reading
    }

    pub fn forget(&mut self, tab: uuid::Uuid) {
        self.tabs.remove(&tab);
    }
}
