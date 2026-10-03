//! The action palette: everything the picker can do, with the prompt (if
//! any) each action needs.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::cli::SortBy;

/// A prompt the picker can open for input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Add,
    Rename,
    AliasAdd,
    AliasRemove,
    TagAdd,
    TagRemove,
    Repath,
}

impl PromptKind {
    /// The label in front of the input line.
    pub fn label(self) -> &'static str {
        match self {
            PromptKind::Add => "add path: ",
            PromptKind::Rename => "new name: ",
            PromptKind::AliasAdd => "add alias: ",
            PromptKind::AliasRemove => "remove alias: ",
            PromptKind::TagAdd => "add tag: ",
            PromptKind::TagRemove => "remove tag: ",
            PromptKind::Repath => "new path: ",
        }
    }
}

/// Everything the palette offers, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Jump,
    Pin,
    Add,
    Remove,
    Rename,
    AliasAdd,
    AliasRemove,
    TagAdd,
    TagRemove,
    Details,
    SortUsed,
    SortName,
    SortCreated,
    ToggleMissing,
    SearchHome,
    MarkAll,
    UnmarkAll,
    Health,
    Migrate,
    Help,
    Quit,
}

impl Action {
    pub(crate) const ALL: [Action; 21] = [
        Action::Jump,
        Action::Pin,
        Action::Add,
        Action::Remove,
        Action::Rename,
        Action::AliasAdd,
        Action::AliasRemove,
        Action::TagAdd,
        Action::TagRemove,
        Action::Details,
        Action::SortUsed,
        Action::SortName,
        Action::SortCreated,
        Action::ToggleMissing,
        Action::SearchHome,
        Action::MarkAll,
        Action::UnmarkAll,
        Action::Health,
        Action::Migrate,
        Action::Help,
        Action::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Jump => "jump to the highlighted project",
            Action::Pin => "pin / unpin",
            Action::Add => "add a directory to the index",
            Action::Remove => "remove from the index",
            Action::Rename => "rename the project",
            Action::AliasAdd => "add an alias",
            Action::AliasRemove => "remove an alias",
            Action::TagAdd => "add a tag",
            Action::TagRemove => "remove a tag",
            Action::Details => "toggle details",
            Action::SortUsed => "sort by last used",
            Action::SortName => "sort by name",
            Action::SortCreated => "sort by created",
            Action::ToggleMissing => "hide / show missing projects",
            Action::SearchHome => "also search home directories",
            Action::MarkAll => "mark every visible project",
            Action::UnmarkAll => "unmark everything",
            Action::Health => "check the index's health (doctor)",
            Action::Migrate => "migrate the JSON index to SQLite",
            Action::Help => "key help",
            Action::Quit => "quit",
        }
    }

    /// The chord that does the same thing, when one exists.
    pub fn hint(self) -> &'static str {
        match self {
            Action::Jump => "enter",
            Action::Pin => "^p",
            Action::Add => "^a",
            Action::Remove => "^x",
            Action::Help => "?",
            Action::Quit => "esc",
            _ => "",
        }
    }

    /// The prompt this action opens, if it needs input.
    pub fn prompt(self) -> Option<PromptKind> {
        match self {
            Action::Add => Some(PromptKind::Add),
            Action::Rename => Some(PromptKind::Rename),
            Action::AliasAdd => Some(PromptKind::AliasAdd),
            Action::AliasRemove => Some(PromptKind::AliasRemove),
            Action::TagAdd => Some(PromptKind::TagAdd),
            Action::TagRemove => Some(PromptKind::TagRemove),
            _ => None,
        }
    }

    /// The sort order this action selects, if it is a sort action.
    pub fn sort(self) -> Option<SortBy> {
        match self {
            Action::SortUsed => Some(SortBy::Used),
            Action::SortName => Some(SortBy::Name),
            Action::SortCreated => Some(SortBy::Created),
            _ => None,
        }
    }
}

/// The palette entries matching `query`, best first; all of them (in
/// [`Action::ALL`] order) for an empty query.
pub fn palette_matches(query: &str) -> Vec<Action> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Action::ALL.to_vec();
    }

    let matcher = SkimMatcherV2::default().smart_case();
    let mut scored: Vec<(i64, Action)> = Action::ALL
        .iter()
        .filter_map(|&action| {
            matcher
                .fuzzy_match(action.label(), &query)
                .map(|score| (score, action))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label().cmp(b.1.label())));
    scored.into_iter().map(|(_, action)| action).collect()
}

/// The key list behind the `?` overlay: `(keys, description)`.
pub const HELP: [(&str, &str); 16] = [
    ("type", "filter the list"),
    ("enter", "jump to the highlighted project"),
    ("tab", "search mode / list mode"),
    (
        "j k ↑ ↓",
        "move the highlight (list mode); arrows work while typing",
    ),
    ("g G", "first / last row"),
    ("u d PgUp PgDn", "half-page moves"),
    ("^p", "pin / unpin (marked rows when any are marked)"),
    ("^a", "add a directory (query or cwd prefilled)"),
    ("^x", "remove from the index (asks first)"),
    ("m", "mark / unmark the highlighted row"),
    ("^space :", "action palette"),
    ("?", "this help"),
    ("esc ^c", "cancel, or close whatever is open"),
    ("wheel", "scroll the list"),
    ("click", "select the clicked row"),
    ("y / n", "answer a confirmation"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_lists_every_action_in_order() {
        let matches = palette_matches("");
        assert_eq!(matches.len(), Action::ALL.len());
        assert_eq!(matches[0], Action::Jump);
        assert_eq!(*matches.last().expect("last"), Action::Quit);
    }

    #[test]
    fn palette_queries_filter_fuzzily() {
        let matches = palette_matches("alias");
        assert_eq!(
            matches,
            vec![Action::AliasAdd, Action::AliasRemove],
            "both alias actions, alphabetical on identical scores"
        );

        assert!(palette_matches("zzzz").is_empty());
        assert_eq!(palette_matches("migr")[0], Action::Migrate);
    }

    #[test]
    fn prompts_and_sorts_are_wired() {
        assert_eq!(Action::Add.prompt(), Some(PromptKind::Add));
        assert_eq!(Action::Rename.prompt(), Some(PromptKind::Rename));
        assert_eq!(Action::Jump.prompt(), None);
        assert_eq!(Action::SortName.sort(), Some(SortBy::Name));
        assert_eq!(Action::Help.sort(), None);

        assert_eq!(PromptKind::Add.label(), "add path: ");
        assert_eq!(PromptKind::AliasRemove.label(), "remove alias: ");
    }

    #[test]
    fn every_action_has_a_label_and_the_help_list_is_filled() {
        for action in Action::ALL {
            assert!(!action.label().is_empty(), "{action:?}");
        }
        for (keys, description) in HELP {
            assert!(!keys.is_empty() && !description.is_empty());
        }
    }
}
