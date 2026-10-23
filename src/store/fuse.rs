//! Hybrid search: the keyword list and the vector list of one query, fused into one.
//!
//! Reciprocal rank fusion: a note scores `1 / (K + rank)` in each list it is in (ranks count from
//! 1), so a note both lists find outranks a note one list finds, and neither list's own scale
//! matters. The keyword list is ordered by the keyword relevance alone, without the boosts the
//! keyword ranking applies (see [`Hit::boost_milli`]), and the boosts are applied once, to the
//! fused score. The score is scaled so a note first in both lists scores 1000 before the boosts.

use std::cmp::Reverse;
use std::collections::HashMap;

use super::Hit;

/// The rank constant of the fusion: how fast a note's share falls with its rank.
const K: f64 = 60.0;

/// What a note is worth in the lists it is in.
struct Entry {
    hit: Hit,
    keyword: Option<usize>,
    vector: Option<usize>,
}

/// The best `limit` notes of `keyword` (in the order the keyword search gave) and `vector` (the
/// closest first, only notes the query may see), each with its fused [`Hit::score_milli`]. A note
/// in both lists is shown as the keyword search found it (its snippet is cut around the match).
pub(crate) fn fuse(keyword: Vec<Hit>, vector: Vec<Hit>, limit: usize) -> Vec<Hit> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut at: HashMap<i64, usize> = HashMap::new();

    for (rank, hit) in by_relevance(keyword).into_iter().enumerate() {
        at.insert(hit.id, entries.len());
        entries.push(Entry {
            hit,
            keyword: Some(rank + 1),
            vector: None,
        });
    }

    for (rank, hit) in vector.into_iter().enumerate() {
        match at.get(&hit.id) {
            Some(index) => entries[*index].vector = Some(rank + 1),
            None => {
                at.insert(hit.id, entries.len());
                entries.push(Entry {
                    hit,
                    keyword: None,
                    vector: Some(rank + 1),
                });
            }
        }
    }

    let mut fused: Vec<(Hit, bool)> = entries
        .into_iter()
        .map(|entry| {
            let share = |rank: Option<usize>| rank.map_or(0.0, |rank| 1.0 / (K + rank as f64));
            let boost = f64::from(entry.hit.boost_milli) / 1000.0;
            let score = (share(entry.keyword) + share(entry.vector)) * (K + 1.0) / 2.0 * boost;

            let hit = Hit {
                score_milli: (score * 1000.0).round() as u32,
                ..entry.hit
            };

            (hit, entry.keyword.is_some())
        })
        .collect();

    fused.sort_by_key(|(hit, in_keyword)| {
        (
            Reverse(hit.score_milli),
            Reverse(*in_keyword),
            Reverse(hit.updated_at),
            Reverse(hit.id),
        )
    });
    fused.truncate(limit);

    fused.into_iter().map(|(hit, _)| hit).collect()
}

/// `hits` ordered by their keyword score without the boosts; hits of equal score keep their
/// order.
fn by_relevance(mut hits: Vec<Hit>) -> Vec<Hit> {
    let relevance = |hit: &Hit| f64::from(hit.score_milli) / f64::from(hit.boost_milli.max(1));

    hits.sort_by(|a, b| relevance(b).total_cmp(&relevance(a)));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NoteKind, Provenance, Scope};

    /// A keyword hit that scored `milli` after `boost_milli`.
    fn keyword(id: i64, milli: u32, boost_milli: u32) -> Hit {
        Hit {
            id,
            kind: NoteKind::Note,
            scope: Scope::Global,
            title: format!("note {id}"),
            snippet: "around the match".to_owned(),
            updated_at: 0,
            entities: Vec::new(),
            provenance: Provenance::default(),
            score_milli: milli,
            boost_milli,
        }
    }

    fn vector(id: i64, boost_milli: u32) -> Hit {
        Hit {
            snippet: "start of the body".to_owned(),
            score_milli: 0,
            ..keyword(id, 0, boost_milli)
        }
    }

    fn ids(hits: &[Hit]) -> Vec<i64> {
        hits.iter().map(|hit| hit.id).collect()
    }

    #[test]
    fn a_note_in_both_lists_outranks_notes_in_one() {
        let fused = fuse(
            vec![keyword(1, 900, 1000), keyword(2, 800, 1000)],
            vec![vector(3, 1000), vector(2, 1000)],
            10,
        );

        assert_eq!(ids(&fused), [2, 1, 3]);
        assert!(fused[0].score_milli > fused[1].score_milli);
    }

    #[test]
    fn a_note_only_the_vectors_find_surfaces() {
        let fused = fuse(Vec::new(), vec![vector(7, 1000)], 10);

        assert_eq!(ids(&fused), [7]);
        assert_eq!(fused[0].snippet, "start of the body");
        assert_eq!(fused[0].score_milli, 500);
    }

    #[test]
    fn a_note_first_in_both_lists_scores_a_thousand() {
        let fused = fuse(vec![keyword(1, 700, 1000)], vec![vector(1, 1000)], 10);

        assert_eq!(fused[0].score_milli, 1000);
        assert_eq!(fused[0].snippet, "around the match", "the keyword snippet is kept");
    }

    #[test]
    fn the_keyword_order_leaves_out_the_boosts_and_they_apply_once() {
        // Note 1 only leads the keyword list because it is in the repository: without the boost
        // it scores less than note 2.
        let keyword = vec![keyword(1, 750, 1250), keyword(2, 700, 1000)];
        let fused = fuse(keyword, Vec::new(), 10);

        // Note 2 is first by relevance (rank 1), note 1 second (rank 2), then the boost.
        let note_two: f64 = (1.0 / 61.0) * 61.0 / 2.0 * 1000.0;
        let note_one: f64 = (1.0 / 62.0) * 61.0 / 2.0 * 1000.0 * 1.25;

        assert_eq!(ids(&fused), [1, 2]);
        assert_eq!(fused[0].score_milli, note_one.round() as u32);
        assert_eq!(fused[1].score_milli, note_two.round() as u32);
    }

    #[test]
    fn the_limit_keeps_the_best() {
        let hits: Vec<Hit> = (1..=5).map(|id| vector(id, 1000)).collect();

        assert_eq!(ids(&fuse(Vec::new(), hits, 2)), [1, 2]);
    }

    #[test]
    fn equal_scores_prefer_the_keyword_note() {
        let fused = fuse(vec![keyword(1, 500, 1000)], vec![vector(2, 1000)], 10);

        assert_eq!(ids(&fused), [1, 2]);
    }
}
