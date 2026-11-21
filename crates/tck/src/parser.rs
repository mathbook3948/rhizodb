use std::error::Error;
use std::fmt;

use crate::steps::RawStep;

/// A scenario or outline as written in a feature file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenarioBlock {
    /// Scenario name.
    pub name: String,
    /// Tags on the scenario, with the leading `@`.
    pub tags: Vec<String>,
    /// Steps in order.
    pub steps: Vec<RawStep>,
    /// Whether the block is a `Scenario Outline`.
    pub outline: bool,
    /// Example tables of an outline.
    pub examples: Vec<ExamplesBlock>,
}

/// An `Examples:` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExamplesBlock {
    /// Tags on the table, with the leading `@`.
    pub tags: Vec<String>,
    /// Header row followed by the example rows.
    pub rows: Vec<Vec<String>>,
}

/// A parsed feature file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureFile {
    /// Tags on the feature, with the leading `@`.
    pub tags: Vec<String>,
    /// Steps shared by every scenario.
    pub background: Vec<RawStep>,
    /// Scenarios and outlines in file order.
    pub scenarios: Vec<ScenarioBlock>,
}

/// A line the feature parser does not understand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureSyntaxError {
    /// One-based line number.
    pub line: usize,
    /// What is wrong with the line.
    pub message: String,
}

impl fmt::Display for FeatureSyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl Error for FeatureSyntaxError {}

const STEP_KEYWORDS: [&str; 5] = ["Given", "When", "Then", "And", "But"];

enum Section {
    Header,
    Background,
    Scenario,
    Examples,
}

/// Parses the Gherkin subset the TCK uses: tags, `Feature`, `Background`, `Scenario`,
/// `Scenario Outline`, `Examples`, steps with doc strings or tables, and `#` comments.
/// Anything else is a syntax error, so a new TCK construct cannot be skipped silently.
pub fn parse_feature(text: &str) -> Result<FeatureFile, FeatureSyntaxError> {
    let mut file = FeatureFile::default();
    let mut section = Section::Header;
    let mut pending_tags: Vec<String> = Vec::new();
    let mut in_description = true;
    let mut lines = text.lines().enumerate().peekable();
    while let Some((index, raw)) = lines.next() {
        let line = index + 1;
        let fail = |message: &str| FeatureSyntaxError {
            line,
            message: message.to_string(),
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('@') {
            pending_tags.extend(trimmed.split_whitespace().map(str::to_string));
            continue;
        }
        let keyword_line = trimmed.starts_with("Feature:")
            || trimmed.starts_with("Background:")
            || trimmed.starts_with("Scenario:")
            || trimmed.starts_with("Scenario Outline:")
            || trimmed.starts_with("Examples:");
        if keyword_line {
            in_description = true;
        }
        if trimmed.starts_with("Feature:") {
            file.tags = std::mem::take(&mut pending_tags);
            section = Section::Header;
        } else if trimmed.starts_with("Background:") {
            section = Section::Background;
        } else if let Some(name) = trimmed.strip_prefix("Scenario Outline:") {
            file.scenarios
                .push(new_block(name, true, std::mem::take(&mut pending_tags)));
            section = Section::Scenario;
        } else if let Some(name) = trimmed.strip_prefix("Scenario:") {
            file.scenarios
                .push(new_block(name, false, std::mem::take(&mut pending_tags)));
            section = Section::Scenario;
        } else if trimmed.starts_with("Examples:") {
            let block = file
                .scenarios
                .last_mut()
                .filter(|block| block.outline)
                .ok_or_else(|| fail("Examples outside a Scenario Outline"))?;
            block.examples.push(ExamplesBlock {
                tags: std::mem::take(&mut pending_tags),
                rows: Vec::new(),
            });
            section = Section::Examples;
        } else if trimmed.starts_with('|') {
            in_description = false;
            let row = split_row(trimmed);
            let rows = match section {
                Section::Examples => file
                    .scenarios
                    .last_mut()
                    .and_then(|block| block.examples.last_mut())
                    .map(|examples| &mut examples.rows),
                Section::Background => file.background.last_mut().map(table_of),
                Section::Scenario => file
                    .scenarios
                    .last_mut()
                    .and_then(|block| block.steps.last_mut())
                    .map(table_of),
                Section::Header => None,
            };
            rows.ok_or_else(|| fail("table without a step or Examples"))?
                .push(row);
        } else if trimmed == "\"\"\"" {
            in_description = false;
            let indent = raw.len() - raw.trim_start().len();
            let mut content: Vec<String> = Vec::new();
            loop {
                let (_, next) = lines
                    .next()
                    .ok_or_else(|| fail("doc string is never closed"))?;
                if next.trim() == "\"\"\"" {
                    break;
                }
                content.push(strip_indent(next, indent).to_string());
            }
            let step = match section {
                Section::Background => file.background.last_mut(),
                Section::Scenario => file
                    .scenarios
                    .last_mut()
                    .and_then(|block| block.steps.last_mut()),
                Section::Header | Section::Examples => None,
            };
            step.ok_or_else(|| fail("doc string without a step"))?
                .docstring = Some(content.join("\n"));
        } else if let Some(text) = step_text(trimmed) {
            in_description = false;
            let step = RawStep {
                text: text.to_string(),
                docstring: None,
                table: None,
                line,
            };
            match section {
                Section::Background => file.background.push(step),
                Section::Scenario => file
                    .scenarios
                    .last_mut()
                    .ok_or_else(|| fail("step outside a scenario"))?
                    .steps
                    .push(step),
                Section::Header | Section::Examples => return Err(fail("step outside a scenario")),
            }
        } else if !in_description {
            return Err(fail("unrecognized line"));
        }
    }
    Ok(file)
}

fn new_block(name: &str, outline: bool, tags: Vec<String>) -> ScenarioBlock {
    ScenarioBlock {
        name: name.trim().to_string(),
        tags,
        steps: Vec::new(),
        outline,
        examples: Vec::new(),
    }
}

fn table_of(step: &mut RawStep) -> &mut Vec<Vec<String>> {
    step.table.get_or_insert_with(Vec::new)
}

fn step_text(line: &str) -> Option<&str> {
    let (word, rest) = line.split_once(' ')?;
    STEP_KEYWORDS.contains(&word).then(|| rest.trim())
}

fn strip_indent(line: &str, indent: usize) -> &str {
    let leading = line.len() - line.trim_start().len();
    &line[leading.min(indent)..]
}

fn split_row(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().chars().peekable();
    chars.next();
    while let Some(c) = chars.next() {
        match c {
            '|' => {
                cells.push(cell.trim().to_string());
                cell.clear();
            }
            '\\' => match chars.peek() {
                Some('|') => {
                    cell.push('|');
                    chars.next();
                }
                Some('\\') => {
                    cell.push('\\');
                    chars.next();
                }
                Some('n') => {
                    cell.push('\n');
                    chars.next();
                }
                _ => cell.push('\\'),
            },
            other => cell.push(other),
        }
    }
    cells
}

#[cfg(test)]
#[path = "parser.test.rs"]
mod tests;
