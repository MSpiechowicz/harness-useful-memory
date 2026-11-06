//! Listing notes and scripts by who added them: the filter ([`AuthorFilter`]) and the facet
//! ([`AuthorFacet`]) a list answers with, for a table that lets a person pick one name.
//!
//! The person is the git `user.name` the record was added under (`added_by_name`), whatever client
//! the write went through: `Ada` and `Ada via claude-code` are one person. Names match exactly,
//! case-sensitive and unfolded. A record without a known name (no git name, or a database without
//! the author columns) is unknown. The facet counts the records every filter but this one keeps,
//! so a picker shows what picking each name would leave; a sort by the person puts the unknown
//! records last in both directions ([`fold`] orders the names).

use std::collections::BTreeMap;

use crate::limits::AUTHOR_FACET_MAX;
use crate::store::team_docs::check_author;
use crate::store::{Author, Authors, StoreError};

/// Which records a listing keeps by who added them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AuthorFilter {
    /// Every record.
    #[default]
    Any,
    /// The records added by the person of this name (exactly, case-sensitive).
    Named(String),
    /// The records without a known author.
    Unknown,
}

impl AuthorFilter {
    /// The filter `name` (trimmed, a valid author name) or `unknown` asks for; an error for a name
    /// that is blank or not valid, and for both together.
    pub fn parse(name: Option<&str>, unknown: bool) -> Result<Self, StoreError> {
        match (name, unknown) {
            (Some(_), true) => Err(StoreError::Invalid(
                "an author and unknown authors cannot be asked for together".to_owned(),
            )),
            (Some(name), false) => check_author(name).map(Self::Named),
            (None, true) => Ok(Self::Unknown),
            (None, false) => Ok(Self::Any),
        }
    }

    /// The name asked for, when one was.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            Self::Any | Self::Unknown => None,
        }
    }

    /// True when a record with `authors` is kept. Without author columns nobody has an author, so
    /// a name keeps nothing and unknown keeps everything.
    pub fn keeps(&self, authors: &Authors) -> bool {
        let added_by = authors.added_by.as_ref().map(|author| author.name.as_str());

        match self {
            Self::Any => true,
            Self::Named(name) => added_by == Some(name.as_str()),
            Self::Unknown => added_by.is_none(),
        }
    }

    /// The SQL condition on the `added_by_name` column `column` keeping these records, with the
    /// statement's parameter `mark` bound to the name asked for (`NULL` when none was). The
    /// parameter appears in every form because the statements bind a fixed list of positional
    /// parameters. Without the author columns (`authors` false) no record has a name.
    pub(crate) fn condition(&self, column: &str, mark: &str, authors: bool) -> String {
        match (self, authors) {
            (Self::Any, _) | (Self::Unknown, false) => format!("{mark} IS NULL"),
            (Self::Named(_), true) => format!("{column} = {mark}"),
            (Self::Named(_), false) => format!("{mark} IS NULL AND 1 = 0"),
            (Self::Unknown, true) => format!("{mark} IS NULL AND ({column} IS NULL OR {column} = '')"),
        }
    }
}

/// The key a name is ordered by in a sort by author: ASCII letters folded, as SQLite's `lower()`.
pub(crate) fn fold(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// The name `author` added a record under, for ordering; `None` for an unknown author.
pub(crate) fn name_of(author: Option<&Author>) -> Option<&str> {
    author.map(|author| author.name.as_str())
}

/// Orders `people`: the most records first, then by folded name, then by name.
fn order(people: &mut [(String, usize)]) {
    people.sort_by(|(a, a_count), (b, b_count)| {
        b_count
            .cmp(a_count)
            .then_with(|| fold(a).cmp(&fold(b)))
            .then_with(|| a.cmp(b))
    });
}

/// Who added the records a listing keeps, apart from the author filter: a count per person and
/// the number of records without a known one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorFacet {
    people: BTreeMap<String, usize>,
    unknown: usize,
}

impl AuthorFacet {
    /// The facet of the groups `(name, count)` a database counted (`None` or blank for a record
    /// without a name). A name that is not a valid author name, as a database written by hand may
    /// hold, counts as unknown and is never kept.
    pub(crate) fn from_groups(groups: impl IntoIterator<Item = (Option<String>, usize)>) -> Self {
        let mut facet = Self::default();

        for (name, count) in groups {
            facet.add(name.as_deref(), count);
        }

        facet
    }

    /// The facet of records added by `added_by` each.
    pub(crate) fn of_records<'a>(added_by: impl IntoIterator<Item = Option<&'a Author>>) -> Self {
        Self::from_groups(
            added_by
                .into_iter()
                .map(|author| (name_of(author).map(str::to_owned), 1)),
        )
    }

    /// Counts `count` more records added by `name` (unknown for `None`, a blank or an invalid one).
    fn add(&mut self, name: Option<&str>, count: usize) {
        let valid = name.filter(|name| check_author(name).is_ok_and(|clean| clean == *name));

        match valid {
            Some(name) => *self.people.entry(name.to_owned()).or_default() += count,
            None => self.unknown += count,
        }
    }

    /// Adds the counts of `other` to these.
    pub(crate) fn merge(&mut self, other: Self) {
        for (name, count) in other.people {
            *self.people.entry(name).or_default() += count;
        }

        self.unknown += other.unknown;
    }

    /// The number of records without a known author.
    pub fn unknown(&self) -> usize {
        self.unknown
    }

    /// The people with their counts: the most records first, then by folded name, then by name.
    pub fn people(&self) -> Vec<(String, usize)> {
        let mut people: Vec<(String, usize)> = self.people.iter().map(|(name, count)| (name.clone(), *count)).collect();
        order(&mut people);

        people
    }

    /// At most [`AUTHOR_FACET_MAX`] people, and `selected` (the name the list is filtered by) among
    /// them whatever its count: when it is not among the most records its count is `total`, the
    /// records the whole list keeps (what filtering by it leaves, 0 for a name nobody added).
    pub fn capped(self, selected: Option<&str>, total: usize) -> Vec<(String, usize)> {
        let mut people = self.people();

        let Some(selected) = selected else {
            people.truncate(AUTHOR_FACET_MAX);
            return people;
        };

        if people.iter().take(AUTHOR_FACET_MAX).any(|(name, _)| name == selected) {
            people.truncate(AUTHOR_FACET_MAX);
            return people;
        }

        people.retain(|(name, _)| name != selected);
        people.truncate(AUTHOR_FACET_MAX - 1);
        people.push((selected.to_owned(), total));
        order(&mut people);

        people
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facet(groups: &[(Option<&str>, usize)]) -> AuthorFacet {
        AuthorFacet::from_groups(groups.iter().map(|(name, count)| (name.map(str::to_owned), *count)))
    }

    #[test]
    fn a_filter_takes_a_valid_name_or_unknown_but_not_both() {
        assert_eq!(AuthorFilter::parse(None, false).expect("any"), AuthorFilter::Any);
        assert_eq!(AuthorFilter::parse(None, true).expect("unknown"), AuthorFilter::Unknown);
        assert_eq!(
            AuthorFilter::parse(Some("  Ada Lovelace "), false).expect("named"),
            AuthorFilter::Named("Ada Lovelace".to_owned())
        );

        for bad in [Some(""), Some("   "), Some("Ada\nLovelace"), Some("Ada\u{200b}")] {
            assert!(AuthorFilter::parse(bad, false).is_err(), "{bad:?}");
        }

        assert!(AuthorFilter::parse(Some("Ada"), true).is_err());
    }

    #[test]
    fn groups_merge_by_exact_name_and_invalid_names_count_as_unknown() {
        let facet = facet(&[
            (Some("Ada"), 2),
            (Some("ada"), 1),
            (None, 4),
            (Some(""), 1),
            (Some("Bad\u{7}name"), 3),
            (Some(" padded "), 1),
        ]);

        assert_eq!(
            facet.people(),
            vec![("Ada".to_owned(), 2), ("ada".to_owned(), 1)],
            "case is kept, nothing invalid is listed"
        );
        assert_eq!(facet.unknown(), 9);
    }

    #[test]
    fn people_order_by_count_then_folded_name_then_name() {
        let people = facet(&[
            (Some("bob"), 1),
            (Some("Bob"), 1),
            (Some("Alice"), 1),
            (Some("Zed"), 5),
            (Some("carol"), 1),
        ])
        .people();

        let names: Vec<&str> = people.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["Zed", "Alice", "Bob", "bob", "carol"]);
    }

    #[test]
    fn the_cap_keeps_the_selected_name_with_the_total() {
        let many: Vec<(String, usize)> = (0..250)
            .map(|index| (format!("person {index:03}"), 250 - index))
            .collect();
        let many = AuthorFacet::from_groups(many.into_iter().map(|(name, count)| (Some(name), count)));

        let plain = many.clone().capped(None, 0);
        assert_eq!(plain.len(), AUTHOR_FACET_MAX);
        assert_eq!(plain[0].0, "person 000");

        let inside = many.clone().capped(Some("person 005"), 245);
        assert_eq!(inside, plain);

        let beyond = many.clone().capped(Some("person 240"), 10);
        assert_eq!(beyond.len(), AUTHOR_FACET_MAX);
        assert!(beyond.contains(&("person 240".to_owned(), 10)));
        assert!(
            !beyond.contains(&("person 199".to_owned(), 51)),
            "the cap makes room for it"
        );

        let missing = many.capped(Some("nobody"), 0);
        assert!(missing.contains(&("nobody".to_owned(), 0)));
        assert_eq!(missing.len(), AUTHOR_FACET_MAX);

        let few = facet(&[(Some("Ada"), 3)]).capped(Some("nobody"), 0);
        assert_eq!(few, vec![("Ada".to_owned(), 3), ("nobody".to_owned(), 0)]);
    }

    #[test]
    fn merging_adds_the_counts() {
        let mut left = facet(&[(Some("Ada"), 1), (None, 1)]);
        left.merge(facet(&[(Some("Ada"), 2), (Some("Bob"), 1), (None, 3)]));

        assert_eq!(left.people(), vec![("Ada".to_owned(), 3), ("Bob".to_owned(), 1)]);
        assert_eq!(left.unknown(), 4);
    }
}
