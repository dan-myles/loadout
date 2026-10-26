use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{Update, report};
use crate::diagnostic::{Diagnostics, Severity};
use crate::machine::Facts;
use crate::manifest::{
    Condition, CustomAgent, FilePatch, InstructionsPatch, LayerFile, List, ListEdit, Loadout,
    Manager, McpPatch, ProgramPatch, ROOT_FILE, Scope, SkillPatch, Value, Variable,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Edits<T> {
    base: Option<Vec<T>>,
    edits: Vec<ListEdit<T>>,
}

impl<T> Default for Edits<T> {
    fn default() -> Self {
        Self {
            base: None,
            edits: Vec::new(),
        }
    }
}

impl<T: Clone + PartialEq> Edits<T> {
    fn apply(&mut self, list: &List<T>) {
        match list {
            List::Assign(values) => {
                self.base = Some(values.clone());
                self.edits.clear();
            }
            List::Edit(edit) => self.edits.push(edit.clone()),
        }
    }

    pub(super) fn is_set(&self) -> bool {
        self.base.is_some() || !self.edits.is_empty()
    }

    /// `default` stands in for a list no layer assigned, so an edit of an item's unset `agents`
    /// edits the machine's enabled agents.
    pub(super) fn finish(&self, default: &[T]) -> Vec<T> {
        let mut values = self.base.clone().unwrap_or_else(|| default.to_vec());
        for edit in &self.edits {
            values.retain(|value| !edit.remove.contains(value));
            for value in &edit.add {
                if !values.contains(value) {
                    values.push(value.clone());
                }
            }
        }
        values
    }
}

pub(super) trait Draft: Default {
    type Patch;
    const KIND: &'static str;
    fn flags(patch: &Self::Patch) -> (bool, bool);
    fn edit(&mut self, patch: &Self::Patch);
    fn missing(&self) -> Option<&'static str>;
    fn when(&self) -> Option<&Condition>;
}

fn set<T: Clone>(field: &mut Option<T>, value: Option<&T>) {
    if let Some(value) = value {
        *field = Some(value.clone());
    }
}

#[derive(Debug, Default)]
pub(super) struct FileDraft {
    pub(super) source: Option<String>,
    pub(super) target: Option<String>,
    pub(super) when: Option<Condition>,
}

impl Draft for FileDraft {
    type Patch = FilePatch;
    const KIND: &'static str = "files";

    fn flags(patch: &FilePatch) -> (bool, bool) {
        (patch.remove, patch.replace)
    }

    fn edit(&mut self, patch: &FilePatch) {
        set(&mut self.source, patch.source.as_ref());
        set(&mut self.target, patch.target.as_ref());
        set(&mut self.when, patch.when.as_ref());
    }

    fn missing(&self) -> Option<&'static str> {
        if self.source.is_none() {
            Some("source")
        } else if self.target.is_none() {
            Some("target")
        } else {
            None
        }
    }

    fn when(&self) -> Option<&Condition> {
        self.when.as_ref()
    }
}

#[derive(Debug, Default)]
pub(super) struct SourceDraft {
    pub(super) source: Option<String>,
    pub(super) agents: Edits<String>,
    pub(super) when: Option<Condition>,
}

impl SourceDraft {
    fn edit(
        &mut self,
        source: Option<&String>,
        agents: Option<&List<String>>,
        when: Option<&Condition>,
    ) {
        set(&mut self.source, source);
        if let Some(agents) = agents {
            self.agents.apply(agents);
        }
        set(&mut self.when, when);
    }

    fn missing(&self) -> Option<&'static str> {
        self.source.is_none().then_some("source")
    }
}

#[derive(Debug, Default)]
pub(super) struct SkillDraft(pub(super) SourceDraft);

impl Draft for SkillDraft {
    type Patch = SkillPatch;
    const KIND: &'static str = "skills";

    fn flags(patch: &SkillPatch) -> (bool, bool) {
        (patch.remove, patch.replace)
    }

    fn edit(&mut self, patch: &SkillPatch) {
        self.0.edit(
            patch.source.as_ref(),
            patch.agents.as_ref(),
            patch.when.as_ref(),
        );
    }

    fn missing(&self) -> Option<&'static str> {
        self.0.missing()
    }

    fn when(&self) -> Option<&Condition> {
        self.0.when.as_ref()
    }
}

#[derive(Debug, Default)]
pub(super) struct InstructionsDraft(pub(super) SourceDraft);

impl Draft for InstructionsDraft {
    type Patch = InstructionsPatch;
    const KIND: &'static str = "instructions";

    fn flags(patch: &InstructionsPatch) -> (bool, bool) {
        (patch.remove, patch.replace)
    }

    fn edit(&mut self, patch: &InstructionsPatch) {
        self.0.edit(
            patch.source.as_ref(),
            patch.agents.as_ref(),
            patch.when.as_ref(),
        );
    }

    fn missing(&self) -> Option<&'static str> {
        self.0.missing()
    }

    fn when(&self) -> Option<&Condition> {
        self.0.when.as_ref()
    }
}

#[derive(Debug, Default)]
pub(super) struct McpDraft {
    pub(super) command: Option<String>,
    pub(super) args: Edits<String>,
    pub(super) env: Option<BTreeMap<String, Value>>,
    pub(super) url: Option<String>,
    pub(super) headers: Option<BTreeMap<String, Value>>,
    pub(super) agents: Edits<String>,
    pub(super) when: Option<Condition>,
}

impl Draft for McpDraft {
    type Patch = McpPatch;
    const KIND: &'static str = "mcp";

    fn flags(patch: &McpPatch) -> (bool, bool) {
        (patch.remove, patch.replace)
    }

    fn edit(&mut self, patch: &McpPatch) {
        set(&mut self.command, patch.command.as_ref());
        if let Some(args) = &patch.args {
            self.args.apply(args);
        }
        set(&mut self.env, patch.env.as_ref());
        set(&mut self.url, patch.url.as_ref());
        set(&mut self.headers, patch.headers.as_ref());
        if let Some(agents) = &patch.agents {
            self.agents.apply(agents);
        }
        set(&mut self.when, patch.when.as_ref());
    }

    fn missing(&self) -> Option<&'static str> {
        (self.command.is_none() && self.url.is_none())
            .then_some("command (stdio server) or url (remote server)")
    }

    fn when(&self) -> Option<&Condition> {
        self.when.as_ref()
    }
}

#[derive(Debug, Default)]
pub(super) struct ProgramDraft {
    pub(super) packages: BTreeMap<Manager, String>,
    pub(super) when: Option<Condition>,
}

impl Draft for ProgramDraft {
    type Patch = ProgramPatch;
    const KIND: &'static str = "programs";

    fn flags(patch: &ProgramPatch) -> (bool, bool) {
        (patch.remove, patch.replace)
    }

    fn edit(&mut self, patch: &ProgramPatch) {
        for (manager, package) in patch.packages() {
            self.packages.insert(manager, package.to_owned());
        }
        set(&mut self.when, patch.when.as_ref());
    }

    fn missing(&self) -> Option<&'static str> {
        self.packages
            .is_empty()
            .then_some("a package for at least one manager")
    }

    fn when(&self) -> Option<&Condition> {
        self.when.as_ref()
    }
}

#[derive(Debug, Default)]
pub(super) struct Item<D> {
    pub(super) draft: D,
    pub(super) layers: Vec<PathBuf>,
}

impl<D> Item<D> {
    pub(super) fn file(&self) -> Option<&Path> {
        self.layers.last().map(PathBuf::as_path)
    }

    pub(super) fn history(&self) -> String {
        self.layers
            .iter()
            .map(|layer| layer.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Default)]
pub(super) struct Merged {
    pub(super) tags: Vec<String>,
    pub(super) applied: Vec<PathBuf>,
    pub(super) variables: BTreeMap<String, Variable>,
    pub(super) update: Update,
    pub(super) enabled: Edits<String>,
    pub(super) enabled_file: Option<PathBuf>,
    pub(super) custom: BTreeMap<String, (CustomAgent, PathBuf)>,
    pub(super) files: BTreeMap<String, Item<FileDraft>>,
    pub(super) skills: BTreeMap<String, Item<SkillDraft>>,
    pub(super) instructions: BTreeMap<String, Item<InstructionsDraft>>,
    pub(super) mcp: BTreeMap<String, Item<McpDraft>>,
    pub(super) programs: BTreeMap<String, Item<ProgramDraft>>,
}

pub(super) fn merge(loadout: &Loadout, facts: &Facts, diagnostics: &mut Diagnostics) -> Merged {
    let mut merged = Merged::default();
    for file in layers(loadout, facts, &mut merged.tags, diagnostics) {
        let layer = &file.layer;
        let path = file.file.as_path();
        merged.applied.push(path.to_path_buf());
        merged.variables.extend(
            layer
                .variables
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        if let Some(update) = &layer.update {
            let current = &mut merged.update;
            current.auto = update.auto.unwrap_or(current.auto);
            current.interval_minutes = update.interval_minutes.unwrap_or(current.interval_minutes);
        }
        if let Some(agents) = &layer.agents {
            if let Some(enabled) = &agents.enabled {
                merged.enabled.apply(enabled);
                merged.enabled_file = Some(path.to_path_buf());
            }
            for (id, agent) in &agents.custom {
                merged
                    .custom
                    .insert(id.clone(), (agent.clone(), path.to_path_buf()));
            }
        }
        items(&mut merged.files, &layer.files, path, diagnostics);
        items(&mut merged.skills, &layer.skills, path, diagnostics);
        items(
            &mut merged.instructions,
            &layer.instructions,
            path,
            diagnostics,
        );
        items(&mut merged.mcp, &layer.mcp, path, diagnostics);
        items(&mut merged.programs, &layer.programs, path, diagnostics);
    }
    merged
}

fn layers<'a>(
    loadout: &'a Loadout,
    facts: &Facts,
    tags: &mut Vec<String>,
    diagnostics: &mut Diagnostics,
) -> Vec<&'a LayerFile> {
    let find = |scope: Scope| loadout.layers.iter().find(|file| file.scope == scope);
    let mut layers: Vec<&LayerFile> = [find(Scope::Global), find(Scope::Os(facts.os))]
        .into_iter()
        .flatten()
        .collect();
    let Some(entry) = loadout.machines.get(&facts.name) else {
        return layers;
    };
    for tag in &entry.tags {
        match find(Scope::Tag(tag.clone())) {
            Some(file) => layers.push(file),
            None => report(
                diagnostics,
                Severity::Error,
                Some(Path::new(ROOT_FILE)),
                format!("machines.{}.tags", facts.name),
                format!("tag {tag:?} has no layer file tags/{tag}.toml"),
            ),
        }
        tags.push(tag.clone());
    }
    layers.extend(find(Scope::Machine(facts.name.clone())));
    layers
}

fn items<D: Draft>(
    merged: &mut BTreeMap<String, Item<D>>,
    patches: &BTreeMap<String, D::Patch>,
    file: &Path,
    diagnostics: &mut Diagnostics,
) {
    for (id, patch) in patches {
        let key = format!("{}.{id}", D::KIND);
        let (remove, replace) = D::flags(patch);
        if remove {
            if merged.remove(id).is_none() {
                report(
                    diagnostics,
                    Severity::Warning,
                    Some(file),
                    key,
                    "removes an item that no broader layer declares for this machine",
                );
            }
            continue;
        }
        let added = replace || !merged.contains_key(id);
        if added {
            merged.insert(id.clone(), Item::default());
        }
        let item = merged.get_mut(id).expect("inserted above when absent");
        item.draft.edit(patch);
        item.layers.push(file.to_path_buf());
        if added && let Some(field) = item.draft.missing() {
            report(
                diagnostics,
                Severity::Error,
                Some(file),
                key,
                format!(
                    "adds this item here because no broader layer declares it for this machine, but it is incomplete: missing {field}"
                ),
            );
            merged.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(add: &[&str], remove: &[&str]) -> List<String> {
        List::Edit(ListEdit {
            add: add.iter().map(|value| (*value).to_owned()).collect(),
            remove: remove.iter().map(|value| (*value).to_owned()).collect(),
        })
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn an_edit_removes_first_then_appends_only_what_is_missing() {
        let mut list = Edits::default();
        list.apply(&List::Assign(strings(&["a", "b", "a"])));
        assert_eq!(list.finish(&[]), strings(&["a", "b", "a"]));
        list.apply(&edit(&["c", "b", "c"], &["a"]));
        assert_eq!(list.finish(&[]), strings(&["b", "c"]));
        list.apply(&edit(&["a"], &["a"]));
        assert_eq!(list.finish(&[]), strings(&["b", "c", "a"]));
        list.apply(&List::Assign(strings(&["z"])));
        assert_eq!(list.finish(&[]), strings(&["z"]));
    }

    #[test]
    fn an_unassigned_list_edits_the_default() {
        let mut list = Edits::default();
        assert!(!list.is_set());
        assert_eq!(list.finish(&strings(&["x", "y"])), strings(&["x", "y"]));
        list.apply(&edit(&["z"], &["x"]));
        assert!(list.is_set());
        assert_eq!(list.finish(&strings(&["x", "y"])), strings(&["y", "z"]));
    }
}
