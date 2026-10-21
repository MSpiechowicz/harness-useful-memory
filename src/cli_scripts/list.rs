//! `scripts list --json` (documented with the other script commands in [`crate::cli_trust`]).

use std::cmp::Ordering;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::{Opened, hidden};
use crate::backend::Place;
use crate::cli::{Args, PAGE_OPTIONS, PICK_VALUED, Page, Repos};
use crate::cli_trust::usage;
use crate::limits::QUERY_MAX;
use crate::mcp::{Approval, place_label};
use crate::store::{Scope, ScriptSummary};

/// `scripts list`.
pub(crate) fn list(args: &[Option<&str>]) -> ExitCode {
    let Some(options) = parse_list(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(&options.repos) else {
        return ExitCode::FAILURE;
    };

    let page = &options.page;

    if page.query.chars().count() > QUERY_MAX {
        return opened.failed(&format!("query is longer than {QUERY_MAX} characters"));
    }

    let query = (!page.query.is_empty()).then_some(page.query.as_str());
    let listed = if options.hidden {
        hidden_rows(&mut opened, query)
    } else {
        opened.list(query)
    };

    let mut listed = match listed {
        Ok(listed) => listed,
        Err(message) => return opened.failed(&message),
    };

    let place_of = |summary: &ScriptSummary| opened.spot(summary.id).0;
    let place_keeps = |summary: &ScriptSummary| options.place.is_none_or(|place| place_of(summary) == place);

    let count = |keeps: &dyn Fn(&ScriptSummary, Approval) -> bool| {
        listed
            .iter()
            .filter(|(summary, approval)| keeps(summary, *approval))
            .count()
    };
    let in_group =
        |group: Group| count(&|summary, approval| Group::of(summary, approval) == group && place_keeps(summary));
    let in_scope = |scope: Scope| {
        count(&|summary, approval| summary.scope == scope && options.review_keeps(approval) && place_keeps(summary))
    };
    let in_review = |review: Review| {
        count(&|summary, approval| {
            Review::of(approval) == review && options.scope_keeps(summary) && place_keeps(summary)
        })
    };
    let in_place =
        |place: Place| count(&|summary, approval| place_of(summary) == place && options.keeps(summary, approval));

    let groups = json!({
        "needsReview": in_group(Group::NeedsReview),
        "repo": in_group(Group::Repo),
        "global": in_group(Group::Global),
    });
    let scopes = json!({
        "repo": in_scope(Scope::Repo),
        "global": in_scope(Scope::Global),
    });
    let review = json!({
        "needed": in_review(Review::Needed),
        "trusted": in_review(Review::Trusted),
    });
    let places = json!({
        place_label(Place::Local): in_place(Place::Local),
        place_label(Place::Remote): in_place(Place::Remote),
    });

    listed.retain(|(summary, approval)| options.keeps(summary, *approval) && place_keeps(summary));
    listed.sort_by(|(a, _), (b, _)| options.compare(a, b, |summary| place_of(summary) == Place::Local));

    let objects: Vec<Value> = listed
        .iter()
        .skip(page.offset)
        .take(page.limit)
        .map(|(summary, approval)| opened.json(summary, *approval, options.hidden))
        .collect();

    let mut shown = json!({
        "total": listed.len(),
        "groups": groups,
        "scopes": scopes,
        "review": review,
        "places": places,
        "scripts": objects,
    });

    if let Err(error) = opened.add_team_counts(&mut shown) {
        return opened.failed(&error.to_string());
    }

    println!("{shown}");
    ExitCode::SUCCESS
}

/// The hidden team scripts of the view `query` keeps, with how this machine may run them.
fn hidden_rows(opened: &mut Opened, query: Option<&str>) -> Result<Vec<(ScriptSummary, Approval)>, String> {
    let summaries = hidden::listed(opened, query).map_err(|error| error.to_string())?;

    summaries
        .into_iter()
        .map(|summary| {
            let approval = opened.approval(&summary)?;
            Ok((summary, approval))
        })
        .collect()
}

/// The options of `scripts list --json`.
#[derive(Debug)]
pub(crate) struct ListOptions {
    group: Option<Group>,
    /// `--scope`; never given with [`Self::group`].
    scope: Option<Scope>,
    /// `--review`; never given with [`Self::group`].
    review: Option<Review>,
    /// `--place`: the scripts kept on this computer or in the team database.
    place: Option<Place>,
    /// `--hidden`: the hidden team scripts instead of the visible ones.
    hidden: bool,
    repos: Repos,
    sort: ScriptSort,
    descending: bool,
    page: Page,
}

impl ListOptions {
    /// True when the group, the scope and the review asked for keep the script.
    fn keeps(&self, summary: &ScriptSummary, approval: Approval) -> bool {
        let in_group = self.group.is_none_or(|group| Group::of(summary, approval) == group);

        in_group && self.scope_keeps(summary) && self.review_keeps(approval)
    }

    /// True when the scope asked for (if any) keeps the script.
    fn scope_keeps(&self, summary: &ScriptSummary) -> bool {
        self.scope.is_none_or(|scope| summary.scope == scope)
    }

    /// True when the review asked for (if any) keeps a script with `approval`.
    fn review_keeps(&self, approval: Approval) -> bool {
        self.review.is_none_or(|review| Review::of(approval) == review)
    }

    /// The order of the list: by [`Self::sort`], then by id, turned round when descending; a
    /// script never run comes after the others by `lastRun` either way. `local` says whether a
    /// listed script is kept on this computer.
    fn compare(&self, a: &ScriptSummary, b: &ScriptSummary, local: impl Fn(&ScriptSummary) -> bool) -> Ordering {
        let is_repo = |summary: &ScriptSummary| summary.scope == Scope::Repo;

        let ascending = match self.sort {
            ScriptSort::Name => a
                .name
                .cmp(&b.name)
                .then(is_repo(b).cmp(&is_repo(a)))
                .then(local(b).cmp(&local(a))),
            ScriptSort::Language => a.language.as_str().cmp(b.language.as_str()),
            ScriptSort::Scope => is_repo(a).cmp(&is_repo(b)),
            ScriptSort::Place => local(b).cmp(&local(a)),
            ScriptSort::Runs => a.run_count.cmp(&b.run_count),
            ScriptSort::LastRun => match (a.last_run_at, b.last_run_at) {
                (Some(a_run), Some(b_run)) => a_run.cmp(&b_run),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            ScriptSort::Updated => a.updated_at.cmp(&b.updated_at),
        }
        .then(a.id.cmp(&b.id));

        if self.descending {
            ascending.reverse()
        } else {
            ascending
        }
    }
}

/// What `scripts list` sorts by (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptSort {
    Name,
    Language,
    Scope,
    Place,
    Runs,
    LastRun,
    Updated,
}

impl ScriptSort {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "name" => Some(Self::Name),
            "language" => Some(Self::Language),
            "scope" => Some(Self::Scope),
            "place" => Some(Self::Place),
            "runs" => Some(Self::Runs),
            "lastRun" => Some(Self::LastRun),
            "updated" => Some(Self::Updated),
            _ => None,
        }
    }
}

/// The group of a listed script: every script that needs approval, else its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    NeedsReview,
    Repo,
    Global,
}

impl Group {
    fn of(summary: &ScriptSummary, approval: Approval) -> Self {
        if !approval.is_trusted() {
            return Self::NeedsReview;
        }

        match summary.scope {
            Scope::Repo => Self::Repo,
            Scope::Global => Self::Global,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "needsReview" => Some(Self::NeedsReview),
            "repo" => Some(Self::Repo),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

/// Whether a listed script needs approval (the [`Group::NeedsReview`] scripts) or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Review {
    Needed,
    Trusted,
}

impl Review {
    fn of(approval: Approval) -> Self {
        if approval.is_trusted() {
            Self::Trusted
        } else {
            Self::Needed
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "needed" => Some(Self::Needed),
            "trusted" => Some(Self::Trusted),
            _ => None,
        }
    }
}

/// The options after `scripts`; `None` for anything but `list --json` with known, unrepeated
/// options and valid values, and for a group (`--group`, `--untrusted`) with `--scope` or
/// `--review`.
fn parse_list(args: &[Option<&str>]) -> Option<ListOptions> {
    let valued = [
        &PAGE_OPTIONS[..],
        &PICK_VALUED[..],
        &["--group", "--scope", "--place", "--review", "--sort"],
    ]
    .concat();
    let parsed = Args::parse(args, &["--json", "--untrusted", "--hidden", "--all-repos"], &valued)?;

    if parsed.positional != ["list"] || !parsed.flag("--json") {
        return None;
    }

    let group = match (parsed.flag("--untrusted"), parsed.value("--group")) {
        (true, Some(_)) => return None,
        (true, None) => Some(Group::NeedsReview),
        (false, Some(raw)) => Some(Group::parse(raw)?),
        (false, None) => None,
    };

    let scope = parsed.scope().ok()?;
    let review = match parsed.value("--review") {
        Some(raw) => Some(Review::parse(raw)?),
        None => None,
    };

    if group.is_some() && (scope.is_some() || review.is_some()) {
        return None;
    }

    let sort = match parsed.value("--sort") {
        Some(raw) => ScriptSort::parse(raw)?,
        None => ScriptSort::Name,
    };

    let page = Page::of(&parsed)?;

    Some(ListOptions {
        group,
        scope,
        review,
        place: parsed.place().ok()?,
        hidden: parsed.flag("--hidden"),
        repos: parsed.repos().ok()?,
        sort,
        descending: page.descending.unwrap_or(sort == ScriptSort::Updated),
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn list_options_need_json_and_take_a_project() {
        let parsed = parse_list(&args(&["list", "--json", "--project", "/w", "--untrusted"])).expect("valid");
        assert_eq!(parsed.group, Some(Group::NeedsReview));
        assert_eq!((parsed.sort, parsed.descending), (ScriptSort::Name, false));
        assert_eq!(parsed.repos, Repos::Project(Some("/w".to_owned())));

        for (raw, sort) in [
            ("language", ScriptSort::Language),
            ("scope", ScriptSort::Scope),
            ("place", ScriptSort::Place),
            ("runs", ScriptSort::Runs),
            ("lastRun", ScriptSort::LastRun),
        ] {
            let parsed = parse_list(&args(&["list", "--json", "--sort", raw])).expect("valid");
            assert_eq!((parsed.sort, parsed.descending), (sort, false), "{raw}");
        }

        let filtered =
            parse_list(&args(&["list", "--json", "--scope", "global", "--review", "needed"])).expect("valid");
        assert_eq!(
            (filtered.group, filtered.scope, filtered.review),
            (None, Some(Scope::Global), Some(Review::Needed))
        );

        assert_eq!(filtered.place, None);

        for (raw, place) in [("local", Place::Local), ("team", Place::Remote)] {
            let parsed = parse_list(&args(&["list", "--json", "--place", raw])).expect("valid");
            assert_eq!(parsed.place, Some(place), "{raw}");
        }

        let grouped = parse_list(&args(&["list", "--json", "--untrusted", "--place=team", "--hidden"])).expect("valid");
        assert_eq!(
            (grouped.group, grouped.place, grouped.hidden),
            (Some(Group::NeedsReview), Some(Place::Remote), true),
            "a place combines with a group"
        );

        let updated = parse_list(&args(&["list", "--json", "--group", "repo", "--sort", "updated"])).expect("valid");
        assert_eq!(
            (updated.group, updated.sort, updated.descending),
            (Some(Group::Repo), ScriptSort::Updated, true)
        );
        assert_eq!(
            parse_list(&args(&["list", "--json", "--project=/w"]))
                .expect("valid")
                .repos,
            Repos::Project(Some("/w".to_owned()))
        );
        assert_eq!(
            parse_list(&args(&["list", "--json", "--all-repos"]))
                .expect("valid")
                .repos,
            Repos::Every
        );
        assert_eq!(
            parse_list(&args(&["list", "--json", "--repo", "git:h/o/r"]))
                .expect("valid")
                .repos,
            Repos::Key("git:h/o/r".to_owned())
        );

        for bad in [
            &["list"][..],
            &["--json"],
            &["show", "--json"],
            &["list", "--json", "--json"],
            &["list", "--json", "--project"],
            &["list", "--json", "--project", "/a", "--project", "/b"],
            &["list", "--json", "--all-repos", "--project", "/a"],
            &["list", "--json", "--repo", "git:h/o/r", "--all-repos"],
            &["list", "--json", "--repo", " "],
            &["list", "--json", "--yaml"],
            &["list", "--json", "--untrusted", "--group", "repo"],
            &["list", "--json", "--untrusted", "--scope", "repo"],
            &["list", "--json", "--group", "repo", "--review", "trusted"],
            &["list", "--json", "--scope", "both"],
            &["list", "--json", "--review", "pending"],
            &["list", "--json", "--group", "trusted"],
            &["list", "--json", "--sort", "size"],
            &["list", "--json", "--limit", "500"],
            &["list", "--json", "--place", "remote"],
            &["list", "--json", "--place", "Local"],
            &["list", "--json", "--place", ""],
            &["list", "--json", "--place"],
            &["list", "--json", "--place", "local", "--place", "team"],
        ] {
            assert!(parse_list(&args(bad)).is_none(), "{bad:?}");
        }
    }
}
